//! Move to Trash through NSFileManager, which returns where the item landed (needed to restore).

use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(test))]
pub(crate) fn move_to_trash(path: &Path) -> io::Result<PathBuf> {
    ns_trash(path)
}

/// Tests never touch the user's Trash: items go to a folder in the test data dir instead.
#[cfg(test)]
pub(crate) fn move_to_trash(path: &Path) -> io::Result<PathBuf> {
    let dir = crate::storage::data_dir().join("test-trash");
    std::fs::create_dir_all(&dir)?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let to = super::unique_name(&dir, name);
    super::rename_excl(path, &to)?;
    Ok(to)
}

pub(crate) fn ns_trash(path: &Path) -> io::Result<PathBuf> {
    use objc2_foundation::{NSFileManager, NSURL};
    let url = NSURL::from_file_path(path).ok_or(io::ErrorKind::InvalidInput)?;
    let mut out = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut out))
        .map_err(|e| io::Error::other(e.localizedDescription().to_string()))?;
    out.and_then(|u| u.to_file_path()).ok_or_else(|| io::Error::other("trash did not report the new location"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moves a synthetic file into the real user Trash; run manually with `--ignored`.
    #[test]
    #[ignore]
    fn f02_real_trash_returns_location() {
        let t = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let f = t.path().join("wayfold-trash-test.txt");
        std::fs::write(&f, b"x").unwrap();
        let trashed = ns_trash(&f).unwrap();
        assert!(!f.exists());
        assert_eq!(std::fs::read(&trashed).unwrap(), b"x");
        std::fs::remove_file(trashed).unwrap();
    }
}
