//! Disk usage dialog (P04): scan progress + Cancel, squarified treemap of the current node
//! (plain absolutely positioned divs), breadcrumb drill-down, sortable list of children and
//! a logical/allocated toggle. The scan runs on the pools; its tree lives only in this view.

use crate::ui::browser::icon;
use crate::ui::fileops::{Segment, footer, segmented};
use crate::ui::theme::{Palette, palette};
use crate::ui::tools::table_cell;
use crate::ui::workspace::CancelGuard;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;
use wayfold_core::diskusage::{self, UsageEvent, UsageNode, UsageReport, squarify};
use wayfold_core::fs::format_size;

const MAP_W: f32 = 860.;
const MAP_H: f32 = 260.;
const ROW_H: f32 = 28.;
const SIZE_W: Option<f32> = Some(110.);
const ITEMS_W: Option<f32> = Some(90.);
const SIZE_MODES: [Segment<bool>; 2] = [("du-alloc", "On disk (allocated)", true), ("du-logical", "Logical size", false)];
/// Treemap tiles drawn per level; the rest is too small to see.
const MAX_TILES: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Logical,
    Allocated,
    Items,
}

pub struct DiskUsageView {
    root: PathBuf,
    report: Option<UsageReport>,
    progress: (u64, u64),
    /// Child indices from the root to the shown node.
    drill: Vec<usize>,
    pub(crate) allocated: bool,
    sort: SortKey,
    scan: (CancelGuard, Task<()>),
    pub(crate) running: bool,
}

impl DiskUsageView {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let handle = diskusage::start_scan(root.clone());
        let events = handle.events.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                // Only the newest progress matters; a Done ends the stream.
                let mut last = ev;
                while let Ok(e) = events.try_recv() {
                    last = e;
                }
                let alive = this.update(cx, |v, cx| {
                    match last {
                        UsageEvent::Progress { items, allocated } => v.progress = (items, allocated),
                        UsageEvent::Done(report) => {
                            v.report = Some(*report);
                            v.running = false;
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        Self {
            root,
            report: None,
            progress: (0, 0),
            drill: Vec::new(),
            allocated: true,
            sort: SortKey::Allocated,
            scan: (CancelGuard(handle.cancel), task),
            running: true,
        }
    }

    fn node(&self) -> Option<&UsageNode> {
        let mut n = &self.report.as_ref()?.root;
        for &i in &self.drill {
            n = n.children.get(i)?;
        }
        Some(n)
    }

    fn size(&self, n: &UsageNode) -> u64 {
        if self.allocated { n.allocated } else { n.logical }
    }

    /// Child indices of the shown node in list order.
    fn sorted_children(&self) -> Vec<usize> {
        let Some(n) = self.node() else { return vec![] };
        let mut ix: Vec<usize> = (0..n.children.len()).collect();
        let c = &n.children;
        match self.sort {
            SortKey::Name => ix.sort_by(|&a, &b| c[a].name.cmp(&c[b].name)),
            SortKey::Logical => ix.sort_by(|&a, &b| c[b].logical.cmp(&c[a].logical)),
            SortKey::Allocated => ix.sort_by(|&a, &b| c[b].allocated.cmp(&c[a].allocated)),
            SortKey::Items => ix.sort_by(|&a, &b| c[b].items.cmp(&c[a].items)),
        }
        ix
    }

    fn open_child(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.node().and_then(|n| n.children.get(ix)).is_some_and(|c| c.is_dir && !c.children.is_empty()) {
            self.drill.push(ix);
            cx.notify();
        }
    }

    fn render_treemap(&self, node: &UsageNode, p: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut order: Vec<usize> = (0..node.children.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.size(&node.children[i])));
        order.truncate(MAX_TILES);
        let sizes: Vec<u64> = order.iter().map(|&i| self.size(&node.children[i])).collect();
        let rects = squarify(&sizes, MAP_W, MAP_H);
        div().relative().w(px(MAP_W)).h(px(MAP_H)).bg(p.panel).rounded_md().overflow_hidden().children(
            order.iter().zip(rects).enumerate().filter(|(_, (_, r))| r.2 >= 2. && r.3 >= 2.).map(|(rank, (&ix, (x, y, w, h)))| {
                let c = &node.children[ix];
                // Palette-derived tile colors: folders in accent hues, files in muted ones.
                let base = if c.is_dir { p.accent } else { p.muted };
                let tint = Hsla { h: (base.h + rank as f32 * 0.045) % 1.0, ..base }.opacity(0.35);
                let label = format!("{} · {}", c.name.to_string_lossy(), format_size(self.size(c)));
                div()
                    .id(("tile", ix))
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .bg(tint)
                    .border_1()
                    .border_color(p.canvas)
                    .overflow_hidden()
                    .text_xs()
                    .text_color(p.text)
                    .when(c.is_dir, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)))
                    .when(w > 70. && h > 18., |d| d.child(div().px_1().truncate().child(label)))
                    .on_click(cx.listener(move |v, _, _, cx| v.open_child(ix, cx)))
            }),
        )
    }

    fn render_breadcrumb(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut crumbs =
            vec![self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.root.display().to_string())];
        if let Some(report) = &self.report {
            let mut n = &report.root;
            for &i in &self.drill {
                n = &n.children[i];
                crumbs.push(n.name.to_string_lossy().into_owned());
            }
        }
        h_flex()
            .gap_1()
            .text_sm()
            .child(Button::new("du-up").xsmall().icon(icon("arrow-up")).tooltip("Up one level").on_click(cx.listener(|v, _, _, cx| {
                v.drill.pop();
                cx.notify();
            })))
            .children(crumbs.into_iter().enumerate().map(|(depth, name)| {
                h_flex().gap_1().when(depth > 0, |d| d.child(div().text_color(p.muted).child("/"))).child(
                    div().id(("crumb", depth)).cursor_pointer().hover(|s| s.text_color(p.accent)).child(name).on_click(cx.listener(
                        move |v, _, _, cx| {
                            v.drill.truncate(depth);
                            cx.notify();
                        },
                    )),
                )
            }))
    }

    fn render_list(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let header = |id: &'static str, label: &'static str, key: SortKey, width: Option<f32>, cx: &mut Context<Self>| {
            table_cell(width)
                .id(id)
                .debug_selector(|| id.into())
                .cursor_pointer()
                .when(self.sort == key, |d| d.text_color(p.accent))
                .child(label)
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.sort = key;
                    cx.notify();
                }))
        };
        let order = self.sorted_children();
        let count = order.len();
        let list = uniform_list(
            "du-rows",
            count,
            cx.processor(move |v: &mut DiskUsageView, range: std::ops::Range<usize>, _, cx| {
                let p = palette(cx);
                let Some(node) = v.node() else { return vec![] };
                range
                    .filter_map(|i| order.get(i).copied())
                    .map(|ix| {
                        let c = &node.children[ix];
                        let name = c.name.to_string_lossy().into_owned();
                        h_flex()
                            .id(("du-row", ix))
                            .w_full()
                            .h(px(ROW_H))
                            .px_3()
                            .gap_2()
                            .text_sm()
                            .border_b_1()
                            .border_color(p.line.opacity(0.5))
                            .when(c.is_dir, |d| d.cursor_pointer().hover(|s| s.bg(p.hover)))
                            .child(
                                table_cell(None)
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .debug_selector(move || format!("du-name-{ix}"))
                                    .child(icon(if c.is_dir { "folder" } else { "file" }).small())
                                    .child(div().min_w_0().truncate().child(name)),
                            )
                            .child(table_cell(SIZE_W).child(format_size(c.logical)))
                            .child(table_cell(SIZE_W).child(format_size(c.allocated)))
                            .child(
                                table_cell(ITEMS_W)
                                    .debug_selector(move || format!("du-items-{ix}"))
                                    .text_color(p.muted)
                                    .child(c.items.to_string()),
                            )
                            .on_click(cx.listener(move |v, _, _, cx| v.open_child(ix, cx)))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .h_full();
        v_flex()
            .border_1()
            .border_color(p.line)
            .rounded_md()
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .bg(p.panel)
                    .text_xs()
                    .text_color(p.muted)
                    .child(header("du-h-name", "Name", SortKey::Name, None, cx))
                    .child(header("du-h-log", "Logical", SortKey::Logical, SIZE_W, cx))
                    .child(header("du-h-alloc", "On disk", SortKey::Allocated, SIZE_W, cx))
                    .child(header("du-h-items", "Items", SortKey::Items, ITEMS_W, cx)),
            )
            .child(div().h(px(220.)).child(list))
    }
}

impl Render for DiskUsageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let size_mode = segmented(
            "du-size-mode",
            &SIZE_MODES,
            self.allocated,
            false,
            cx,
            cx.listener(|v, on: &bool, _, cx| {
                v.allocated = *on;
                cx.notify();
            }),
        );
        let caption = match self.allocated {
            true => "Space the items really use on the volume (whole blocks, after compression).",
            false => "Sum of the file sizes as apps report them.",
        };
        let body = match (self.running, self.node()) {
            (true, _) => v_flex()
                .gap_2()
                .child(div().text_sm().child(format!(
                    "Scanning {} … {} items, {} on disk",
                    self.root.display(),
                    self.progress.0,
                    format_size(self.progress.1)
                )))
                .child(Progress::new("du-progress").loading(true))
                .into_any_element(),
            (false, Some(node)) => v_flex()
                .gap_2()
                .child(self.render_breadcrumb(p, cx))
                .child(div().text_sm().text_color(p.muted).child(format!(
                    "{} logical · {} on disk · {} items",
                    format_size(node.logical),
                    format_size(node.allocated),
                    node.items
                )))
                .child(self.render_treemap(node, p, cx))
                .child(self.render_list(p, cx))
                .into_any_element(),
            (false, None) => div().child("No data").into_any_element(),
        };
        let report = self.report.as_ref();
        let cancelled = report.is_some_and(|r| r.cancelled);
        let errors: Vec<String> =
            report.map(|r| r.errors.iter().take(5).map(|(path, e)| format!("{}: {e}", path.display())).collect()).unwrap_or_default();
        let error_count = report.map_or(0, |r| r.error_count);
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(size_mode)
                    .child(div().flex_1().min_w_0().text_xs().text_color(p.muted).child(caption))
                    .when(self.running, |d| {
                        d.child(Button::new("du-cancel").label("Stop").on_click(cx.listener(|v, _, _, cx| {
                            v.scan.0.0.cancel();
                            cx.notify();
                        })))
                    }),
            )
            .child(body)
            .when(cancelled, |d| d.child(div().text_sm().text_color(p.warning).child("⚠ Scan stopped: totals are incomplete")))
            .when(error_count > 0, |d| {
                d.child(
                    v_flex()
                        .text_sm()
                        .text_color(p.warning)
                        .child(format!("⚠ {error_count} item(s) could not be read and are not counted"))
                        .children(errors.into_iter().map(|e| div().text_xs().text_color(p.muted).truncate().child(e))),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted)
                    .child("Hard links count once; symbolic links are not followed; other volumes are skipped."),
            )
            .child(footer(cx).child(Button::new("du-close").label("Close").on_click(|_, window, cx| window.close_dialog(cx))))
    }
}
