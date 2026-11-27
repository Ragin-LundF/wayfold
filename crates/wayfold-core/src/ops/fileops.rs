//! Single file operations (F02): create, rename, duplicate, trash/restore, symlink.

use super::journal::{self, HOP_PREFIX, JournalOp, journaled};
use super::trash::move_to_trash;
use super::{
    FileIdentity, ItemOutcome, ItemResult, RenamedItem, TransferHandle, TransferMode, TransferRequest, TrashedItem, UndoAction, identity,
    next_id, rename_excl, start_transfer, validate_name,
};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub fn create_folder(parent: &Path, name: &OsStr) -> io::Result<(PathBuf, UndoAction)> {
    let path = child(parent, name)?;
    journaled(JournalOp::CreateFolder, &path, &path, || fs::create_dir(&path))?;
    Ok((path.clone(), created(&path)?))
}

/// Creates an empty file; fails if the name exists.
pub fn create_file(parent: &Path, name: &OsStr) -> io::Result<(PathBuf, UndoAction)> {
    let path = child(parent, name)?;
    journaled(JournalOp::CreateFile, &path, &path, || OpenOptions::new().write(true).create_new(true).open(&path).map(drop))?;
    Ok((path.clone(), created(&path)?))
}

/// Creates `link` pointing at `target` (stored as given, relative or absolute).
pub fn create_symlink(target: &Path, link: &Path) -> io::Result<UndoAction> {
    validate_name(link.file_name().unwrap_or_default()).map_err(|_| io::ErrorKind::InvalidInput)?;
    journaled(JournalOp::Symlink, target, link, || std::os::unix::fs::symlink(target, link))?;
    created(link)
}

/// Renames within the same folder. Case-only renames work on case-insensitive volumes.
pub fn rename(path: &Path, new_name: &OsStr) -> io::Result<UndoAction> {
    let to = child(path.parent().unwrap_or(Path::new("/")), new_name)?;
    if to == path {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "name unchanged"));
    }
    let item = RenamedItem { from: path.to_path_buf(), to, identity: identity(path)? };
    let (results, renamed) = rename_set(&[item]);
    match &results[0].outcome {
        ItemOutcome::Done(_) => Ok(UndoAction::Renamed(renamed)),
        ItemOutcome::Failed(why) | ItemOutcome::Skipped(why) => Err(io::Error::other(why.clone())),
    }
}

/// Duplicates next to the original ("name copy", "name copy 2"); folders copy recursively.
pub fn duplicate(path: &Path) -> TransferHandle {
    let dest_dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    start_transfer(TransferRequest { mode: TransferMode::Copy, sources: vec![path.to_path_buf()], dest_dir })
}

/// The undo action covers only the items that reached the Trash; `None` when none did.
pub fn trash(paths: &[PathBuf]) -> (Vec<ItemResult>, Option<UndoAction>) {
    let mut trashed = Vec::new();
    let results = paths
        .iter()
        .map(|p| {
            let r = identity(p).and_then(|id| {
                let to = journaled(JournalOp::Trash, p, p, || move_to_trash(p))?;
                Ok(TrashedItem { original: p.clone(), trashed: to, identity: id })
            });
            match r {
                Ok(item) => {
                    let res = ItemResult::new(p, ItemOutcome::Done(item.trashed.clone()));
                    trashed.push(item);
                    res
                }
                Err(e) => ItemResult::failed(p, e),
            }
        })
        .collect();
    let undo = (!trashed.is_empty()).then_some(UndoAction::Trashed(trashed));
    (results, undo)
}

/// Moves a trashed item back if it is unchanged and its original path is free.
pub fn restore(item: &TrashedItem) -> ItemResult {
    if identity(&item.trashed).ok() != Some(item.identity) {
        return ItemResult::failed(&item.original, "the item is no longer in the trash");
    }
    match journaled(JournalOp::Restore, &item.trashed, &item.original, || rename_excl(&item.trashed, &item.original)) {
        Ok(()) => ItemResult::new(&item.original, ItemOutcome::Done(item.original.clone())),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => ItemResult::failed(&item.original, "the original location is occupied"),
        Err(e) => ItemResult::failed(&item.original, e),
    }
}

fn child(parent: &Path, name: &OsStr) -> io::Result<PathBuf> {
    validate_name(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid name"))?;
    Ok(parent.join(name))
}

fn created(path: &Path) -> io::Result<UndoAction> {
    Ok(UndoAction::Created { path: path.to_path_buf(), identity: identity(path)? })
}

/// Renames a set of items in two phases (each to a hop name, then to its target), which
/// makes swaps (a→b, b→a), chains and case-only renames safe. Identity is checked before
/// the first mutation; nothing is ever overwritten. Returns per-item results (in input
/// order) and the renames that completed.
pub(crate) fn rename_set(items: &[RenamedItem]) -> (Vec<ItemResult>, Vec<RenamedItem>) {
    let moving: HashSet<FileIdentity> = items.iter().map(|i| i.identity).collect();
    let mut results: Vec<Option<ItemResult>> = vec![None; items.len()];
    let mut hops = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if let Err(why) = check_rename(it, &moving) {
            results[i] = Some(ItemResult::failed(&it.from, why));
            continue;
        }
        let hop = it.from.with_file_name(format!("{HOP_PREFIX}{}", next_id()));
        let id = match journal::intent(JournalOp::Rename, &it.from, &it.to, &hop) {
            Ok(id) => id,
            Err(e) => {
                results[i] = Some(ItemResult::failed(&it.from, format!("journal unavailable: {e}")));
                continue;
            }
        };
        if let Err(e) = rename_excl(&it.from, &hop) {
            journal::done(&id);
            results[i] = Some(ItemResult::failed(&it.from, e));
            continue;
        }
        hops.push((i, hop, id));
    }
    let mut renamed = Vec::new();
    for (i, hop, id) in hops {
        let it = &items[i];
        results[i] = Some(match rename_excl(&hop, &it.to) {
            Ok(()) => {
                journal::done(&id);
                renamed.push(it.clone());
                ItemResult::new(&it.from, ItemOutcome::Done(it.to.clone()))
            }
            Err(e) => {
                let why = if e.kind() == io::ErrorKind::AlreadyExists { "the target name exists".into() } else { e.to_string() };
                if rename_excl(&hop, &it.from).is_ok() {
                    journal::done(&id);
                    ItemResult::failed(&it.from, why)
                } else {
                    // The journal intent stays open so startup recovery retries the way back.
                    ItemResult::failed(&it.from, format!("{why}; item left at {}", hop.display()))
                }
            }
        });
    }
    (results.into_iter().map(|r| r.expect("every item has a result")).collect(), renamed)
}

fn check_rename(it: &RenamedItem, moving: &HashSet<FileIdentity>) -> Result<(), String> {
    if identity(&it.from).ok() != Some(it.identity) {
        return Err("the item was changed or removed".into());
    }
    match identity(&it.to) {
        // Same file (case-only rename) or a batch member that moves away first.
        Ok(id) if id == it.identity || moving.contains(&id) => Ok(()),
        Ok(_) => Err("the target name exists".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::test_env;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    #[test]
    fn f02_case_only_rename() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("readme.md");
        fs::write(&a, b"x").unwrap();
        rename(&a, OsStr::new("README.md")).unwrap();
        assert_eq!(names(t.path()), vec!["README.md"]);
    }

    #[test]
    fn f02_rename_refuses_existing_target_and_bad_names() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert!(rename(&a, OsStr::new("b")).is_err());
        assert!(rename(&a, OsStr::new("x/y")).is_err());
        assert_eq!(fs::read(&b).unwrap(), b"b");
        assert_eq!(names(t.path()), vec!["a", "b"], "no hop file left behind");
    }

    #[test]
    fn f02_create_folder_file_and_symlinks() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let (dir, _) = create_folder(t.path(), OsStr::new("d")).unwrap();
        assert!(dir.is_dir());
        assert!(create_folder(t.path(), OsStr::new("d")).is_err());
        let (f, _) = create_file(t.path(), OsStr::new("f.txt")).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"");
        assert!(create_file(t.path(), OsStr::new("f.txt")).is_err());
        create_symlink(Path::new("missing"), &t.path().join("broken")).unwrap();
        assert_eq!(fs::read_link(t.path().join("broken")).unwrap(), Path::new("missing"));
    }

    #[test]
    fn f02_restore_refuses_occupied_original() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        fs::write(&a, b"old").unwrap();
        let (_, Some(UndoAction::Trashed(items))) = trash(std::slice::from_ref(&a)) else { unreachable!() };
        fs::write(&a, b"new").unwrap();
        assert!(matches!(restore(&items[0]).outcome, ItemOutcome::Failed(_)));
        assert_eq!(fs::read(&a).unwrap(), b"new");
        assert_eq!(fs::read(&items[0].trashed).unwrap(), b"old");
    }
}
