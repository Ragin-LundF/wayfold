//! U14: Home/End move the cursor to the first/last row; with shift they extend the selection.
//! T-225: PageDown/PageUp move it by one visible page; with shift they extend the selection.

use super::harness::{Env, keys, open};
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::collections::HashSet;
use std::path::PathBuf;

fn cursor_and_selection(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> (Option<PathBuf>, HashSet<PathBuf>) {
    ws.read_with(cx, |w, _| (w.tab().cursor.clone(), w.tab().selection.clone()))
}

#[gpui_kit::test]
fn u14_home_end_and_shift_extend_in_every_view(cx: &mut TestAppContext) {
    let env = Env::new(&["a", "b", "c", "d"]);
    let (ws, cx) = open(cx, &env);
    let path = |name: &str| env.work().join(name);
    let all: HashSet<PathBuf> = ["a", "b", "c", "d"].map(path).into();

    for view in ["cmd-2", "cmd-1", "cmd-3"] {
        keys(&ws, cx, view);
        keys(&ws, cx, "end");
        assert_eq!(cursor_and_selection(&ws, cx), (Some(path("d")), [path("d")].into()), "end in {view}");
        keys(&ws, cx, "home");
        assert_eq!(cursor_and_selection(&ws, cx), (Some(path("a")), [path("a")].into()), "home in {view}");
        keys(&ws, cx, "shift-end");
        assert_eq!(cursor_and_selection(&ws, cx), (Some(path("d")), all.clone()), "shift-end in {view}");
        keys(&ws, cx, "down down end shift-home");
        assert_eq!(cursor_and_selection(&ws, cx), (Some(path("a")), all.clone()), "shift-home in {view}");
    }
}

fn viewport_height(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> f32 {
    ws.read_with(cx, |w, _| w.scroll[&w.tab().id].0.borrow().last_item_size.unwrap().item.height.to_f64() as f32)
}

fn scroll_offset(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> gpui_kit::Pixels {
    ws.read_with(cx, |w, _| w.scroll[&w.tab().id].0.borrow().base_handle.offset().y)
}

fn cursor_index(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |w, _| w.rows().iter().position(|e| Some(&e.path) == w.tab().cursor.as_ref()).unwrap())
}

#[gpui_kit::test]
fn t225_page_down_up_move_by_a_visible_page_and_shift_extends(cx: &mut TestAppContext) {
    let files: Vec<String> = (0..300).map(|i| format!("f{i:03}")).collect();
    let env = Env::new(&files.iter().map(String::as_str).collect::<Vec<_>>());
    let (ws, cx) = open(cx, &env);

    keys(&ws, cx, "home pagedown");
    let page = cursor_index(&ws, cx);
    let measured = (viewport_height(&ws, cx) / crate::ui::theme::ROW_H) as usize;
    assert_eq!(page, measured, "one page is the rows the measured viewport shows");
    assert_ne!(page, 10, "not the pre-layout fallback");
    keys(&ws, cx, "pagedown");
    assert_eq!(cursor_index(&ws, cx), 2 * page);
    assert!(scroll_offset(&ws, cx) < gpui_kit::px(0.), "the cursor row is scrolled into view");

    keys(&ws, cx, "pageup");
    assert_eq!(cursor_index(&ws, cx), page);
    keys(&ws, cx, "shift-pagedown");
    assert_eq!(cursor_index(&ws, cx), 2 * page);
    assert_eq!(cursor_and_selection(&ws, cx).1.len(), page + 1, "shift-pagedown extends from the anchor");
    keys(&ws, cx, "shift-pageup shift-pageup");
    assert_eq!(cursor_index(&ws, cx), 0, "clamped at the first row");
    assert_eq!(cursor_and_selection(&ws, cx).1.len(), page + 1);

    keys(&ws, cx, "end pagedown");
    assert_eq!(cursor_index(&ws, cx), 299, "clamped at the last row");
}

#[gpui_kit::test]
fn t225_icons_view_scrolls_the_cursor_tile_into_view_and_pages_whole_tile_rows(cx: &mut TestAppContext) {
    let files: Vec<String> = (0..300).map(|i| format!("f{i:03}")).collect();
    let env = Env::new(&files.iter().map(String::as_str).collect::<Vec<_>>());
    let (ws, cx) = open(cx, &env);

    keys(&ws, cx, "cmd-1 home end");
    assert_eq!(cursor_index(&ws, cx), 299);
    assert!(scroll_offset(&ws, cx) < gpui_kit::px(0.), "end scrolls the icon list to the last tile");

    keys(&ws, cx, "home right pagedown");
    let per_row = ws.read_with(cx, |w, _| w.icons_per_row.get());
    let tile_rows = (viewport_height(&ws, cx) / crate::ui::browser::ICON_ROW_H) as usize;
    assert!(per_row > 1, "the test window fits several tiles per row");
    assert_eq!(cursor_index(&ws, cx), 1 + tile_rows * per_row, "a page is whole tile rows, same column");
}
