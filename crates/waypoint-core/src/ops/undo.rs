//! Undo for reversible operations only (rename, batch rename, create, trash, same-volume move).
//! Each step checks that the file is still the one the operation touched (device + inode)
//! and reports a conflict instead of overwriting anything.

use super::fileops::{rename_set, restore};
use super::journal::{journaled, JournalOp};
use super::trash::move_to_trash;
use super::{identity, FileIdentity, ItemOutcome, ItemResult};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenamedItem {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Identity of the file being renamed (same before and after a same-volume rename).
    pub identity: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashedItem {
    pub original: PathBuf,
    pub trashed: PathBuf,
    pub identity: FileIdentity,
}

/// Returned by reversible operations; the UI keeps these on its undo stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoAction {
    /// Rename, batch rename or same-volume move.
    Renamed(Vec<RenamedItem>),
    Created { path: PathBuf, identity: FileIdentity },
    Trashed(Vec<TrashedItem>),
}

pub fn undo(action: &UndoAction) -> Vec<ItemResult> {
    match action {
        UndoAction::Renamed(items) => {
            let back: Vec<_> = items
                .iter()
                .map(|i| RenamedItem { from: i.to.clone(), to: i.from.clone(), identity: i.identity })
                .collect();
            rename_set(&back).0
        }
        UndoAction::Trashed(items) => items.iter().map(restore).collect(),
        UndoAction::Created { path, identity } => vec![undo_create(path, *identity)],
    }
}

/// Undoing a create moves the item to the trash, and only while it is still empty.
fn undo_create(path: &Path, id: FileIdentity) -> ItemResult {
    if identity(path).ok() != Some(id) {
        return ItemResult::failed(path, "the item was changed or removed since it was created");
    }
    let has_content = match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(true),
        Ok(md) => md.is_file() && md.len() > 0,
        Err(_) => true,
    };
    if has_content {
        return ItemResult::failed(path, "the item has content now and was not removed");
    }
    match journaled(JournalOp::Trash, path, path, || move_to_trash(path)) {
        Ok(trashed) => ItemResult::new(path, ItemOutcome::Done(trashed)),
        Err(e) => ItemResult::failed(path, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{create_folder, rename, test_env, trash};
    use std::ffi::OsStr;

    #[test]
    fn f02_undo_rename_create_and_trash() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.txt");
        fs::write(&a, b"a").unwrap();

        let u = rename(&a, OsStr::new("b.txt")).unwrap();
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Done(_)));
        assert_eq!(fs::read(&a).unwrap(), b"a");

        let (folder, u) = create_folder(t.path(), OsStr::new("new")).unwrap();
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Done(_)));
        assert!(!folder.exists());

        let (r, u) = trash(std::slice::from_ref(&a));
        assert!(matches!(r[0].outcome, ItemOutcome::Done(_)) && !a.exists());
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Done(_)));
        assert_eq!(fs::read(&a).unwrap(), b"a");
    }

    #[test]
    fn f02_undo_reports_conflicts_instead_of_overwriting() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        fs::write(&a, b"a").unwrap();
        let u = rename(&a, OsStr::new("b")).unwrap();
        fs::write(&a, b"new a").unwrap(); // original name taken again
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Failed(_)));
        assert_eq!(fs::read(&a).unwrap(), b"new a");
        assert_eq!(fs::read(t.path().join("b")).unwrap(), b"a");

        // Identity check: a different file at the renamed path is not moved back.
        fs::remove_file(&a).unwrap();
        fs::remove_file(t.path().join("b")).unwrap();
        fs::write(t.path().join("b"), b"other").unwrap();
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Failed(_)));
        assert!(!a.exists());

        // A created folder that gained content stays.
        let (folder, u) = create_folder(t.path(), OsStr::new("f")).unwrap();
        fs::write(folder.join("x"), b"").unwrap();
        assert!(matches!(undo(&u)[0].outcome, ItemOutcome::Failed(_)));
        assert!(folder.join("x").exists());
    }
}
