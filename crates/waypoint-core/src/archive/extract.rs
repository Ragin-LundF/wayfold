//! Safe, staged, all-or-nothing extraction.

use super::read::{self, rar_err, rar_item, rar_open, Item};
use super::{publish, temp_path, ArchiveError, ArchiveFormat, EntryKind, Limits, Progress, ProgressFn, RATIO_FLOOR};
use crate::pool::CancelToken;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const CHUNK: usize = 64 << 10;
const PROGRESS_BYTES: u64 = 4 << 20;

/// Extract `archive` into `dest_dir`, published as `name` (made unique). Nothing is left
/// behind on error or cancellation.
pub fn extract_archive(
    archive: &Path,
    dest_dir: &Path,
    name: &OsStr,
    password: Option<&str>,
    limits: Limits,
    cancel: &CancelToken,
    progress: ProgressFn,
) -> Result<PathBuf, ArchiveError> {
    let format = ArchiveFormat::detect(archive).ok_or_else(|| ArchiveError::Unsupported("unknown archive type".into()))?;
    crate::ops::validate_name(name).map_err(|_| ArchiveError::Io("invalid destination name".into()))?;
    let packed = fs::metadata(archive)?.len().max(1);
    let staging = temp_path(dest_dir);
    fs::create_dir(&staging)?;
    let mut stager = Stager { root: &staging, limits, packed, cancel, progress, total: 0, entries: 0, last_report: 0, links: Vec::new() };
    let result = stager.run(archive, format, password).and_then(|()| {
        if format.is_single_stream() {
            let staged = staging.join(super::output_name(archive));
            let published = publish(&staged, dest_dir, name)?;
            let _ = fs::remove_dir(&staging);
            Ok(published)
        } else {
            publish(&staging, dest_dir, name)
        }
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

struct Stager<'a> {
    root: &'a Path,
    limits: Limits,
    packed: u64,
    cancel: &'a CancelToken,
    progress: ProgressFn<'a>,
    total: u64,
    entries: u64,
    last_report: u64,
    /// Symlinks are created last, so no file is ever written through a link.
    links: Vec<(PathBuf, String, String)>,
}

impl Stager<'_> {
    fn run(&mut self, archive: &Path, format: ArchiveFormat, password: Option<&str>) -> Result<(), ArchiveError> {
        if format == ArchiveFormat::Rar {
            self.run_rar(archive, password)?;
        } else {
            read::visit(archive, format, password, &mut |item, r| self.entry(item, r).map(|()| true))?;
        }
        self.create_links()
    }

    fn report(&mut self, current: &str, force: bool) {
        if force || self.total - self.last_report >= PROGRESS_BYTES {
            self.last_report = self.total;
            (self.progress)(Progress { bytes: self.total, entries: self.entries, current: current.to_string() });
        }
    }

    /// Checks every entry passes before anything is written for it.
    fn admit(&mut self, item: &Item) -> Result<PathBuf, ArchiveError> {
        if self.cancel.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(ArchiveError::LimitExceeded(format!("more than {} entries", self.limits.max_entries)));
        }
        let rel = safe_relative(&item.name)?;
        if item.kind == EntryKind::Special {
            return Err(unsafe_entry(item, "special file (device, FIFO or socket)"));
        }
        if let (Some(size), Some(packed)) = (item.size, item.compressed)
            && size > RATIO_FLOOR
            && size / packed.max(1) > self.limits.max_ratio
        {
            return Err(ArchiveError::LimitExceeded(format!("\"{}\" expands more than {}x", item.name, self.limits.max_ratio)));
        }
        if let Some(size) = item.size
            && self.total.saturating_add(size) > self.limits.max_total_bytes
        {
            return Err(self.total_exceeded());
        }
        self.report(&item.name, true);
        Ok(self.root.join(rel))
    }

    fn total_exceeded(&self) -> ArchiveError {
        ArchiveError::LimitExceeded(format!("more than {} expanded bytes", self.limits.max_total_bytes))
    }

    fn add_bytes(&mut self, n: u64, written: u64, item: &Item) -> Result<(), ArchiveError> {
        self.total += n;
        if self.total > self.limits.max_total_bytes {
            return Err(self.total_exceeded());
        }
        if self.total > RATIO_FLOOR && self.total / self.packed > self.limits.max_ratio {
            return Err(ArchiveError::LimitExceeded(format!("archive expands more than {}x", self.limits.max_ratio)));
        }
        if item.size.is_some_and(|s| written > s) {
            return Err(ArchiveError::Corrupt(format!("\"{}\" is larger than recorded", item.name)));
        }
        Ok(())
    }

    fn entry(&mut self, item: &Item, data: &mut dyn Read) -> Result<(), ArchiveError> {
        let target = self.admit(item)?;
        match item.kind {
            EntryKind::Dir => fs::create_dir_all(&target)?,
            EntryKind::File => self.write_file(&target, item, data)?,
            EntryKind::Symlink => {
                let link = item.link.clone().ok_or_else(|| ArchiveError::Corrupt("link without target".into()))?;
                self.links.push((target, link, item.name.clone()));
            }
            EntryKind::HardLink => {
                let source = self.root.join(safe_relative(item.link.as_deref().unwrap_or_default())?);
                if !fs::symlink_metadata(&source).is_ok_and(|m| m.is_file()) {
                    return Err(unsafe_entry(item, "hard link to a missing or non-file entry"));
                }
                ensure_parent(&target)?;
                let n = fs::copy(&source, &target)?;
                self.add_bytes(n, 0, item)?;
            }
            EntryKind::Special => unreachable!("rejected in admit"),
        }
        Ok(())
    }

    fn write_file(&mut self, target: &Path, item: &Item, data: &mut dyn Read) -> Result<(), ArchiveError> {
        ensure_parent(target)?;
        let mode = item.mode.map_or(0o644, |m| (m & 0o777) | 0o600);
        let mut out = OpenOptions::new().write(true).create(true).truncate(true).mode(mode).custom_flags(libc::O_NOFOLLOW).open(target)?;
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;
        loop {
            if self.cancel.is_cancelled() {
                return Err(ArchiveError::Cancelled);
            }
            let n = data.read(&mut buf).map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => ArchiveError::Corrupt(e.to_string()),
                _ => e.into(),
            })?;
            if n == 0 {
                return Ok(());
            }
            written += n as u64;
            self.add_bytes(n as u64, written, item)?;
            out.write_all(&buf[..n])?;
            self.report(&item.name, false);
        }
    }

    fn run_rar(&mut self, archive: &Path, password: Option<&str>) -> Result<(), ArchiveError> {
        let mut open = rar_open(archive, password).open_for_processing().map_err(rar_err)?;
        while let Some(h) = open.read_header().map_err(rar_err)? {
            let item = rar_item(h.entry());
            let target = self.admit(&item)?;
            open = match item.kind {
                EntryKind::Dir => {
                    fs::create_dir_all(&target)?;
                    h.skip().map_err(rar_err)?
                }
                EntryKind::File => {
                    ensure_parent(&target)?;
                    let next = h.extract_to(&target).map_err(rar_err)?;
                    let md = fs::symlink_metadata(&target)?;
                    // unrar applies the stored mode; drop setuid/setgid/sticky bits.
                    fs::set_permissions(&target, fs::Permissions::from_mode((md.mode() & 0o777) | 0o600))?;
                    let n = md.len();
                    self.add_bytes(n, n, &item)?;
                    next
                }
                // unrar cannot hand out the link target without writing the link itself.
                _ => return Err(unsafe_entry(&item, "links in RAR archives are not supported")),
            };
        }
        Ok(())
    }

    /// Create deferred links. A target must be relative, stay inside the root and must not
    /// pass through another link of this archive (whose real location could differ).
    fn create_links(&mut self) -> Result<(), ArchiveError> {
        let link_paths: HashSet<PathBuf> = self.links.iter().map(|(p, ..)| p.clone()).collect();
        for (path, target, name) in &self.links {
            let parent = path.parent().unwrap_or(self.root);
            if parent.ancestors().take_while(|a| *a != self.root).any(|a| link_paths.contains(a)) {
                return Err(ArchiveError::Unsafe { entry: name.clone(), reason: "link passes through another link" });
            }
            let mut resolved = parent.strip_prefix(self.root).unwrap_or(Path::new("")).to_path_buf();
            let comps: Vec<Component> = Path::new(target).components().collect();
            for (i, c) in comps.iter().enumerate() {
                let escape = || ArchiveError::Unsafe { entry: name.clone(), reason: "link points outside the destination" };
                match c {
                    Component::RootDir | Component::Prefix(_) => return Err(escape()),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if !resolved.pop() {
                            return Err(escape());
                        }
                    }
                    Component::Normal(n) => {
                        resolved.push(n);
                        if i + 1 < comps.len() && link_paths.contains(&self.root.join(&resolved)) {
                            return Err(ArchiveError::Unsafe { entry: name.clone(), reason: "link passes through another link" });
                        }
                    }
                }
            }
            if self.cancel.is_cancelled() {
                return Err(ArchiveError::Cancelled);
            }
            ensure_parent(path)?;
            symlink(target, path)?;
        }
        Ok(())
    }
}

fn unsafe_entry(item: &Item, reason: &'static str) -> ArchiveError {
    ArchiveError::Unsafe { entry: item.name.clone(), reason }
}

fn ensure_parent(p: &Path) -> std::io::Result<()> {
    p.parent().map_or(Ok(()), fs::create_dir_all)
}

/// Archive name → relative path inside the destination. Rejects absolute names, `..`
/// and NUL; drops `.` and empty components.
fn safe_relative(name: &str) -> Result<PathBuf, ArchiveError> {
    let unsafe_name = |reason| ArchiveError::Unsafe { entry: name.to_string(), reason };
    if name.starts_with('/') {
        return Err(unsafe_name("absolute path"));
    }
    if name.contains('\0') {
        return Err(unsafe_name("invalid name"));
    }
    let mut rel = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_name("path leaves the destination")),
            p => rel.push(p),
        }
    }
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f08_safe_relative_rules() {
        assert_eq!(safe_relative("a/./b//c").unwrap(), PathBuf::from("a/b/c"));
        assert_eq!(safe_relative("./").unwrap(), PathBuf::new());
        for bad in ["/etc/passwd", "../evil", "a/../../evil", "a/..", "x\0y"] {
            assert!(matches!(safe_relative(bad), Err(ArchiveError::Unsafe { .. })), "{bad}");
        }
    }
}
