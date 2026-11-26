//! One browser pane: tab strip, breadcrumb, and list / icon / Miller-column views.
//! All views are virtualized (`uniform_list`) so 100k-entry folders stay smooth.

use crate::app::actions::*;
use crate::app::state::*;
use crate::ui::theme::{self, palette, Palette, HEADER_H, ROW_H, TABSTRIP_H};
use crate::ui::workspace::Workspace;
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::component::{h_flex, v_flex, Icon};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::Sizable as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use waypoint_core::fs::{format_size, Entry, Kind};

pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

pub fn entry_icon(e: &Entry) -> Icon {
    match e.kind {
        Kind::Dir => icon("folder"),
        Kind::Symlink { to_dir: true, .. } => icon("folder-symlink"),
        Kind::Symlink { .. } => icon("file-symlink"),
        _ => match e.kind_label() {
            "Image" => icon("file-image"),
            "Archive" => icon("file-archive"),
            "Code" => icon("file-code"),
            "Markdown" | "Text" | "PDF" | "HTML" => icon("file-text"),
            "CSV" => icon("file-spreadsheet"),
            "Audio" => icon("file-music"),
            "Video" => icon("file-play"),
            _ => icon("file"),
        },
    }
}

/// Miller columns: up to two ancestors above the current folder, then the folder itself.
pub fn column_dirs(loc: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = loc.ancestors().take(3).map(Path::to_path_buf).collect();
    v.reverse();
    v
}

/// Drag payload for entries moved between panes / folders (F02 drag & drop).
#[derive(Clone)]
pub struct DraggedEntries {
    pub paths: Vec<PathBuf>,
    /// Same order as `paths`: whether each is a folder (needed for native drags out).
    pub dirs: Vec<bool>,
}

pub struct DragPreview {
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div().px_2().py_1().rounded_md().bg(p.selection).text_color(p.text).text_sm().child(self.label.clone())
    }
}

pub fn format_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".into() };
    let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) else { return "—".into() };
    let secs = d.as_secs() as i64;
    // Civil date from days since epoch (UTC; shortcut: no local timezone offset, upgrade with a tz crate if users ask).
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, dd) = civil_from_days(days);
    format!("{y:04}-{m:02}-{dd:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn tag_dots(e: &Entry) -> impl IntoElement {
    h_flex().gap(px(3.)).children(e.tags.iter().filter_map(|t| {
        theme::tag_color(t).map(|c| div().size(px(9.)).rounded_full().bg(c).flex_none())
    }))
}

/// Clipboard mark: icon + text, never color alone (F01).
fn clip_mark(w: &Workspace, e: &Entry, p: &Palette) -> Option<AnyElement> {
    let mode = w.clip_mode_for(&e.path)?;
    let (ico, label) = if mode == ClipMode::Cut { ("scissors", "CUT") } else { ("copy", "COPY") };
    Some(
        h_flex()
            .gap_1()
            .text_xs()
            .text_color(p.warning)
            .child(icon(ico).xsmall().text_color(p.warning))
            .child(label)
            .into_any_element(),
    )
}

pub use crate::ui::clipboard::ClipMode;

pub fn render_pane(w: &Workspace, ix: usize, window: &mut Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let pane = &w.panes[ix];
    let tab = pane.tab();
    let active = ix == w.active_pane;
    let focus = w.pane_focus[ix].clone();
    let listing = w.cache.get(&tab.location);

    let body: AnyElement = match (listing, tab.view) {
        _ if w.search.active_for(tab.id) => crate::ui::search::render_results(w, ix, cx),
        (Some(l), _) if l.error.is_some() => status_message(&p, &format!("Cannot open folder: {}", l.error.as_deref().unwrap_or("")), true),
        (None, _) => status_message(&p, "Loading…", false),
        (_, ViewMode::List) => render_list(w, ix, cx).into_any_element(),
        (_, ViewMode::Icons) => render_icons(w, ix, window, cx).into_any_element(),
        (_, ViewMode::Columns) => render_columns(w, ix, cx).into_any_element(),
    };
    let loading = listing.is_some_and(|l| l.loading);

    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .border_r_1()
        .border_color(p.line)
        .when(active && w.panes.len() > 1, |d| d.border_t_2().border_color(p.accent))
        .child(render_tabs(w, ix, cx))
        .child(render_breadcrumb(w, ix, loading, cx))
        .child(
            div()
                .id(("pane-body", ix))
                .role(Role::List)
                .aria_label(SharedString::from(format!("Files in {}", tab.title())))
                .flex_1()
                .min_h_0()
                .track_focus(&focus)
                .key_context(BROWSER)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |w, _, window, cx| {
                        if w.active_pane != ix {
                            w.activate_pane(ix, window, cx);
                        }
                    }),
                )
                .on_key_down(cx.listener(|w, ev: &KeyDownEvent, _, cx| {
                    let ks = &ev.keystroke;
                    if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.function {
                        return;
                    }
                    if let Some(ch) = ks.key_char.as_deref().filter(|c| c.chars().all(|c| !c.is_control() && c != ' ')) {
                        w.type_to_select(ch, cx);
                    }
                }))
                .on_drop(cx.listener(move |w, d: &DraggedEntries, window, cx| {
                    let dest = w.panes[ix].tab().location.clone();
                    w.drop_paths(d.paths.clone(), dest, window, cx);
                }))
                .on_drop(cx.listener(move |w, d: &ExternalPaths, window, cx| {
                    let dest = w.panes[ix].tab().location.clone();
                    w.drop_paths(d.paths().to_vec(), dest, window, cx);
                }))
                .child(body),
        )
}

fn status_message(p: &Palette, msg: &str, error: bool) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(if error { p.danger } else { p.muted })
        .child(SharedString::from(msg.to_owned()))
        .into_any_element()
}

/// Tabs: one container per tab with separate activate and close hit targets (v0.3).
fn render_tabs(w: &Workspace, ix: usize, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let pane = &w.panes[ix];
    let single = pane.tabs.len() == 1;
    h_flex()
        .id(("tabs", ix))
        .h(px(TABSTRIP_H + 8.))
        .px_2()
        .gap_2()
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .overflow_x_scroll()
        .children(pane.tabs.iter().enumerate().map(|(j, t)| {
            let on = j == pane.active;
            let id = t.id;
            let title = SharedString::from(t.title());
            h_flex()
                .flex_none()
                .h(px(TABSTRIP_H - 6.))
                .rounded(px(6.))
                .border_1()
                .border_color(if on { p.accent } else { p.line })
                .bg(if on { p.selection } else { p.control })
                .overflow_hidden()
                .child(
                    div()
                        .id(("tab", id.0))
                        .role(Role::Tab)
                        .aria_label(SharedString::from(format!("{title} tab")))
                        .aria_selected(on)
                        .px_3()
                        .h_full()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .hover(|s| s.bg(p.hover))
                        .child(icon("folder").xsmall().text_color(p.muted))
                        .child(title.clone())
                        .on_click(cx.listener(move |w, _, window, cx| {
                            w.active_pane = ix;
                            if let Some(k) = w.panes[ix].tabs.iter().position(|t| t.id == id) {
                                w.panes[ix].active = k;
                            }
                            w.after_location_change(cx);
                            w.focus_browser(window, cx);
                        })),
                )
                .child(
                    div()
                        .id(("tab-close", id.0))
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!("Close {title} tab")))
                        .px_2()
                        .h_full()
                        .flex()
                        .items_center()
                        .text_color(p.muted)
                        .when(single, |d| d.opacity(0.25))
                        .when(!single, |d| {
                            d.hover(|s| s.bg(p.hover).text_color(p.text)).on_click(cx.listener(move |w, _, _, cx| {
                                cx.stop_propagation();
                                if w.panes[ix].close_tab(id) {
                                    w.after_location_change(cx);
                                }
                            }))
                        })
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(format!("Close {title} tab")).build(window, cx))
                        .child(icon("x").xsmall()),
                )
        }))
        .child(
            div()
                .id(("new-tab", ix))
                .role(Role::Button)
                .aria_label("New tab")
                .flex_none()
                .px_2()
                .py_1()
                .rounded_md()
                .hover(|s| s.bg(p.hover))
                .child(icon("plus").small())
                .on_click(cx.listener(move |w, _, window, cx| {
                    w.active_pane = ix;
                    w.panes[ix].new_tab();
                    w.after_location_change(cx);
                    w.focus_browser(window, cx);
                })),
        )
}

/// Clickable breadcrumb segments of the pane's location (N01).
fn render_breadcrumb(w: &Workspace, ix: usize, loading: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let tab = w.panes[ix].tab();
    let mut crumbs: Vec<PathBuf> = tab.location.ancestors().map(Path::to_path_buf).collect();
    crumbs.reverse();
    let n = crumbs.len();
    h_flex()
        .id(("crumbs", ix))
        .h(px(30.))
        .px_3()
        .gap_1()
        .text_sm()
        .text_color(p.muted)
        .border_b_1()
        .border_color(p.line)
        .overflow_x_scroll()
        .children(crumbs.into_iter().enumerate().flat_map(move |(k, c)| {
            let label = SharedString::from(display_name(&c));
            let last = k + 1 == n;
            let seg = div()
                .id(SharedString::from(format!("crumb-{ix}-{k}")))
                .role(Role::Link)
                .aria_label(SharedString::from(format!("Go to {label}")))
                .flex_none()
                .px_1()
                .rounded_sm()
                .when(last, |d| d.text_color(p.text))
                .hover(|s| s.bg(p.hover))
                .child(label)
                .on_click(cx.listener(move |w, _, window, cx| {
                    w.active_pane = ix;
                    w.navigate(c.clone(), cx);
                    w.focus_browser(window, cx);
                }))
                .into_any_element();
            let sep = (!last).then(|| icon("chevron-right").xsmall().text_color(p.muted).into_any_element());
            std::iter::once(seg).chain(sep)
        }))
        .when(!tab.filter.is_empty(), |d| {
            d.child(div().ml_2().text_color(p.warning).child(SharedString::from(format!("Filter: {}", tab.filter))))
        })
        .when_some(w.tag_filter.clone(), |d, t| {
            d.child(div().ml_2().text_color(p.warning).child(SharedString::from(format!("Tag: {t}"))))
        })
        .when(loading, |d| d.child(div().ml_auto().text_xs().child("Loading…")))
}

/// Row click semantics shared by all views (N05).
fn click_entry(w: &mut Workspace, ix: usize, path: &Path, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Workspace>) {
    if w.active_pane != ix {
        w.activate_pane(ix, window, cx);
    }
    let m = ev.modifiers();
    let mut t = w.tab().clone();
    let rows = w.rows_for(&t);
    if m.platform {
        t.toggle(path);
    } else if m.shift {
        t.select_range(&rows, path);
    } else {
        t.select_only(path);
    }
    let open = ev.click_count() == 2 && !m.platform && !m.shift;
    let entry = rows.iter().find(|e| e.path == path).map(|e| (*e).clone());
    *w.tab_mut() = t;
    if open {
        if let Some(e) = entry {
            w.open_entry(&e, cx);
        }
    } else if w.tab().view == ViewMode::Columns {
        w.refresh_visible(cx);
    }
    w.focus_browser(window, cx);
    cx.notify();
}

fn entry_context_menu<E: ContextMenuExt + 'static>(el: E) -> gpui_kit::component::menu::ContextMenu<E> {
    el.context_menu(|menu, _, _| {
        menu.menu("Open", Box::new(OpenItems))
            .menu("Open With…", Box::new(OpenWith))
            .menu("Show Original", Box::new(RevealOriginal))
            .separator()
            .menu("Copy", Box::new(CopyItems))
            .menu("Cut", Box::new(CutItems))
            .menu("Paste", Box::new(PasteItems))
            .menu("Copy Path", Box::new(CopyPath))
            .separator()
            .menu("Rename", Box::new(RenameItem))
            .menu("Batch Rename…", Box::new(BatchRename))
            .menu("Duplicate", Box::new(DuplicateItems))
            .menu("Make Alias", Box::new(MakeAlias))
            .menu("Tags…", Box::new(EditTags))
            .menu("Add to Favorites", Box::new(AddFavorite))
            .separator()
            .menu("Get Info / Folder Size", Box::new(GetInfo))
            .menu("Compress…", Box::new(CreateArchive))
            .menu("Extract…", Box::new(ExtractArchive))
            .menu("Compare Folders…", Box::new(CompareFolders))
            .menu("Analyze Disk Usage…", Box::new(AnalyzeDiskUsage))
            .separator()
            .menu("Move to Trash", Box::new(TrashItems))
    })
}

/// Right-click selects the row first (unless it is already part of the selection).
fn select_for_context(w: &mut Workspace, ix: usize, path: &Path, window: &mut Window, cx: &mut Context<Workspace>) {
    if w.active_pane != ix {
        w.activate_pane(ix, window, cx);
    }
    if !w.tab().selection.contains(path) {
        w.tab_mut().select_only(path);
        cx.notify();
    }
}

fn drag_source<E: StatefulInteractiveElement>(el: E, w: &Workspace, e: &Entry) -> E {
    let (paths, dirs): (Vec<PathBuf>, Vec<bool>) = if w.tab().selection.contains(&e.path) {
        let rows = w.rows();
        w.tab().selected(&rows).into_iter().map(|e| (e.path.clone(), e.is_dir())).unzip()
    } else {
        (vec![e.path.clone()], vec![e.is_dir()])
    };
    let label: SharedString = if paths.len() == 1 { e.display_name().into() } else { format!("{} items", paths.len()).into() };
    el.on_drag(DraggedEntries { paths, dirs }, move |_, _, _, cx| cx.new(|_| DragPreview { label: label.clone() }))
        .external_drag_payload(|d: &DraggedEntries, _, _| {
            Some(ExternalDragPayload::Files(FileDragPaths::new(d.paths.iter().cloned().zip(d.dirs.iter().copied()))))
        })
}

fn row_bg(sel: bool, p: &Palette) -> Hsla {
    if sel { p.selection } else { gpui_kit::transparent_black() }
}

fn render_list(w: &Workspace, ix: usize, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let tab = w.panes[ix].tab();
    let count = display_items(&w.rows_for(tab), tab.sort.group_kind).len();
    let scroll = w.scroll.get(&tab.id).cloned().unwrap_or_default();
    let cols = w.columns;
    let sort = tab.sort;
    let header_cell = |label: &'static str, key: SortKey, width: f32, col: usize, cx: &mut Context<Workspace>| {
        let arrow = if sort.key == key { if sort.ascending { " ▲" } else { " ▼" } } else { "" };
        h_flex()
            .w(px(width))
            .flex_none()
            .h_full()
            .child(
                div()
                    .id(SharedString::from(format!("th-{ix}-{label}")))
                    .flex_1()
                    .px_2()
                    .h_full()
                    .flex()
                    .items_center()
                    .hover(|s| s.bg(p.hover))
                    .child(SharedString::from(format!("{label}{arrow}")))
                    .on_click(cx.listener(move |w, _, _, cx| {
                        w.active_pane = ix;
                        w.set_sort(key, cx);
                    })),
            )
            .child(
                // Column resize handle (N02 adjustable columns).
                div()
                    .id(SharedString::from(format!("th-resize-{ix}-{col}")))
                    .w(px(5.))
                    .h_full()
                    .cursor_col_resize()
                    .border_r_1()
                    .border_color(p.line)
                    .on_drag(ColumnDrag(col), |_, _, _, cx| cx.new(|_| EmptyView))
                    .on_drag_move(cx.listener(move |w, ev: &DragMoveEvent<ColumnDrag>, _, cx| {
                        let col = ev.drag(cx).0;
                        let left: f32 = w.columns[..col].iter().sum::<f32>() + ev.bounds.left().to_f64() as f32;
                        let x = ev.event.position.x.to_f64() as f32;
                        w.columns[col] = (x - left).clamp(60., 900.);
                        cx.notify();
                    })),
            )
    };
    let header = h_flex()
        .w_full()
        .h(px(HEADER_H))
        .flex_none()
        .bg(p.panel)
        .text_sm()
        .text_color(p.muted)
        .border_b_1()
        .border_color(p.line)
        .child(header_cell("Name", SortKey::Name, cols[0], 0, cx))
        .when(w.column_visible("Size"), |d| d.child(header_cell("Size", SortKey::Size, cols[1], 1, cx)))
        .when(w.column_visible("Kind"), |d| d.child(header_cell("Kind", SortKey::Kind, cols[2], 2, cx)))
        .when(w.column_visible("Modified"), |d| d.child(header_cell("Modified", SortKey::Modified, cols[3], 3, cx)));

    let list = uniform_list(
        ("list", tab.id.0),
        count,
        cx.processor(move |w: &mut Workspace, range: std::ops::Range<usize>, _window, cx| {
            let p = palette(cx);
            let tab = w.panes[ix].tab().clone();
            let rows = w.rows_for(&tab);
            let cols = w.columns;
            let items = display_items(&rows, tab.sort.group_kind);
            items[range.start.min(items.len())..range.end.min(items.len())]
                .iter()
                .map(|item| {
                    let e = match item {
                        DisplayItem::Header(kind) => {
                            return h_flex()
                                .id(SharedString::from(format!("group-{kind}")))
                                .w_full()
                                .h(px(ROW_H))
                                .px_2()
                                .items_end()
                                .pb_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.muted)
                                .border_b_1()
                                .border_color(p.line)
                                .child(*kind)
                                .into_any_element();
                        }
                        DisplayItem::Row(i) => rows[*i],
                    };
                    let sel = tab.selection.contains(&e.path);
                    let cut = w.clip_mode_for(&e.path) == Some(ClipMode::Cut);
                    let path = e.path.clone();
                    let path2 = e.path.clone();
                    let row = h_flex()
                        .id(SharedString::from(e.path.to_string_lossy().into_owned()))
                        .role(Role::Row)
                        .aria_label(a11y_label(w, e))
                        .aria_selected(sel)
                        .w_full()
                        .h(px(ROW_H))
                        .text_sm()
                        .bg(row_bg(sel, &p))
                        .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                        .when(cut, |d| d.opacity(0.5))
                        .border_b_1()
                        .border_color(p.line.opacity(0.4))
                        .child(
                            h_flex()
                                .w(px(cols[0] + 5.))
                                .flex_none()
                                .px_2()
                                .gap_2()
                                .overflow_hidden()
                                .child(entry_icon(e).small().text_color(if e.is_dir() { p.accent } else { p.muted }))
                                .child(div().truncate().child(shown_name(w, e)))
                                .child(tag_dots(e))
                                .children(clip_mark(w, e, &p))
                                .when_some(e.error.clone(), |d, err| d.child(div().text_xs().text_color(p.danger).child(format!("⚠ {err}")))),
                        )
                        .when(w.column_visible("Size"), |d| d.child(div().w(px(cols[1] + 5.)).flex_none().px_2().text_color(p.muted).child(size_text(w, e))))
                        .when(w.column_visible("Kind"), |d| d.child(div().w(px(cols[2] + 5.)).flex_none().px_2().text_color(p.muted).truncate().child(e.kind_label())))
                        .when(w.column_visible("Modified"), |d| d.child(div().w(px(cols[3] + 5.)).flex_none().px_2().text_color(p.muted).child(format_time(e.modified))))
                        .on_click(cx.listener(move |w, ev, window, cx| click_entry(w, ix, &path, ev, window, cx)))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |w, _, window, cx| select_for_context(w, ix, &path2, window, cx)));
                    let row = drag_source(row, w, e);
                    let row = drop_target(row, e, ix, cx);
                    entry_context_menu(row).into_any_element()
                })
                .collect::<Vec<_>>()
        }),
    )
    .track_scroll(&scroll)
    .flex_1();

    v_flex()
        .size_full()
        .child(header)
        .child(if count == 0 { empty_folder(&p).into_any_element() } else { list.into_any_element() })
        .context_menu(|menu, _, _| {
            menu.menu("New Folder", Box::new(NewFolder))
                .menu("New File", Box::new(NewFile))
                .menu("Paste", Box::new(PasteItems))
                .separator()
                .menu("as Icons", Box::new(ViewIcons))
                .menu("as List", Box::new(ViewList))
                .menu("as Columns", Box::new(ViewColumns))
                .separator()
                .menu("Get Info / Folder Size", Box::new(GetInfo))
                .menu("Add to Favorites", Box::new(AddFavorite))
                .menu("Analyze Disk Usage…", Box::new(AnalyzeDiskUsage))
        })
}

#[derive(Clone)]
struct ColumnDrag(usize);

pub enum DisplayItem {
    Header(&'static str),
    Row(usize),
}

/// Rows plus kind section headers when grouping (N02 group).
pub fn display_items(rows: &[&Entry], group: bool) -> Vec<DisplayItem> {
    let mut out = Vec::with_capacity(rows.len() + 8);
    let mut last: Option<&'static str> = None;
    for (i, e) in rows.iter().enumerate() {
        let k = if e.is_dir() { "Folders" } else { e.kind_label() };
        if group && last != Some(k) {
            out.push(DisplayItem::Header(k));
            last = Some(k);
        }
        out.push(DisplayItem::Row(i));
    }
    out
}

/// Row index → index in the displayed list (headers shift it when grouping).
pub fn display_index(rows: &[&Entry], group: bool, row: usize) -> usize {
    display_items(rows, group).iter().position(|d| matches!(d, DisplayItem::Row(i) if *i == row)).unwrap_or(row)
}

pub fn shown_name(w: &Workspace, e: &Entry) -> String {
    if w.prefs.show_extensions || e.is_dir() {
        return e.display_name();
    }
    e.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| e.display_name())
}

fn empty_folder(p: &Palette) -> impl IntoElement {
    div().flex_1().flex().items_center().justify_center().text_color(p.muted).child("No items")
}

/// Folders accept drops of entries (move/copy into that folder).
fn drop_target<E: InteractiveElement>(el: E, e: &Entry, _ix: usize, cx: &mut Context<Workspace>) -> E {
    if !e.is_dir() {
        return el;
    }
    let dest = e.path.clone();
    let dest2 = e.path.clone();
    let p = palette(cx);
    el.drag_over::<DraggedEntries>(move |s, _, _, _| s.bg(p.selection))
        .on_drop(cx.listener(move |w, d: &DraggedEntries, window, cx| {
            cx.stop_propagation();
            w.drop_paths(d.paths.clone(), dest.clone(), window, cx);
        }))
        .on_drop(cx.listener(move |w, d: &ExternalPaths, window, cx| {
            cx.stop_propagation();
            w.drop_paths(d.paths().to_vec(), dest2.clone(), window, cx);
        }))
}

/// Screen reader text: name, kind, size and clipboard state, never color alone (Q01/F01).
fn a11y_label(w: &Workspace, e: &Entry) -> SharedString {
    let mut s = format!("{}, {}, {}", e.display_name(), e.kind_label(), size_text(w, e));
    if !e.tags.is_empty() {
        s.push_str(&format!(", tags {}", e.tags.join(" ")));
    }
    match w.clip_mode_for(&e.path) {
        Some(ClipMode::Cut) => s.push_str(", cut"),
        Some(ClipMode::Copy) => s.push_str(", copied"),
        None => {}
    }
    s.into()
}

pub fn size_text(w: &Workspace, e: &Entry) -> SharedString {
    if e.is_dir() {
        return w.folder_size_label(&e.path);
    }
    e.size.map(format_size).unwrap_or_else(|| "—".into()).into()
}

const TILE_W: f32 = 108.;
const TILE_H: f32 = 96.;

fn render_icons(w: &Workspace, ix: usize, window: &mut Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let tab = w.panes[ix].tab();
    let count = w.rows_for(tab).len();
    // Pane width from the window layout: sidebar and optional preview are fixed widths.
    let vw = window.viewport_size().width.to_f64() as f32;
    let side = theme::SIDEBAR_W + if w.preview_open && vw > theme::NARROW_W { theme::PREVIEW_W } else { 0. };
    let pane_w = ((vw - side) / w.panes.len() as f32 - 24.).max(TILE_W);
    let per_row = ((pane_w / (TILE_W + 8.)) as usize).max(1);
    let row_count = count.div_ceil(per_row);
    let scroll = w.scroll.get(&tab.id).cloned().unwrap_or_default();
    if count == 0 {
        return empty_folder(&p).into_any_element();
    }
    uniform_list(
        ("icons", tab.id.0),
        row_count,
        cx.processor(move |w: &mut Workspace, range: std::ops::Range<usize>, _window, cx| {
            let p = palette(cx);
            let tab = w.panes[ix].tab().clone();
            let rows = w.rows_for(&tab);
            range
                .map(|r| {
                    let items = &rows[(r * per_row).min(rows.len())..((r + 1) * per_row).min(rows.len())];
                    h_flex().id(("icon-row", r)).h(px(TILE_H + 8.)).px_3().gap_2().children(items.iter().map(|e| {
                        let sel = tab.selection.contains(&e.path);
                        let cut = w.clip_mode_for(&e.path) == Some(ClipMode::Cut);
                        let path = e.path.clone();
                        let path2 = e.path.clone();
                        let tile = v_flex()
                            .id(SharedString::from(e.path.to_string_lossy().into_owned()))
                            .role(Role::ListItem)
                            .aria_label(a11y_label(w, e))
                            .aria_selected(sel)
                            .w(px(TILE_W))
                            .h(px(TILE_H))
                            .p_2()
                            .gap_1()
                            .items_center()
                            .rounded_md()
                            .bg(row_bg(sel, &p))
                            .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                            .when(cut, |d| d.opacity(0.5))
                            .child(entry_icon(e).size(px(36.)).text_color(if e.is_dir() { p.accent } else { p.muted }))
                            .child(div().text_xs().text_center().line_clamp(2).child(shown_name(w, e)))
                            .child(h_flex().gap_1().child(tag_dots(e)).children(clip_mark(w, e, &p)))
                            .on_click(cx.listener(move |w, ev, window, cx| click_entry(w, ix, &path, ev, window, cx)))
                            .on_mouse_down(MouseButton::Right, cx.listener(move |w, _, window, cx| select_for_context(w, ix, &path2, window, cx)));
                        let tile = drag_source(tile, w, e);
                        entry_context_menu(drop_target(tile, e, ix, cx))
                    }))
                })
                .collect::<Vec<_>>()
        }),
    )
    .track_scroll(&scroll)
    .size_full()
    .into_any_element()
}

/// Miller columns: ancestors on the left, current folder, then the selected subfolder.
fn render_columns(w: &Workspace, ix: usize, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let tab = w.panes[ix].tab();
    let mut dirs = column_dirs(&tab.location);
    if let Some(c) = tab.cursor.as_ref().filter(|c| tab.selection.len() == 1 && w.cache.get(c).is_some()) {
        dirs.push(c.clone());
    }
    let n = dirs.len();
    h_flex().size_full().children(dirs.into_iter().enumerate().map(|(k, dir)| {
        let is_current = dir == tab.location;
        let entries: Vec<Entry> = w
            .cache
            .get(&dir)
            .map(|l| visible_rows(&l.entries, w.filters, if is_current { &tab.filter } else { "" }, tab.sort).into_iter().cloned().collect())
            .unwrap_or_default();
        // In ancestor columns the child on the path is highlighted.
        let on_path: Option<PathBuf> = tab.location.ancestors().find(|a| a.parent() == Some(dir.as_path())).map(Path::to_path_buf);
        let selection = tab.selection.clone();
        let entries = std::rc::Rc::new(entries);
        let count = entries.len();
        let dir_for_list = dir.clone();
        div()
            .flex_1()
            .min_w(px(160.))
            .h_full()
            .when(k + 1 < n, |d| d.border_r_1().border_color(p.line))
            .child(
                uniform_list(
                    SharedString::from(format!("col-{ix}-{}", dir.display())),
                    count,
                    cx.processor(move |_w: &mut Workspace, range: std::ops::Range<usize>, _window, cx| {
                        let p = palette(cx);
                        entries[range.start.min(count)..range.end.min(count)]
                            .iter()
                            .map(|e| {
                                let sel = if is_current { selection.contains(&e.path) } else { on_path.as_ref() == Some(&e.path) };
                                let path = e.path.clone();
                                let dir = dir_for_list.clone();
                                h_flex()
                                    .id(SharedString::from(e.path.to_string_lossy().into_owned()))
                                    .role(Role::ListItem)
                                    .aria_label(SharedString::from(format!("{}, {}", e.display_name(), e.kind_label())))
                                    .aria_selected(sel)
                                    .h(px(ROW_H - 4.))
                                    .px_2()
                                    .gap_2()
                                    .text_sm()
                                    .bg(row_bg(sel, &p))
                                    .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                                    .child(entry_icon(e).small().text_color(if e.is_dir() { p.accent } else { p.muted }))
                                    .child(div().flex_1().truncate().child(e.display_name()))
                                    .child(tag_dots(e))
                                    .when(e.is_dir(), |d| d.child(icon("chevron-right").xsmall().text_color(p.muted)))
                                    .on_click(cx.listener(move |w, ev: &ClickEvent, window, cx| {
                                        if w.tab().location != dir {
                                            // Clicking in another column moves the current folder there.
                                            w.active_pane = ix;
                                            w.navigate(dir.clone(), cx);
                                        }
                                        click_entry(w, ix, &path, ev, window, cx);
                                    }))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .size_full(),
            )
    }))
}

impl Workspace {
    pub fn ensure_scroll_handles(&mut self) {
        for p in &self.panes {
            for t in &p.tabs {
                self.scroll.entry(t.id).or_default();
            }
        }
        let live: std::collections::HashSet<TabId> = self.panes.iter().flat_map(|p| p.tabs.iter().map(|t| t.id)).collect();
        self.scroll.retain(|id, _| live.contains(id));
    }
}
