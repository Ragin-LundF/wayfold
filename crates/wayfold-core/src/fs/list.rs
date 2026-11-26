use super::{Entry, Kind, Visibility};
use crate::pool::CancelToken;
use std::fs;
use std::io;
use std::os::macos::fs::MetadataExt;
use std::path::Path;

/// Entries per progressive batch sent to the UI.
pub const BATCH: usize = 256;

const UF_HIDDEN: u32 = 0x8000;

/// Stat one path without following a final symlink. Never fails: errors go into `Entry::error`.
pub fn stat_entry(path: &Path) -> Entry {
    let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| path.as_os_str().to_os_string());
    let dot = name.as_encoded_bytes().first() == Some(&b'.');
    match fs::symlink_metadata(path) {
        Ok(md) => {
            let ft = md.file_type();
            let kind = if ft.is_symlink() {
                match fs::metadata(path) {
                    Ok(t) => Kind::Symlink { to_dir: t.is_dir(), broken: false },
                    Err(_) => Kind::Symlink { to_dir: false, broken: true },
                }
            } else if ft.is_dir() {
                Kind::Dir
            } else if ft.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            let visibility = if dot {
                Visibility::Hidden
            } else if md.st_flags() & UF_HIDDEN != 0 {
                Visibility::System
            } else {
                Visibility::Normal
            };
            Entry {
                path: path.to_path_buf(),
                name,
                kind,
                size: (!ft.is_dir()).then_some(md.len()),
                modified: md.modified().ok(),
                visibility,
                error: None,
                tags: super::tags::read(path),
            }
        }
        Err(e) => Entry {
            path: path.to_path_buf(),
            name,
            kind: Kind::Other,
            size: None,
            modified: None,
            visibility: if dot { Visibility::Hidden } else { Visibility::Normal },
            error: Some(e.to_string()),
            tags: vec![],
        },
    }
}

/// List `dir` progressively. `on_batch` receives up to `BATCH` entries at a time; it returns
/// `false` to stop early. Cancellation is checked between batches.
pub fn list_dir(
    dir: &Path,
    cancel: &CancelToken,
    mut on_batch: impl FnMut(Vec<Entry>) -> bool,
) -> io::Result<()> {
    let mut batch = Vec::with_capacity(BATCH);
    for item in fs::read_dir(dir)? {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let path = match item {
            Ok(d) => d.path(),
            Err(e) => {
                tracing::debug!("readdir entry error in {}: {e}", dir.display());
                continue;
            }
        };
        batch.push(stat_entry(&path));
        if batch.len() == BATCH && !on_batch(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))) {
            return Ok(());
        }
    }
    if !batch.is_empty() && !cancel.is_cancelled() {
        on_batch(batch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_in_batches_and_classifies() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..600 {
            fs::write(t.path().join(format!("f{i}")), b"x").unwrap();
        }
        fs::write(t.path().join(".dot"), b"").unwrap();
        fs::create_dir(t.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(t.path().join("sub"), t.path().join("link")).unwrap();
        std::os::unix::fs::symlink(t.path().join("missing"), t.path().join("broken")).unwrap();

        let mut sizes = vec![];
        let mut all = vec![];
        list_dir(t.path(), &CancelToken::new(), |b| {
            sizes.push(b.len());
            all.extend(b);
            true
        })
        .unwrap();
        assert_eq!(all.len(), 604);
        assert!(sizes.iter().all(|&s| s <= BATCH));
        let find = |n: &str| all.iter().find(|e| e.name == n).unwrap();
        assert_eq!(find(".dot").visibility, Visibility::Hidden);
        assert_eq!(find("sub").kind, Kind::Dir);
        assert!(find("link").is_dir());
        assert_eq!(find("broken").kind, Kind::Symlink { to_dir: false, broken: true });
        assert_eq!(find("f1").size, Some(1));
    }

    #[test]
    fn cancel_stops_listing() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..1000 {
            fs::write(t.path().join(format!("f{i}")), b"").unwrap();
        }
        let c = CancelToken::new();
        let mut n = 0;
        list_dir(t.path(), &c, |b| {
            n += b.len();
            c.cancel();
            true
        })
        .unwrap();
        assert_eq!(n, BATCH);
    }

    #[test]
    fn system_flag_is_classified() {
        // ~/Library carries UF_HIDDEN on macOS.
        let lib = std::env::var_os("HOME").map(|h| Path::new(&h).join("Library"));
        if let Some(lib) = lib.filter(|p| p.exists()) {
            assert_eq!(stat_entry(&lib).visibility, Visibility::System);
        }
    }
}
