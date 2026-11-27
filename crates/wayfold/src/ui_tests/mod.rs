//! Headless UI tests (spec 07). Each test builds an isolated `harness::Env` first.

mod clipboard;
mod drop;
mod folders_first;
mod harness;
mod keys;
mod regressions;
mod tags;
mod terminal;
mod theme;
mod tools;
mod transfers;
mod trash;

use gpui_kit::TestAppContext;
use harness::{Env, keys, names, open};
use wayfold_core::ops::{self, UndoAction};

#[gpui_kit::test]
fn ut_smoke_workspace_lists_temp_folder_and_cmd_z_undoes(cx: &mut TestAppContext) {
    let env = Env::new(&["b.txt", "a.txt", "c.txt"]);
    let (ws, cx) = open(cx, &env);

    assert_eq!(names(&ws, cx), ["a.txt", "b.txt", "c.txt"]);
    assert_eq!(ws.read_with(cx, |w, _| w.home.clone()), env.root.join("home"));
    assert_eq!(wayfold_core::storage::data_dir(), env.root.join("data"));

    keys(&ws, cx, "cmd-z");
    assert_eq!(ws.read_with(cx, |w, _| w.status.clone()).as_deref(), Some("Nothing to undo"));

    // Precondition for trash tests: the test-trash feature keeps items out of the real ~/.Trash.
    let probe = env.root.join("home/probe.txt");
    std::fs::write(&probe, b"x").unwrap();
    let (_, Some(UndoAction::Trashed(items))) = ops::trash(&[probe]) else { panic!("trash returned another undo action") };
    assert!(items[0].trashed.starts_with(env.root.join("data/test-trash")), "trashed to {}", items[0].trashed.display());
}
