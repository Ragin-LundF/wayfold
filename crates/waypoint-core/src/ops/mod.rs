//! Mutating file operations: create/rename/trash (F02), transfers (F03), batch rename (F04)
//! and undo. Every mutation writes a journal intent first (see `journal`), never overwrites
//! silently (`RENAME_EXCL`) and never deletes permanently (replace goes to the trash).
//!
//! All functions here block on I/O: call them from `pool::for_path(..)`, never the UI thread.

mod batch_rename;
mod fileops;
pub mod journal;
mod transfer;
mod trash;
mod undo;

pub use batch_rename::{
    apply_batch_rename, plan_batch_rename, CaseRule, RenameProblem, RenameRow, RenameRules, Sequence,
};
pub use fileops::{create_file, create_folder, create_symlink, duplicate, rename, restore, trash};
pub use transfer::{
    start_transfer, ConflictAction, ConflictResolution, TransferEvent, TransferHandle, TransferMode,
    TransferRequest, TransferState,
};
pub use undo::{undo, RenamedItem, TrashedItem, UndoAction};

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Result for one item of a multi-item operation; failures stay visible per item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemResult {
    pub path: PathBuf,
    pub outcome: ItemOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemOutcome {
    /// Finished; carries the resulting path.
    Done(PathBuf),
    Skipped(String),
    Failed(String),
}

impl ItemResult {
    fn new(path: &Path, outcome: ItemOutcome) -> Self {
        Self { path: path.to_path_buf(), outcome }
    }
    fn failed(path: &Path, reason: impl ToString) -> Self {
        Self::new(path, ItemOutcome::Failed(reason.to_string()))
    }
}

/// Device + inode of a path (not following a final symlink): proves "still the same file"
/// before undo, restore or source removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

pub fn identity(path: &Path) -> io::Result<FileIdentity> {
    let md = std::fs::symlink_metadata(path)?;
    Ok(FileIdentity { dev: md.dev(), ino: md.ino() })
}

/// Why a file name cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    Empty,
    /// Contains `/` or NUL, is `.`/`..`, exceeds 255 bytes or is not valid UTF-8 where text is required.
    Invalid,
}

pub fn validate_name(name: &OsStr) -> Result<(), NameProblem> {
    let b = name.as_bytes();
    if b.is_empty() {
        return Err(NameProblem::Empty);
    }
    if b == b"." || b == b".." || b.len() > 255 || b.iter().any(|&c| c == b'/' || c == 0) {
        return Err(NameProblem::Invalid);
    }
    Ok(())
}

/// Whether names in `dir` are case-sensitive. Unknown counts as insensitive, the safer
/// assumption for collision checks.
pub fn is_case_sensitive(dir: &Path) -> bool {
    let Ok(c) = cstr(dir) else { return false };
    // SAFETY: valid NUL-terminated path; pathconf only reads it.
    unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) == 1 }
}

/// Free name for "keep both": `name 2.ext`, `name 3.ext`, ...
pub fn unique_name(dir: &Path, name: &OsStr) -> PathBuf {
    first_free(dir, name, 2, |stem, n| [stem, format!(" {n}").as_bytes()].concat())
}

/// Free name for duplicates: `name copy.ext`, `name copy 2.ext`, ...
pub fn copy_name(dir: &Path, name: &OsStr) -> PathBuf {
    first_free(dir, name, 1, |stem, n| match n {
        1 => [stem, b" copy"].concat(),
        n => [stem, format!(" copy {n}").as_bytes()].concat(),
    })
}

fn first_free(dir: &Path, name: &OsStr, first: u32, stem_for: impl Fn(&[u8], u32) -> Vec<u8>) -> PathBuf {
    let b = name.as_bytes();
    // A leading dot is part of the stem (".zshrc" has no extension).
    let split = b.iter().rposition(|&c| c == b'.').filter(|&i| i > 0).unwrap_or(b.len());
    let (stem, ext) = b.split_at(split);
    (first..)
        .map(|n| dir.join(OsString::from_vec([stem_for(stem, n).as_slice(), ext].concat())))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("unbounded search finds a free name")
}

/// `rename` that fails with `AlreadyExists` instead of replacing an existing target.
pub(crate) fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

pub(crate) fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Unique id for tasks and journal records, distinct across runs.
pub(crate) fn next_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("{ms:x}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Shared test setup: journal and fake trash go to one temp data dir for the whole test run.
#[cfg(test)]
pub(crate) fn test_env() {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        // SAFETY: set once before any ops test reads it; no other test code touches this variable.
        unsafe { std::env::set_var("WAYPOINT_DATA_DIR", d.path()) };
        d
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn f03_keep_both_and_f02_duplicate_names() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a.txt"), b"").unwrap();
        assert_eq!(unique_name(t.path(), OsStr::new("a.txt")), t.path().join("a 2.txt"));
        fs::write(t.path().join("a 2.txt"), b"").unwrap();
        assert_eq!(unique_name(t.path(), OsStr::new("a.txt")), t.path().join("a 3.txt"));
        assert_eq!(copy_name(t.path(), OsStr::new("a.txt")), t.path().join("a copy.txt"));
        fs::write(t.path().join("a copy.txt"), b"").unwrap();
        assert_eq!(copy_name(t.path(), OsStr::new("a.txt")), t.path().join("a copy 2.txt"));
        assert_eq!(copy_name(t.path(), OsStr::new(".env")), t.path().join(".env copy"));
    }

    #[test]
    fn f04_invalid_names() {
        assert_eq!(validate_name(OsStr::new("")), Err(NameProblem::Empty));
        for bad in ["a/b", ".", "..", "a\0b"] {
            assert_eq!(validate_name(OsStr::new(bad)), Err(NameProblem::Invalid), "{bad}");
        }
        assert_eq!(validate_name(OsStr::new(&"x".repeat(256))), Err(NameProblem::Invalid));
        assert!(validate_name(OsStr::new("ok.txt")).is_ok());
    }

    #[test]
    fn rename_excl_never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert_eq!(rename_excl(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"b");
    }
}
