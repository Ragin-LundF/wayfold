//! Window chrome: toolbar, sidebar, status bar, optional panels and the overall layout.

use crate::app::actions::*;
use crate::app::state::*;
use crate::ui::browser::{icon, render_pane};
use crate::ui::theme::{self, palette, Palette, NARROW_W, PREVIEW_W, QUEUE_H, SIDEBAR_W, STATUS_H, TERMINAL_H, TOOLBAR_H};
use crate::ui::workspace::Workspace;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use std::path::PathBuf;
use waypoint_core::fs::format_size;

pub fn render_workspace(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    w.ensure_scroll_handles();
    w.reconcile_clipboard();
    let p = palette(cx);
    let narrow = window.viewport_size().width.to_f64() as f32 <= NARROW_W;

    let browsers = h_flex()
        .flex_1()
        .min_h_0()
        .children((0..w.panes.len()).map(|ix| render_pane(w, ix, window, cx).into_any_element()));

    let mut main = v_flex().flex_1().min_w_0().h_full().child(browsers);
    if w.terminal_open {
        if let Some(term) = &w.terminal {
            main = main.child(div().h(px(TERMINAL_H)).flex_none().border_t_1().border_color(p.line).child(term.clone()));
        }
    }
    if w.queue_open {
        main = main.child(crate::ui::transfers::render_queue(w, cx).into_any_element());
    }

    let preview = w.preview_open.then(|| crate::ui::preview::render_preview(w, window, cx).into_any_element());

    let root = v_flex()
        .id("workspace")
        .size_full()
        .bg(p.canvas)
        .text_color(p.text)
        .text_size(px(13.))
        .key_context(WORKSPACE)
        .child(render_toolbar(w, cx))
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .relative()
                .child(render_sidebar(w, narrow, cx))
                .child(main)
                .when_some(preview, |d, pv| {
                    if narrow {
                        // Narrow windows overlay the preview instead of squeezing file columns.
                        d.child(div().absolute().right_0().top_0().bottom_0().w(px(PREVIEW_W)).shadow_lg().child(pv))
                    } else {
                        d.child(div().w(px(PREVIEW_W)).flex_none().h_full().child(pv))
                    }
                }),
        )
        .child(render_status(w, cx));
    let root = w.register_actions(root, cx);
    w.register_file_actions(root, cx)
}

fn tool_button(id: &'static str, ico: &'static str, tip: &'static str) -> Button {
    Button::new(id).ghost().small().icon(icon(ico)).tooltip(tip)
}

fn render_toolbar(w: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let tab = w.tab();
    let (can_back, can_fwd, can_up) = (!tab.back.is_empty(), !tab.forward.is_empty(), tab.location.parent().is_some());
    let view = tab.view;
    let split = w.panes.len() > 1;
    let path_field: AnyElement = if w.path_editing {
        div()
            .key_context(PATH_INPUT)
            .on_action(cx.listener(|w, _: &CompletePath, window, cx| w.complete_path(window, cx)))
            .child(Input::new(&w.path_input).small().cleanable(false).list(&w.path_list))
            .into_any_element()
    } else {
        div()
            .id("path-display")
            .role(Role::Button)
            .aria_label(SharedString::from(format!("Current folder {}, click to edit path", tab.location.display())))
            .flex_1()
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(p.line)
            .bg(p.control)
            .text_sm()
            .truncate()
            .cursor_text()
            .child(SharedString::from(tab.location.display().to_string()))
            .on_click(cx.listener(|w, _, window, cx| w.start_path_edit(window, cx)))
            .into_any_element()
    };
    h_flex()
        .h(px(TOOLBAR_H))
        .flex_none()
        .px_3()
        .gap_1()
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .child(tool_button("back", "chevron-left", "Back (⌘[)").disabled(!can_back).on_click(|_, w, cx| w.dispatch_action(Box::new(GoBack), cx)))
        .child(tool_button("fwd", "chevron-right", "Forward (⌘])").disabled(!can_fwd).on_click(|_, w, cx| w.dispatch_action(Box::new(GoForward), cx)))
        .child(tool_button("up", "arrow-up", "Enclosing folder (⌘↑)").disabled(!can_up).on_click(|_, w, cx| w.dispatch_action(Box::new(GoUp), cx)))
        .child(div().flex_1().min_w(px(90.)).mx_2().child(path_field))
        .child(Button::new("search").ghost().small().icon(icon("search")).label("Search").tooltip("Search (⌘F)").on_click(|_, w, cx| w.dispatch_action(Box::new(Search), cx)))
        .child(
            Button::new("view")
                .ghost()
                .small()
                .icon(icon(match view {
                    ViewMode::List => "list",
                    ViewMode::Icons => "layout-grid",
                    ViewMode::Columns => "columns-3",
                }))
                .label("View")
                .tooltip("View options")
                .dropdown_menu({
                    let f = w.filters;
                    let group = tab.sort.group_kind;
                    let ext = w.prefs.show_extensions;
                    let (cs, ck, cm) = (w.column_visible("Size"), w.column_visible("Kind"), w.column_visible("Modified"));
                    move |menu, _, _| {
                        menu.menu_with_check("as Icons  ⌘1", view == ViewMode::Icons, Box::new(ViewIcons))
                            .menu_with_check("as List  ⌘2", view == ViewMode::List, Box::new(ViewList))
                            .menu_with_check("as Columns  ⌘3", view == ViewMode::Columns, Box::new(ViewColumns))
                            .separator()
                            .menu("Sort by Name", Box::new(SortByName))
                            .menu("Sort by Size", Box::new(SortBySize))
                            .menu("Sort by Kind", Box::new(SortByKind))
                            .menu("Sort by Date Modified", Box::new(SortByModified))
                            .menu_with_check("Group by kind", group, Box::new(ToggleGroupByKind))
                            .separator()
                            .menu_with_check("Hidden files", f.show_hidden, Box::new(ToggleHidden))
                            .menu_with_check("System files", f.show_system, Box::new(ToggleSystem))
                            .menu_with_check("File extensions", ext, Box::new(ToggleExtensions))
                            .separator()
                            .menu_with_check("Size column", cs, Box::new(ToggleSizeColumn))
                            .menu_with_check("Kind column", ck, Box::new(ToggleKindColumn))
                            .menu_with_check("Modified column", cm, Box::new(ToggleModifiedColumn))
                    }
                }),
        )
        .child(
            Button::new("split")
                .ghost()
                .small()
                .icon(icon("columns-2"))
                .label("Split")
                .selected(split)
                .tooltip("Split view (⌘⇧D)")
                .on_click(|_, w, cx| w.dispatch_action(Box::new(ToggleSplit), cx)),
        )
        .child(tool_button("more", "ellipsis", "More commands (⌘K)").dropdown_menu(|menu, _, _| {
            menu.menu("Command Palette…", Box::new(OpenCommandPalette))
                .separator()
                .menu("Add to Favorites", Box::new(AddFavorite))
                .menu("Tags…", Box::new(EditTags))
                .separator()
                .menu("Copy", Box::new(CopyItems))
                .menu("Cut", Box::new(CutItems))
                .menu("Paste", Box::new(PasteItems))
                .separator()
                .menu("Batch Rename…", Box::new(BatchRename))
                .menu("Open With…", Box::new(OpenWith))
                .menu("Preview", Box::new(TogglePreview))
                .separator()
                .menu("Compare Folders…", Box::new(CompareFolders))
                .menu("Analyze Disk Usage…", Box::new(AnalyzeDiskUsage))
                .menu("Create Archive…", Box::new(CreateArchive))
                .menu("Extract Archive…", Box::new(ExtractArchive))
                .separator()
                .menu("Connect to Server…", Box::new(ConnectToServer))
        }))
}

fn sidebar_heading(p: &Palette, label: &'static str) -> impl IntoElement {
    div().mt_3().mb_1().px_2().text_xs().text_color(p.muted).child(label)
}

fn sidebar_item(id: SharedString, ico: Icon, label: SharedString, on: bool, p: &Palette) -> Stateful<Div> {
    h_flex()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_selected(on)
        .h(px(28.))
        .px_2()
        .gap_2()
        .rounded_md()
        .text_sm()
        .when(on, |d| d.bg(p.selection))
        .when(!on, |d| d.hover(|s| s.bg(p.hover)))
        .child(ico.small().text_color(p.muted))
        .child(div().flex_1().truncate().child(label))
}

use gpui_kit::component::Icon;

fn render_sidebar(w: &Workspace, narrow: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let loc = w.tab().location.clone();
    let width = if narrow { 140. } else { SIDEBAR_W };
    let favorites = w.prefs.favorites.clone();
    let recents: Vec<PathBuf> = w.prefs.recents.iter().filter(|r| !favorites.contains(r)).take(5).cloned().collect();

    let nav = |path: PathBuf| {
        move |w: &mut Workspace, _: &ClickEvent, window: &mut Window, cx: &mut Context<Workspace>| {
            w.navigate(path.clone(), cx);
            w.focus_browser(window, cx);
        }
    };

    v_flex()
        .id("sidebar")
        .w(px(width))
        .flex_none()
        .h_full()
        .px_2()
        .py_1()
        .bg(p.panel)
        .border_r_1()
        .border_color(p.line)
        .overflow_y_scroll()
        .child(sidebar_heading(&p, "FAVORITES"))
        .children(favorites.into_iter().map(|f| {
            let missing = !f.exists();
            let label: SharedString = if f == w.home { "Home".into() } else { display_name(&f).into() };
            let remove = f.clone();
            sidebar_item(format!("fav-{}", f.display()).into(), icon(if f == w.home { "house" } else { "folder" }), label.clone(), loc == f, &p)
                .when(missing, |d| d.opacity(0.5))
                .on_click(cx.listener(nav(f.clone())))
                .child(
                    div()
                        .id(SharedString::from(format!("fav-rm-{}", remove.display())))
                        .role(Role::Button)
                        .aria_label(SharedString::from(format!("Remove {} from Favorites", display_name(&remove))))
                        .px_1()
                        .rounded_sm()
                        .text_color(p.muted)
                        .hover(|s| s.bg(p.line).text_color(p.text))
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(format!("Remove {label} from Favorites (folder is kept)")).build(window, cx))
                        .child(icon("x").xsmall())
                        .on_click(cx.listener(move |w, _, _, cx| {
                            cx.stop_propagation();
                            w.remove_favorite(&remove, cx);
                        })),
                )
        }))
        .child(
            sidebar_item("fav-add".into(), icon("plus"), "Add favorite".into(), false, &p)
                .text_color(p.muted)
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Add the selected or current folder").build(window, cx))
                .on_click(cx.listener(|w, _, _, cx| w.add_favorite(cx))),
        )
        .when(!recents.is_empty(), |d| {
            d.child(sidebar_heading(&p, "RECENT")).children(recents.into_iter().map(|r| {
                sidebar_item(format!("recent-{}", r.display()).into(), icon("clock"), display_name(&r).into(), loc == r, &p)
                    .on_click(cx.listener(nav(r.clone())))
            }))
        })
        .child(sidebar_heading(&p, "TAGS"))
        .children(theme::TAGS.iter().map(|(name, color)| {
            let on = w.tag_filter.as_deref() == Some(*name);
            let name = *name;
            h_flex()
                .id(SharedString::from(format!("tag-{name}")))
                .role(Role::Button)
                .aria_label(SharedString::from(format!("Filter by tag {name}")))
                .aria_selected(on)
                .h(px(28.))
                .px_2()
                .gap_2()
                .rounded_md()
                .text_sm()
                .when(on, |d| d.bg(p.selection))
                .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                .child(div().size(px(10.)).rounded_full().bg(rgb(*color)))
                .child(name)
                .on_click(cx.listener(move |w, _, _, cx| w.toggle_tag_filter(name, cx)))
        }))
        .child(sidebar_heading(&p, "LOCATIONS"))
        .children(w.volumes.iter().filter(|m| m.local).map(|m| {
            sidebar_item(format!("vol-{}", m.path.display()).into(), icon("hard-drive"), m.name().into(), loc == m.path, &p)
                .on_click(cx.listener(nav(m.path.clone())))
        }))
        .child(sidebar_heading(&p, "NETWORK"))
        .children(w.volumes.iter().filter(|m| !m.local).map(|m| {
            sidebar_item(format!("net-{}", m.path.display()).into(), icon("network"), m.name().into(), loc == m.path, &p)
                .on_click(cx.listener(nav(m.path.clone())))
        }))
        .child(
            sidebar_item("connect".into(), icon("plus"), "Connect to server…".into(), false, &p)
                .text_color(p.muted)
                .on_click(|_, w, cx| w.dispatch_action(Box::new(ConnectToServer), cx)),
        )
}

fn render_status(w: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let rows = w.rows();
    let sel = w.tab().selected(&rows);
    let sel_bytes: u64 = sel.iter().filter_map(|e| e.size).sum();
    let mut info = format!("{} items", rows.len());
    if !sel.is_empty() {
        info.push_str(&format!(" · {} selected", sel.len()));
        if sel_bytes > 0 {
            info.push_str(&format!(" · {}", format_size(sel_bytes)));
        }
    }
    let clip: SharedString = match &w.clipboard {
        Some(c) => format!("{} {} · clear", if c.mode == crate::ui::clipboard::ClipMode::Cut { "✂ Cut" } else { "⧉ Copied" }, c.sources.len()).into(),
        None => "Clipboard empty".into(),
    };
    let active = w.active_transfer_count();
    h_flex()
        .h(px(STATUS_H + 4.))
        .flex_none()
        .px_3()
        .gap_2()
        .text_xs()
        .bg(p.panel)
        .border_t_1()
        .border_color(p.line)
        .child(div().text_color(p.muted).child(SharedString::from(info)))
        .when_some(w.status.clone(), |d, s| d.child(div().ml_3().text_color(p.text).truncate().child(s)))
        .child(div().flex_1())
        .child(
            Button::new("clip")
                .ghost()
                .xsmall()
                .label(clip)
                .disabled(w.clipboard.is_none())
                .tooltip("Clear clipboard marks")
                .on_click(cx.listener(|w, _, _, cx| w.clear_clipboard(cx))),
        )
        .child(
            Button::new("queue")
                .ghost()
                .xsmall()
                .selected(w.queue_open)
                .label(SharedString::from(if active > 0 { format!("Transfers {active}") } else { "Transfers".into() }))
                .on_click(|_, win, cx| win.dispatch_action(Box::new(ToggleQueue), cx)),
        )
        .child(
            Button::new("term")
                .ghost()
                .xsmall()
                .selected(w.terminal_open)
                .icon(icon("square-terminal"))
                .label("Terminal")
                .tooltip("Toggle terminal (F4)")
                .on_click(|_, win, cx| win.dispatch_action(Box::new(ToggleTerminal), cx)),
        )
}

pub const QUEUE_HEIGHT: f32 = QUEUE_H;
