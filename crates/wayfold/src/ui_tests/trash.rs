//! U11/U12: trash and undo through the `test-trash` seam (never the real ~/.Trash).

use super::harness::{Env, keys, names, open, settle_until};
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// Panics unless trashing `item` lands in the temp env's test trash.
fn test_trash(env: &Env, item: &Path) -> PathBuf {
    assert!(item.starts_with(&env.root), "{} is outside the temp dir", item.display());
    let data = wayfold_core::storage::data_dir();
    assert_eq!(data, env.root.join("data"), "trash would not be redirected into the temp dir");
    data.join("test-trash")
}

/// Items in the test trash; the seam picks free names (e.g. `a 2.txt`), so tests never rely on them.
fn trashed(trash: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(trash) {
        Ok(dir) => dir.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn select(ws: &Entity<Workspace>, cx: &mut VisualTestContext, path: &Path) {
    ws.update(cx, |w, cx| {
        w.tab_mut().select_only(path);
        cx.notify();
    });
}

fn status(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    ws.read_with(cx, |w, _| w.status.as_deref().unwrap_or_default().to_owned())
}

fn undo_depth(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |w, _| w.undo_stack.len())
}

/// True (after logging) when the process can write into the read-only `dir`, i.e. runs as root.
fn running_as_root(dir: &Path) -> bool {
    let probe = dir.join("probe");
    if std::fs::write(&probe, b"").is_err() {
        return false;
    }
    std::fs::remove_file(&probe).unwrap();
    eprintln!("skipped: running as root");
    true
}

#[gpui_kit::test]
fn u11_trash_and_undo_a_file(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt", "b.txt"]);
    let item = env.work().join("a.txt");
    let trash = test_trash(&env, &item);
    let (ws, cx) = open(cx, &env);

    select(&ws, cx, &item);
    keys(&ws, cx, "cmd-backspace");
    settle_until(&ws, cx, "trash of a.txt", |cx| names(&ws, cx) == ["b.txt"]);
    assert!(!item.exists());
    let [in_trash] = trashed(&trash).try_into().expect("exactly one item in the test trash");
    assert_eq!(std::fs::read(in_trash).unwrap(), b"x");

    keys(&ws, cx, "cmd-z");
    settle_until(&ws, cx, "undo of the trash", |cx| names(&ws, cx) == ["a.txt", "b.txt"]);
    assert_eq!(std::fs::read(&item).unwrap(), b"x");
    assert_eq!(trashed(&trash), Vec::<PathBuf>::new());
}

#[gpui_kit::test]
fn u11_trash_and_undo_a_folder_with_children(cx: &mut TestAppContext) {
    let env = Env::new(&["keep.txt"]);
    let dir = env.work().join("d");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("x.txt"), b"1").unwrap();
    std::fs::write(dir.join("sub/y.txt"), b"2").unwrap();
    let trash = test_trash(&env, &dir);
    let (ws, cx) = open(cx, &env);

    select(&ws, cx, &dir);
    keys(&ws, cx, "cmd-backspace");
    settle_until(&ws, cx, "trash of d", |cx| names(&ws, cx) == ["keep.txt"]);
    assert!(!dir.exists());
    let [trashed_dir] = trashed(&trash).try_into().expect("exactly one item in the test trash");
    assert_eq!(std::fs::read(trashed_dir.join("sub/y.txt")).unwrap(), b"2");

    keys(&ws, cx, "cmd-z");
    settle_until(&ws, cx, "undo of the folder trash", |cx| names(&ws, cx) == ["d", "keep.txt"]);
    assert_eq!(std::fs::read(dir.join("x.txt")).unwrap(), b"1");
    assert_eq!(std::fs::read(dir.join("sub/y.txt")).unwrap(), b"2");
    assert_eq!(trashed(&trash), Vec::<PathBuf>::new());
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

struct Unlock<'a>(&'a Path, &'a Path);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        // No unwrap: a panic while already unwinding would abort the test run.
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(self.1, std::fs::Permissions::from_mode(0o644));
    }
}

#[gpui_kit::test]
fn u12_trash_failure_is_reported_and_keeps_the_item(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let locked = env.work().join("locked");
    let item = locked.join("ro.txt");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(&item, b"x").unwrap();
    let trash = test_trash(&env, &item);
    set_mode(&item, 0o444);
    set_mode(&locked, 0o555);
    // Restores the modes on every exit (also a timed-out wait), so the temp dir can be removed.
    let _unlock = Unlock(&locked, &item);
    if running_as_root(&locked) {
        return;
    }
    let (ws, cx) = open(cx, &env);
    ws.update(cx, |w, cx| w.navigate(locked.clone(), cx));
    settle_until(&ws, cx, "listing of locked", |cx| names(&ws, cx) == ["ro.txt"]);

    select(&ws, cx, &item);
    keys(&ws, cx, "cmd-backspace");
    settle_until(&ws, cx, "failure status", |cx| status(&ws, cx).starts_with("1 failed"));

    assert!(status(&ws, cx).contains("ro.txt"), "status does not name the item: {}", status(&ws, cx));
    assert_eq!(names(&ws, cx), ["ro.txt"]);
    assert_eq!(std::fs::read(&item).unwrap(), b"x");
    assert_eq!(trashed(&trash), Vec::<PathBuf>::new());

    // T-223: nothing reached the Trash, so no undo entry is left behind.
    assert_eq!(undo_depth(&ws, cx), 0);
    keys(&ws, cx, "cmd-z");
    assert_eq!(status(&ws, cx), "Nothing to undo");
}

/// T-223: after a partial failure, Undo restores exactly the items that went to the Trash.
#[gpui_kit::test]
fn u12_partial_trash_failure_undoes_only_the_trashed_items(cx: &mut TestAppContext) {
    let env = Env::new(&["ok.txt"]);
    let ok = env.work().join("ok.txt");
    // A read-only folder cannot move to another parent (its `..` entry would change).
    let locked = env.work().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("in.txt"), b"y").unwrap();
    let trash = test_trash(&env, &ok);
    set_mode(&locked, 0o555);
    let _unlock = Unlock(&locked, &locked.join("in.txt"));
    if running_as_root(&locked) {
        return;
    }
    let (ws, cx) = open(cx, &env);
    settle_until(&ws, cx, "listing of work", |cx| names(&ws, cx) == ["locked", "ok.txt"]);

    keys(&ws, cx, "cmd-a cmd-backspace");
    settle_until(&ws, cx, "partial failure status", |cx| status(&ws, cx).starts_with("1 failed"));
    assert!(status(&ws, cx).contains("locked"), "status does not name the folder: {}", status(&ws, cx));
    assert!(!ok.exists());
    assert_eq!(trashed(&trash).len(), 1);
    assert_eq!(undo_depth(&ws, cx), 1);

    keys(&ws, cx, "cmd-z");
    settle_until(&ws, cx, "undo of the partial trash", |cx| names(&ws, cx) == ["locked", "ok.txt"]);
    assert_eq!(std::fs::read(&ok).unwrap(), b"x");
    assert_eq!(std::fs::read(locked.join("in.txt")).unwrap(), b"y");
    assert_eq!(trashed(&trash), Vec::<PathBuf>::new());
    assert_eq!(undo_depth(&ws, cx), 0);
}
