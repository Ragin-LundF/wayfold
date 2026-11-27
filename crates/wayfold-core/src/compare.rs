//! Folder compare (F07): recursive diff of two roots by relative path, streamed in batches,
//! plus a copy plan built from per-row directions. Symlinks are compared as links (target
//! text), never followed. Missing metadata never counts as equal. Nothing here deletes;
//! applying a plan goes through `ops` transfers, which revalidate and resolve conflicts.

use crate::pool::{self, CancelToken};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const BATCH: usize = 256;
const HASH_CHUNK: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareMode {
    /// Same size and modification time (to the second).
    SizeTime,
    /// Same size and BLAKE3 content hash.
    Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    LeftOnly,
    RightOnly,
    Same,
    Different,
    Unreadable(String),
    ChangedDuringScan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Side {
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRow {
    /// Path relative to both roots. A one-sided folder is one row (its subtree is not listed).
    pub rel: PathBuf,
    pub left: Option<Side>,
    pub right: Option<Side>,
    pub status: DiffStatus,
}

#[derive(Clone, Debug)]
pub enum CompareEvent {
    Rows(Vec<DiffRow>),
    Done { cancelled: bool },
}

pub struct CompareHandle {
    pub events: async_channel::Receiver<CompareEvent>,
    pub cancel: CancelToken,
}

/// Run `compare_dirs` on the pool for `left`; results arrive as events.
pub fn start_compare(left: PathBuf, right: PathBuf, mode: CompareMode) -> CompareHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    pool::for_path(&left).execute(move || {
        compare_dirs(&left, &right, mode, &token, &mut |rows| {
            let _ = tx.send_blocking(CompareEvent::Rows(rows));
        });
        let _ = tx.send_blocking(CompareEvent::Done { cancelled: token.is_cancelled() });
    });
    CompareHandle { events: rx, cancel }
}

/// Blocking recursive compare; rows are handed to `emit` in batches.
pub fn compare_dirs(left: &Path, right: &Path, mode: CompareMode, cancel: &CancelToken, emit: &mut dyn FnMut(Vec<DiffRow>)) {
    let mut walk = Walk { left, right, mode, cancel, emit, batch: Vec::new() };
    walk.dir(Path::new(""));
    if !walk.batch.is_empty() {
        let rows = std::mem::take(&mut walk.batch);
        (walk.emit)(rows);
    }
}

struct Walk<'a> {
    left: &'a Path,
    right: &'a Path,
    mode: CompareMode,
    cancel: &'a CancelToken,
    emit: &'a mut dyn FnMut(Vec<DiffRow>),
    batch: Vec<DiffRow>,
}

impl Walk<'_> {
    fn push(&mut self, row: DiffRow) {
        self.batch.push(row);
        if self.batch.len() >= BATCH {
            (self.emit)(std::mem::take(&mut self.batch));
        }
    }

    fn dir(&mut self, rel: &Path) {
        let (l, r) = (names(&self.left.join(rel)), names(&self.right.join(rel)));
        let (l, r) = match (l, r) {
            (Ok(l), Ok(r)) => (l, r),
            (Err(e), _) | (_, Err(e)) => {
                return self.push(DiffRow {
                    rel: rel.to_path_buf(),
                    left: None,
                    right: None,
                    status: DiffStatus::Unreadable(e.to_string()),
                });
            }
        };
        let all: BTreeSet<&OsString> = l.iter().chain(r.iter()).collect();
        for name in all {
            if self.cancel.is_cancelled() {
                return;
            }
            let rel = rel.join(name);
            let lm = fs::symlink_metadata(self.left.join(&rel)).ok();
            let rm = fs::symlink_metadata(self.right.join(&rel)).ok();
            match (&lm, &rm) {
                (Some(a), Some(b)) if a.is_dir() && b.is_dir() => self.dir(&rel),
                _ => {
                    let status = match (&lm, &rm) {
                        (Some(_), None) => DiffStatus::LeftOnly,
                        (None, Some(_)) => DiffStatus::RightOnly,
                        (Some(_), Some(_)) => compare_pair(&self.left.join(&rel), &self.right.join(&rel), self.mode, self.cancel),
                        // Listed but gone or unreadable on both sides by now.
                        (None, None) => DiffStatus::ChangedDuringScan,
                    };
                    self.push(DiffRow { rel, left: lm.as_ref().map(side), right: rm.as_ref().map(side), status });
                }
            }
        }
    }
}

fn names(dir: &Path) -> io::Result<Vec<OsString>> {
    fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name())).collect()
}

fn side(md: &Metadata) -> Side {
    Side { is_dir: md.is_dir(), size: (!md.is_dir()).then_some(md.len()), modified: md.modified().ok() }
}

/// Compare two non-directory paths (or a file against a folder).
pub fn compare_pair(left: &Path, right: &Path, mode: CompareMode, cancel: &CancelToken) -> DiffStatus {
    compare_pair_with(left, right, mode, cancel, || {})
}

/// `between` runs after hashing, before the re-check; tests use it to change a file mid-scan.
pub(crate) fn compare_pair_with(left: &Path, right: &Path, mode: CompareMode, cancel: &CancelToken, between: impl FnOnce()) -> DiffStatus {
    let (a, b) = match (fs::symlink_metadata(left), fs::symlink_metadata(right)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return DiffStatus::Unreadable(e.to_string()),
    };
    let (ta, tb) = (a.file_type(), b.file_type());
    if ta.is_dir() != tb.is_dir() || ta.is_symlink() != tb.is_symlink() || ta.is_file() != tb.is_file() {
        return DiffStatus::Different;
    }
    if ta.is_symlink() {
        return match (fs::read_link(left), fs::read_link(right)) {
            (Ok(x), Ok(y)) if x == y => DiffStatus::Same,
            (Ok(_), Ok(_)) => DiffStatus::Different,
            (Err(e), _) | (_, Err(e)) => DiffStatus::Unreadable(e.to_string()),
        };
    }
    if !ta.is_file() {
        return DiffStatus::Unreadable("not a regular file".into());
    }
    if a.len() != b.len() {
        return DiffStatus::Different;
    }
    let status = match mode {
        CompareMode::SizeTime => match (secs(&a), secs(&b)) {
            (Some(x), Some(y)) if x == y => DiffStatus::Same,
            (Some(_), Some(_)) => DiffStatus::Different,
            _ => DiffStatus::Unreadable("modification time unavailable".into()),
        },
        CompareMode::Checksum => match (hash_file(left, cancel), hash_file(right, cancel)) {
            (Ok(x), Ok(y)) if x == y => DiffStatus::Same,
            (Ok(_), Ok(_)) => DiffStatus::Different,
            (Err(e), _) | (_, Err(e)) => DiffStatus::Unreadable(e.to_string()),
        },
    };
    between();
    let unchanged = |p: &Path, before: &Metadata| {
        fs::symlink_metadata(p).is_ok_and(|now| now.len() == before.len() && now.modified().ok() == before.modified().ok())
    };
    if unchanged(left, &a) && unchanged(right, &b) { status } else { DiffStatus::ChangedDuringScan }
}

fn secs(md: &Metadata) -> Option<u64> {
    md.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// Streamed BLAKE3 of a regular file; checks `cancel` between 1 MiB chunks.
pub fn hash_file(path: &Path, cancel: &CancelToken) -> io::Result<blake3::Hash> {
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&buf[..n]);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
    Skip,
}

impl DiffRow {
    /// Proposed direction: one-sided rows copy to the missing side; everything else waits
    /// for an explicit choice.
    pub fn default_direction(&self) -> Direction {
        match self.status {
            DiffStatus::LeftOnly => Direction::LeftToRight,
            DiffStatus::RightOnly => Direction::RightToLeft,
            _ => Direction::Skip,
        }
    }

    /// A direction needs an existing source side.
    pub fn allows(&self, d: Direction) -> bool {
        match d {
            Direction::LeftToRight => self.left.is_some() && self.status != DiffStatus::Same,
            Direction::RightToLeft => self.right.is_some() && self.status != DiffStatus::Same,
            Direction::Skip => true,
        }
    }
}

/// One transfer of the plan: copy `sources` into `dest_dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyStep {
    pub dest_dir: PathBuf,
    pub sources: Vec<PathBuf>,
}

/// Copy plan grouped by destination folder. Rows whose direction is not allowed are skipped.
/// Existing targets are not touched here: the transfer asks (keep both / replace to trash / skip).
pub fn copy_plan(left: &Path, right: &Path, rows: &[(DiffRow, Direction)]) -> Vec<CopyStep> {
    let mut by_dest: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for (row, d) in rows {
        if *d == Direction::Skip || !row.allows(*d) {
            continue;
        }
        let (from, to) = if *d == Direction::LeftToRight { (left, right) } else { (right, left) };
        let parent = row.rel.parent().unwrap_or(Path::new(""));
        by_dest.entry(to.join(parent)).or_default().push(from.join(&row.rel));
    }
    by_dest.into_iter().map(|(dest_dir, sources)| CopyStep { dest_dir, sources }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::Duration;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn set_mtime(p: &Path, secs: u64) {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    fn run(l: &Path, r: &Path, mode: CompareMode) -> BTreeMap<String, DiffStatus> {
        let mut out = BTreeMap::new();
        compare_dirs(l, r, mode, &CancelToken::new(), &mut |rows| {
            for row in rows {
                out.insert(row.rel.to_string_lossy().into_owned(), row.status);
            }
        });
        out
    }

    /// left: same.txt, mod.txt, only_l.txt, sub/deep.txt, onlydir/x ; right: same.txt, mod.txt, only_r.txt, sub/deep.txt
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (l, r) = (t.path().join("l"), t.path().join("r"));
        for root in [&l, &r] {
            write(&root.join("same.txt"), "same");
            write(&root.join("sub/deep.txt"), "deep");
            set_mtime(&root.join("same.txt"), 1_000_000);
            set_mtime(&root.join("sub/deep.txt"), 1_000_000);
        }
        write(&l.join("mod.txt"), "aaaa");
        write(&r.join("mod.txt"), "bbbb");
        set_mtime(&l.join("mod.txt"), 1_000_000);
        set_mtime(&r.join("mod.txt"), 1_000_000);
        write(&l.join("only_l.txt"), "l");
        write(&l.join("onlydir/x"), "x");
        write(&r.join("only_r.txt"), "r");
        (t, l, r)
    }

    #[test]
    fn f07_size_time_and_checksum_statuses() {
        let (_t, l, r) = fixture();
        let st = run(&l, &r, CompareMode::SizeTime);
        assert_eq!(st["same.txt"], DiffStatus::Same);
        assert_eq!(st["sub/deep.txt"], DiffStatus::Same);
        assert_eq!(st["only_l.txt"], DiffStatus::LeftOnly);
        assert_eq!(st["onlydir"], DiffStatus::LeftOnly);
        assert!(!st.contains_key("onlydir/x"), "one-sided folder is a single row");
        assert_eq!(st["only_r.txt"], DiffStatus::RightOnly);
        // Same size and time but other bytes: size/time cannot tell, checksum can.
        assert_eq!(st["mod.txt"], DiffStatus::Same);
        let cs = run(&l, &r, CompareMode::Checksum);
        assert_eq!(cs["mod.txt"], DiffStatus::Different);
        assert_eq!(cs["same.txt"], DiffStatus::Same);
    }

    #[test]
    fn f07_time_difference_and_kind_mismatch_are_different() {
        let (_t, l, r) = fixture();
        set_mtime(&r.join("same.txt"), 2_000_000);
        fs::remove_file(r.join("only_r.txt")).unwrap();
        fs::create_dir(r.join("only_r.txt")).unwrap();
        write(&l.join("only_r.txt"), "file on the left");
        let st = run(&l, &r, CompareMode::SizeTime);
        assert_eq!(st["same.txt"], DiffStatus::Different);
        assert_eq!(st["only_r.txt"], DiffStatus::Different);
    }

    #[test]
    fn f07_unreadable_file_and_folder() {
        let (_t, l, r) = fixture();
        let locked = r.join("sub");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(r.join("same.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let cs = run(&l, &r, CompareMode::Checksum);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(r.join("same.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(cs["sub"], DiffStatus::Unreadable(_)), "{:?}", cs["sub"]);
        assert!(matches!(cs["same.txt"], DiffStatus::Unreadable(_)));
    }

    #[test]
    fn f07_symlinks_compared_as_links_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let (l, r) = (t.path().join("l"), t.path().join("r"));
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&r).unwrap();
        write(&t.path().join("big/inner.txt"), "x");
        symlink(t.path().join("big"), l.join("link")).unwrap();
        symlink(t.path().join("other"), r.join("link")).unwrap();
        symlink("same-target", l.join("s")).unwrap();
        symlink("same-target", r.join("s")).unwrap();
        let st = run(&l, &r, CompareMode::Checksum);
        assert_eq!(st["link"], DiffStatus::Different);
        assert_eq!(st["s"], DiffStatus::Same);
        assert!(!st.keys().any(|k| k.contains("inner")));
    }

    #[test]
    fn f07_changed_during_scan() {
        let (_t, l, r) = fixture();
        let target = r.join("same.txt");
        let status = compare_pair_with(&l.join("same.txt"), &target, CompareMode::Checksum, &CancelToken::new(), || {
            fs::write(&target, "grown while hashing").unwrap();
        });
        assert_eq!(status, DiffStatus::ChangedDuringScan);
    }

    #[test]
    fn f07_cancel_stops_scan() {
        let (_t, l, r) = fixture();
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut rows = 0;
        compare_dirs(&l, &r, CompareMode::Checksum, &cancel, &mut |b| rows += b.len());
        assert_eq!(rows, 0);
        assert_eq!(hash_file(&l.join("same.txt"), &cancel).unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn f07_copy_plan_groups_by_destination_and_skips_invalid() {
        let (_t, l, r) = fixture();
        let mut rows = Vec::new();
        compare_dirs(&l, &r, CompareMode::Checksum, &CancelToken::new(), &mut |b| rows.extend(b));
        let pick = |rel: &str, d: Direction| (rows.iter().find(|x| x.rel == Path::new(rel)).unwrap().clone(), d);
        let plan = copy_plan(
            &l,
            &r,
            &[
                pick("only_l.txt", Direction::LeftToRight),
                pick("onlydir", Direction::LeftToRight),
                pick("mod.txt", Direction::RightToLeft),
                pick("only_r.txt", Direction::LeftToRight), // no left side: ignored
                pick("same.txt", Direction::LeftToRight),   // equal: ignored
                pick("sub/deep.txt", Direction::Skip),
            ],
        );
        assert_eq!(
            plan,
            vec![
                CopyStep { dest_dir: l.clone(), sources: vec![r.join("mod.txt")] },
                CopyStep { dest_dir: r.clone(), sources: vec![l.join("only_l.txt"), l.join("onlydir")] },
            ]
        );
    }
}
