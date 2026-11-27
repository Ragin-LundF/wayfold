//! Core analysis and archive tools (F07 compare, P04 disk usage, F08 archives).
//! Entry points dispatched from menus, context menu and the command palette; the dialogs
//! live in `tools_compare`, `tools_diskusage` and `tools_archive`.

use crate::ui::tools_archive::{CreateArchiveView, ExtractArchiveView};
use crate::ui::tools_compare::CompareView;
use crate::ui::tools_diskusage::DiskUsageView;
use crate::ui::workspace::Workspace;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use std::path::PathBuf;
use wayfold_core::archive::ArchiveFormat;

/// Cell of a tool result table. Header and rows build their cells from the same widths, so the
/// columns line up at any dialog width; `None` is the flexible column, which shrinks and truncates
/// instead of pushing the fixed ones.
pub fn table_cell(width: Option<f32>) -> Div {
    match width {
        Some(w) => div().flex_none().w(px(w)).overflow_hidden(),
        None => div().flex_1().min_w_0().overflow_hidden(),
    }
}

pub fn open_compare(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    // Two panes: compare their folders. One pane: the folder and its parent.
    let (left, right) = match w.panes.as_slice() {
        [a, b, ..] => (a.tab().location.clone(), b.tab().location.clone()),
        _ => {
            let here = w.tab().location.clone();
            let parent = here.parent().map(PathBuf::from).unwrap_or_else(|| here.clone());
            (here, parent)
        }
    };
    let workspace = cx.entity().downgrade();
    let view = cx.new(|cx| CompareView::new(left, right, workspace, window, cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Compare Folders").w(px(980.)).overlay_closable(false).child(view.clone()));
}

pub fn open_disk_usage(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let root = w.selected_entries().into_iter().find(|e| e.is_dir()).map(|e| e.path).unwrap_or_else(|| w.tab().location.clone());
    // The scan and its tree live in the view: closing the dialog drops both and cancels the scan.
    let view = cx.new(|cx| DiskUsageView::new(root, cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Disk Usage").w(px(900.)).overlay_closable(false).child(view.clone()));
}

pub fn open_create_archive(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let sources: Vec<PathBuf> = w.selected_entries().into_iter().map(|e| e.path).collect();
    if sources.is_empty() {
        w.notify_status("Select the items to archive", cx);
        return;
    }
    let dest_dir = w.tab().location.clone();
    let workspace = cx.entity().downgrade();
    let view = cx.new(|cx| CreateArchiveView::new(sources, dest_dir, workspace, window, cx));
    let child = view.clone();
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Create Archive").w(px(600.)).overlay_closable(false).child(child.clone()));
    let own = window.focused(cx);
    view.update(cx, |v, _| v.own_dialog.handle = own);
}

pub fn open_extract_archive(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let archive = w
        .focused_entry()
        .filter(|e| ArchiveFormat::detect(&e.path).is_some())
        .or_else(|| w.selected_entries().into_iter().find(|e| ArchiveFormat::detect(&e.path).is_some()));
    let Some(archive) = archive else {
        w.notify_status("Select a ZIP, TAR, GZ, ZST, 7z or RAR archive to extract", cx);
        return;
    };
    let dest_dir = w.tab().location.clone();
    let workspace = cx.entity().downgrade();
    let view = cx.new(|cx| ExtractArchiveView::new(archive.path, dest_dir, workspace, window, cx));
    let child = view.clone();
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Extract Archive").w(px(820.)).overlay_closable(false).child(child.clone()));
    let own = window.focused(cx);
    view.update(cx, |v, _| v.own_dialog.handle = own);
}
