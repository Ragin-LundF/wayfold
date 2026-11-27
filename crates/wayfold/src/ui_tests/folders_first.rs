//! The global "Folders First" pref re-sorts every pane at once and persists in the temp data dir.

use super::harness::{Env, keys, open, settle, settle_until};
use crate::app::actions::{ToggleFoldersFirst, ToggleSplit, ViewColumns};
use crate::app::state::SortKey;
use crate::ui::workspace::{Prefs, Workspace};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use wayfold_core::storage;

const FOLDERS_FIRST: [&str; 4] = ["b-dir", "d-dir", "a.txt", "c.txt"];
const MIXED: [&str; 4] = ["a.txt", "b-dir", "c.txt", "d-dir"];

fn pane_names(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Vec<String>> {
    ws.read_with(cx, |w, _| {
        w.panes.iter().map(|p| w.rows_for(p.tab()).iter().map(|e| e.name.to_string_lossy().into_owned()).collect()).collect()
    })
}

fn toggle_and_check(ws: &Entity<Workspace>, cx: &mut VisualTestContext, on: bool, expected: [&str; 4]) {
    cx.dispatch_action(ToggleFoldersFirst);
    assert_eq!(ws.read_with(cx, |w, _| w.prefs.folders_first), on);
    assert_eq!(pane_names(ws, cx), [expected, expected]);
    settle_until(ws, cx, "prefs saved", |_| storage::load::<Prefs>("prefs").folders_first == on);
}

fn env_with_dirs(files: &[&str], dirs: &[&str]) -> Env {
    let env = Env::new(files);
    for d in dirs {
        std::fs::create_dir(env.work().join(d)).unwrap();
    }
    env
}

#[gpui_kit::test]
fn folders_first_toggle_resorts_all_panes_and_persists(cx: &mut TestAppContext) {
    let env = env_with_dirs(&["a.txt", "c.txt"], &["b-dir", "d-dir"]);
    let (ws, cx) = open(cx, &env);
    cx.dispatch_action(ToggleSplit);
    settle(&ws, cx);
    // Both panes show the folder and have cached rows before the toggle.
    assert_eq!(pane_names(&ws, cx), [FOLDERS_FIRST, FOLDERS_FIRST]);

    toggle_and_check(&ws, cx, false, MIXED);
    toggle_and_check(&ws, cx, true, FOLDERS_FIRST);
    toggle_and_check(&ws, cx, false, MIXED);

    // A new Workspace (as on the next start) restores the pref, even over a tab sort with folders first.
    let (reopened, cx) = open(cx, &env);
    assert!(!reopened.read_with(cx, |w, _| w.prefs.folders_first));
    assert!(reopened.read_with(cx, |w, _| w.tab().sort.folders_first));
    assert_eq!(super::harness::names(&reopened, cx), MIXED);
}

/// Names of the current column's rows, top to bottom, as painted.
fn painted_column_order(cx: &mut VisualTestContext) -> Vec<&'static str> {
    cx.update(|window, cx| window.render_frame(cx));
    let mut rows: Vec<_> = MIXED
        .iter()
        .map(|&name| {
            let selector: &'static str = Box::leak(format!("col-row-{name}").into_boxed_str());
            (cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} not painted")).origin.y, name)
        })
        .collect();
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    rows.into_iter().map(|(_, name)| name).collect()
}

#[gpui_kit::test]
fn columns_view_follows_the_global_pref_over_the_tab_sort(cx: &mut TestAppContext) {
    let env = env_with_dirs(&["a.txt", "c.txt"], &["b-dir", "d-dir"]);
    let (ws, cx) = open(cx, &env);
    cx.dispatch_action(ViewColumns);
    cx.dispatch_action(ToggleFoldersFirst);
    settle(&ws, cx);
    // The tab sort still says folders first; only the global pref is off.
    assert!(ws.read_with(cx, |w, _| w.tab().sort.folders_first));
    assert_eq!(painted_column_order(cx), MIXED);
}

/// How far the active tab's list is scrolled down, in pixels (0 at the top).
fn scrolled_down(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> f32 {
    cx.update(|window, cx| window.render_frame(cx));
    ws.read_with(cx, |w, _| -f32::from(w.scroll[&w.tab().id].0.borrow().base_handle.offset().y))
}

#[gpui_kit::test]
fn toggling_folders_first_scrolls_the_cursor_row_into_view(cx: &mut TestAppContext) {
    let dirs: Vec<String> = (0..100).map(|i| format!("d{i:03}")).collect();
    let env = env_with_dirs(&["a.txt"], &dirs.iter().map(String::as_str).collect::<Vec<_>>());
    let (ws, cx) = open(cx, &env);
    cx.dispatch_action(ToggleFoldersFirst);
    keys(&ws, cx, "down");
    assert_eq!(super::harness::names(&ws, cx)[0], "a.txt");
    assert_eq!(scrolled_down(&ws, cx), 0.);

    // Folders first again: a.txt moves below the 100 folders.
    cx.dispatch_action(ToggleFoldersFirst);
    assert!(scrolled_down(&ws, cx) > 0., "cursor row a.txt was left off screen");
}

#[gpui_kit::test]
fn changing_the_sort_scrolls_the_cursor_row_into_view(cx: &mut TestAppContext) {
    let files: Vec<String> = (0..100).map(|i| format!("f{i:03}.txt")).collect();
    let env = Env::new(&files.iter().map(String::as_str).collect::<Vec<_>>());
    let (ws, cx) = open(cx, &env);
    keys(&ws, cx, "down");
    assert_eq!(scrolled_down(&ws, cx), 0.);

    // Name again flips to descending: the cursor's f000.txt becomes the last row.
    ws.update(cx, |w, cx| w.set_sort(SortKey::Name, cx));
    assert!(scrolled_down(&ws, cx) > 0., "cursor row f000.txt was left off screen");
}

#[test]
fn prefs_without_folders_first_default_to_on() {
    let prefs: Prefs = serde_json::from_str("{}").unwrap();
    assert!(prefs.folders_first);
}
