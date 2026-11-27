//! Regressions U01-U05 (spec 07): each test failed before its fix.

use super::harness::{Env, dialog_open, keys, names, open, settle, settle_until};
use crate::app::state::SortKey;
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::path::Path;

/// Selects the first row, presses Return (Rename), types `text` and presses Return again.
fn rename_first_row(ws: &Entity<Workspace>, cx: &mut VisualTestContext, text: &str) {
    keys(ws, cx, "down enter");
    assert!(dialog_open(cx), "Rename dialog did not open");
    cx.simulate_input(text);
    keys(ws, cx, "enter");
}

#[gpui_kit::test]
fn u01_palette_input_is_focused_and_enter_runs_the_selected_command(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    let sort = ws.read_with(cx, |w, _| w.tab().sort);
    assert_eq!(sort.key, SortKey::Name);

    keys(&ws, cx, "cmd-k");
    assert!(dialog_open(cx), "palette did not open");
    cx.simulate_input("sort by size");
    keys(&ws, cx, "enter");

    let after = ws.read_with(cx, |w, _| w.tab().sort);
    assert_eq!(after.key, SortKey::Size, "typed text did not reach the palette input");
    assert!(!dialog_open(cx), "palette still open after running the command");

    // No match: Return must not run any command, neither SortBySize again (would flip the
    // direction) nor row 0 (Go to Folder…, starts path editing).
    let state = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
        ws.read_with(cx, |w, _| {
            let t = w.tab();
            (t.sort, t.location.clone(), t.view, t.filter.clone(), w.path_editing)
        })
    };
    let before = state(&ws, cx);
    keys(&ws, cx, "cmd-k");
    assert!(dialog_open(cx), "palette did not reopen");
    cx.simulate_input("zzqqxx");
    keys(&ws, cx, "enter");
    assert_eq!(state(&ws, cx), before, "Return without a match ran a command");
    assert!(dialog_open(cx), "palette closed on Return without a match");
}

#[gpui_kit::test]
fn u02_return_in_rename_dialog_renames_the_file(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let (ws, cx) = open(cx, &env);

    rename_first_row(&ws, cx, "b");

    settle_until(&ws, cx, "rename to b.txt", |cx| names(&ws, cx) == ["b.txt"]);
    assert!(!env.work().join("a.txt").exists());
    assert!(!dialog_open(cx), "Rename dialog still open");
}

#[gpui_kit::test]
fn u03_return_in_search_dialog_applies_the_query(cx: &mut TestAppContext) {
    let env = Env::new(&["needle.txt", "other.txt"]);
    let (ws, cx) = open(cx, &env);

    keys(&ws, cx, "cmd-f");
    assert!(dialog_open(cx), "Search dialog did not open");
    cx.simulate_input("needle");
    keys(&ws, cx, "enter");

    // Filter (current folder) is the default search scope, so the query lands in the tab filter.
    assert!(!dialog_open(cx), "Search dialog still open");
    assert_eq!(ws.read_with(cx, |w, _| w.tab().filter.clone()), "needle");
    settle_until(&ws, cx, "filter to needle.txt", |cx| names(&ws, cx) == ["needle.txt"]);
}

#[gpui_kit::test]
fn u04_rows_never_mix_folders_with_equal_listing_versions(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    // Same entry count, so both listings get the same per-folder versions; the hidden file in A
    // makes A's cached row indices wrong for B (two rows instead of three).
    let (a, b) = (env.work().join("a"), env.work().join("b"));
    for (dir, files) in [(&a, [".hidden", "a2", "a1"]), (&b, ["b3", "b1", "b2"])] {
        std::fs::create_dir(dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
    }
    let (ws, cx) = open(cx, &env);
    let go = |ws: &Entity<Workspace>, cx: &mut VisualTestContext, dir: &Path| {
        let dir = dir.to_path_buf();
        ws.update(cx, |w, cx| w.navigate(dir, cx));
        settle(ws, cx);
    };

    go(&ws, cx, &a);
    assert_eq!(names(&ws, cx), ["a1", "a2"]);
    go(&ws, cx, &b);
    assert_eq!(names(&ws, cx), ["b1", "b2", "b3"]);
    go(&ws, cx, &a);
    assert_eq!(names(&ws, cx), ["a1", "a2"]);
    keys(&ws, cx, "cmd-[");
    assert_eq!(ws.read_with(cx, |w, _| w.tab().location.clone()), b);
    assert_eq!(names(&ws, cx), ["b1", "b2", "b3"]);

    ws.update(cx, |w, cx| w.set_sort(SortKey::Name, cx));
    assert_eq!(names(&ws, cx), ["b3", "b2", "b1"]);
    keys(&ws, cx, "cmd-[");
    assert_eq!(names(&ws, cx), ["a2", "a1"], "the tab keeps its descending sort");
}

#[gpui_kit::test]
fn u05_rename_replaces_the_stem_and_keeps_the_extension(cx: &mut TestAppContext) {
    let env = Env::new(&["report.final.pdf"]);
    let (ws, cx) = open(cx, &env);

    rename_first_row(&ws, cx, "x");

    settle_until(&ws, cx, "rename to x.pdf", |cx| names(&ws, cx) == ["x.pdf"]);
}

#[gpui_kit::test]
fn u05_rename_of_a_folder_replaces_the_whole_name(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    std::fs::create_dir(env.work().join("my.dir")).unwrap();
    let (ws, cx) = open(cx, &env);

    rename_first_row(&ws, cx, "x");

    settle_until(&ws, cx, "rename to x", |cx| names(&ws, cx) == ["x"]);
    assert!(env.work().join("x").is_dir());
}
