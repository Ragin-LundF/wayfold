//! Tools dialogs: shared segmented mode control (T-210) and aligned result tables (T-207).

use super::harness::{Env, open, settle_until};
use crate::ui::tools_compare::CompareView;
use crate::ui::tools_diskusage::DiskUsageView;
use crate::ui::workspace::Workspace;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{ElementSnapshot, TestWindowExt as _};
use gpui_kit::{AppContext as _, Entity, ParentElement as _, Render, TestAppContext, VisualTestContext, px};
use wayfold_core::compare::CompareMode;

/// A name long enough to overflow the flexible column at any dialog width.
const LONG_NAME: &str = "a-very-long-file-name-that-would-push-the-other-columns-out-of-line-if-it-could.txt";

fn show<V: Render>(cx: &mut VisualTestContext, view: &Entity<V>, width: f32) {
    let view = view.clone();
    cx.update(|window, cx| window.open_dialog(cx, move |d, _, _| d.w(px(width)).child(view.clone())));
    cx.run_until_parked();
}

fn find(cx: &mut VisualTestContext, id: &'static str) -> ElementSnapshot {
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.find(id)
    })
}

/// Tabs through the dialog until `id` has keyboard focus (Tab only moves focus, it activates nothing).
fn tab_to(cx: &mut VisualTestContext, id: &'static str) {
    for _ in 0..20 {
        if find(cx, id).focused() == Some(true) {
            return;
        }
        cx.update(|window, cx| window.press("tab", cx));
    }
    panic!("Tab never focused {id}");
}

fn compare_view(ws: &Entity<Workspace>, cx: &mut VisualTestContext, env: &Env) -> Entity<CompareView> {
    let right = env.root.join("right");
    std::fs::create_dir(&right).unwrap();
    let (left, workspace) = (env.work(), ws.downgrade());
    cx.update(|window, cx| cx.new(|cx| CompareView::new(left, right, workspace, window, cx)))
}

fn disk_usage_view(ws: &Entity<Workspace>, cx: &mut VisualTestContext, env: &Env) -> Entity<DiskUsageView> {
    let view = cx.update(|_, cx| cx.new(|cx| DiskUsageView::new(env.work(), cx)));
    settle_until(ws, cx, "disk usage scan", |cx| view.read_with(cx, |v, _| !v.running));
    view
}

/// Asserts the active segment shows (and reports) the pressed state and the other does not.
fn assert_active_segment(cx: &mut VisualTestContext, active: &'static str, inactive: &'static str) {
    assert_eq!(find(cx, active).checked(), Some(true), "{active} is not the pressed segment");
    assert_eq!(find(cx, inactive).checked(), Some(false), "{inactive} is still pressed");
}

#[gpui_kit::test]
fn compare_mode_switches_by_click_and_keyboard(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    let view = compare_view(&ws, cx, &env);
    show(cx, &view, 980.);
    assert_active_segment(cx, "mode-st", "mode-cs");

    cx.update(|window, cx| window.click("mode-cs", cx));
    assert_eq!(view.read_with(cx, |v, _| v.mode), CompareMode::Checksum);
    assert_active_segment(cx, "mode-cs", "mode-st");

    tab_to(cx, "mode-st");
    cx.update(|window, cx| window.press("space", cx));
    assert_eq!(view.read_with(cx, |v, _| v.mode), CompareMode::SizeTime);
    assert_active_segment(cx, "mode-st", "mode-cs");
}

#[gpui_kit::test]
fn disk_usage_size_mode_switches_by_click_and_keyboard(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    let view = disk_usage_view(&ws, cx, &env);
    show(cx, &view, 900.);
    assert_active_segment(cx, "du-alloc", "du-logical");

    tab_to(cx, "du-logical");
    cx.update(|window, cx| window.press("space", cx));
    assert!(!view.read_with(cx, |v, _| v.allocated), "Space did not switch to logical sizes");
    assert_active_segment(cx, "du-logical", "du-alloc");

    cx.update(|window, cx| window.click("du-alloc", cx));
    assert!(view.read_with(cx, |v, _| v.allocated), "click did not switch to on-disk sizes");
    assert_active_segment(cx, "du-alloc", "du-logical");
}

/// Header cell and row cell of one column start at the same x and have the same width (within 1 px).
fn assert_column_aligned(cx: &mut VisualTestContext, header: &'static str, row: &str) {
    let h = cx.debug_bounds(header).unwrap_or_else(|| panic!("{header} not painted"));
    let r = cx.debug_bounds(Box::leak(row.to_owned().into_boxed_str())).unwrap_or_else(|| panic!("{row} not painted"));
    let near = |a: gpui_kit::Pixels, b: gpui_kit::Pixels| (a - b).abs() <= px(1.);
    assert!(near(h.origin.x, r.origin.x) && near(h.size.width, r.size.width), "{header} {h:?} vs {row} {r:?}");
}

/// Checks every painted row (`<prefix>-<ix>`) against its header; a short-name row is where a row
/// that does not fill the list width shrinks to its content and drifts from the header.
fn assert_rows_aligned(cx: &mut VisualTestContext, columns: &[(&'static str, &'static str)], rows: usize) {
    cx.update(|window, cx| window.render_frame(cx));
    for ix in 0..rows {
        for (header, cell) in columns {
            assert_column_aligned(cx, header, &format!("{cell}-{ix}"));
        }
    }
}

#[gpui_kit::test]
fn compare_table_columns_line_up_with_short_and_long_names(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt", LONG_NAME]);
    let (ws, cx) = open(cx, &env);
    let view = compare_view(&ws, cx, &env);
    show(cx, &view, 980.);
    cx.update(|window, cx| window.click("cmp-start", cx));
    settle_until(&ws, cx, "compare rows", |cx| view.read_with(cx, |v, _| v.rows.len() >= 2));
    let rows = view.read_with(cx, |v, _| v.rows.len());

    for width in [980., 640.] {
        cx.update(|window, cx| window.close_dialog(cx));
        show(cx, &view, width);
        assert_rows_aligned(cx, &[("cmp-h-path", "cmp-path"), ("cmp-h-copy", "cmp-copy")], rows);
    }
}

#[gpui_kit::test]
fn disk_usage_table_columns_line_up_with_short_and_long_names(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt", LONG_NAME]);
    let (ws, cx) = open(cx, &env);
    let view = disk_usage_view(&ws, cx, &env);

    for width in [900., 640.] {
        show(cx, &view, width);
        assert_rows_aligned(cx, &[("du-h-name", "du-name"), ("du-h-items", "du-items")], 2);
        cx.update(|window, cx| window.close_dialog(cx));
    }
}

#[gpui_kit::test]
fn compare_mode_is_locked_while_a_scan_runs(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    let view = compare_view(&ws, cx, &env);
    show(cx, &view, 980.);
    // Stands in for a running scan: the view disables the mode control from this flag alone.
    view.update(cx, |v, cx| {
        v.running = true;
        cx.notify();
    });

    assert_eq!(find(cx, "mode-cs").disabled(), Some(true), "Checksum segment is not disabled");
    cx.update(|window, cx| window.click("mode-cs", cx));
    assert_eq!(view.read_with(cx, |v, _| v.mode), CompareMode::SizeTime);
    assert_active_segment(cx, "mode-st", "mode-cs");
}
