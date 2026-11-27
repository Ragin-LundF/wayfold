//! Compare folders dialog (F07): two paths, mode, streamed virtualized diff table with a
//! direction per row, plan preview and apply through the transfer queue (which revalidates
//! and asks on conflicts). Nothing is ever deleted.

use crate::ui::browser::icon;
use crate::ui::fileops::{Segment, field, footer, segmented};
use crate::ui::theme::{Palette, palette};
use crate::ui::tools::table_cell;
use crate::ui::workspace::{CancelGuard, Workspace};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::path::PathBuf;
use wayfold_core::compare::{self, CompareEvent, CompareMode, CopyStep, DiffRow, DiffStatus, Direction};
use wayfold_core::fs::format_size;
use wayfold_core::ops::TransferMode;

const ROW_H: f32 = 30.;
const STATUS_W: Option<f32> = Some(150.);
const SIDE_W: Option<f32> = Some(80.);
const COPY_W: Option<f32> = Some(150.);
const MODES: [Segment<CompareMode>; 2] =
    [("mode-st", "Size + date", CompareMode::SizeTime), ("mode-cs", "Checksum", CompareMode::Checksum)];

pub struct CompareView {
    workspace: WeakEntity<Workspace>,
    left: Entity<InputState>,
    right: Entity<InputState>,
    pub(crate) mode: CompareMode,
    pub(crate) rows: Vec<(DiffRow, Direction)>,
    hide_same: bool,
    /// Indices into `rows` after the filter.
    visible: Vec<usize>,
    /// Roots of the scan that produced `rows`.
    roots: Option<(PathBuf, PathBuf)>,
    scan: Option<(CancelGuard, Task<()>)>,
    pub(crate) running: bool,
    cancelled: bool,
    show_plan: bool,
}

impl CompareView {
    pub fn new(left: PathBuf, right: PathBuf, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |p: PathBuf, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).default_value(p.to_string_lossy().into_owned()))
        };
        Self {
            workspace,
            left: input(left, window, cx),
            right: input(right, window, cx),
            mode: CompareMode::SizeTime,
            rows: Vec::new(),
            hide_same: true,
            visible: Vec::new(),
            roots: None,
            scan: None,
            running: false,
            cancelled: false,
            show_plan: false,
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let path = |i: &Entity<InputState>| PathBuf::from(i.read(cx).value().trim().to_string());
        let (left, right) = (path(&self.left), path(&self.right));
        self.rows.clear();
        self.visible.clear();
        self.cancelled = false;
        self.show_plan = false;
        self.roots = Some((left.clone(), right.clone()));
        let handle = compare::start_compare(left, right, self.mode);
        let events = handle.events.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                let mut batch = vec![ev];
                while let Ok(e) = events.try_recv() {
                    batch.push(e);
                }
                let alive = this.update(cx, |v, cx| {
                    for ev in batch {
                        v.apply(ev);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        self.scan = Some((CancelGuard(handle.cancel), task));
        self.running = true;
        cx.notify();
    }

    fn apply(&mut self, ev: CompareEvent) {
        match ev {
            CompareEvent::Rows(rows) => {
                for row in rows {
                    if !(self.hide_same && row.status == DiffStatus::Same) {
                        self.visible.push(self.rows.len());
                    }
                    let d = row.default_direction();
                    self.rows.push((row, d));
                }
            }
            CompareEvent::Done { cancelled } => {
                self.cancelled = cancelled;
                self.running = false;
            }
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some((guard, _)) = &self.scan {
            guard.0.cancel();
        }
        cx.notify();
    }

    fn set_hide_same(&mut self, hide: bool, cx: &mut Context<Self>) {
        self.hide_same = hide;
        self.visible = (0..self.rows.len()).filter(|&i| !(hide && self.rows[i].0.status == DiffStatus::Same)).collect();
        cx.notify();
    }

    fn plan(&self) -> Vec<CopyStep> {
        match &self.roots {
            Some((l, r)) => compare::copy_plan(l, r, &self.rows),
            None => Vec::new(),
        }
    }

    fn apply_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plan = self.plan();
        if plan.is_empty() {
            return;
        }
        window.close_dialog(cx);
        self.workspace
            .update(cx, |w, cx| {
                for step in plan {
                    w.start_transfer(TransferMode::Copy, step.sources, step.dest_dir, window, cx);
                }
            })
            .ok();
    }

    fn render_row(&self, ix: usize, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let (row, dir) = &self.rows[ix];
        let (label, icon_name, color) = status_style(&row.status, p);
        let side = |s: &Option<compare::Side>| match s {
            None => "—".to_string(),
            Some(s) if s.is_dir => "Folder".to_string(),
            Some(s) => s.size.map(format_size).unwrap_or_else(|| "?".into()),
        };
        let dir_button = |id: &'static str, label: &'static str, d: Direction, cx: &mut Context<Self>| {
            Button::new((id, ix)).xsmall().label(label).selected(*dir == d).disabled(!row.allows(d)).on_click(cx.listener(
                move |v, _, _, cx| {
                    v.rows[ix].1 = d;
                    cx.notify();
                },
            ))
        };
        h_flex()
            .id(("cmp-row", ix))
            .w_full()
            .h(px(ROW_H))
            .px_3()
            .gap_2()
            .text_sm()
            .border_b_1()
            .border_color(p.line.opacity(0.5))
            .child(table_cell(STATUS_W).flex().items_center().gap_1().text_color(color).child(icon(icon_name).small()).child(label))
            .child(table_cell(None).truncate().debug_selector(move || format!("cmp-path-{ix}")).child(if row.rel.as_os_str().is_empty() {
                "(compared folder)".to_string()
            } else {
                row.rel.to_string_lossy().into_owned()
            }))
            .child(table_cell(SIDE_W).text_color(p.muted).child(side(&row.left)))
            .child(table_cell(SIDE_W).text_color(p.muted).child(side(&row.right)))
            .child(
                table_cell(COPY_W)
                    .flex()
                    .gap_1()
                    .debug_selector(move || format!("cmp-copy-{ix}"))
                    .child(dir_button("to-right", "→", Direction::LeftToRight, cx))
                    .child(dir_button("to-left", "←", Direction::RightToLeft, cx))
                    .child(dir_button("skip", "Skip", Direction::Skip, cx)),
            )
            .into_any_element()
    }

    fn render_plan(&self, p: Palette) -> impl IntoElement {
        let plan = self.plan();
        v_flex()
            .id("cmp-plan")
            .max_h(px(200.))
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .border_1()
            .border_color(p.line)
            .rounded_md()
            .text_sm()
            .when(plan.is_empty(), |d| d.child(div().text_color(p.muted).child("Nothing to copy: choose → or ← on rows.")))
            .children(plan.iter().take(200).map(|s| {
                let names: Vec<String> =
                    s.sources.iter().take(6).map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
                let more = s.sources.len().saturating_sub(names.len());
                let names = if more > 0 { format!("{} and {more} more", names.join(", ")) } else { names.join(", ") };
                div().child(format!("Copy {} item(s) into {}: {names}", s.sources.len(), s.dest_dir.display()))
            }))
            .child(div().text_color(p.muted).child(
                "Existing items are never overwritten silently: the transfer asks to keep both, replace (old item to Trash) or skip.",
            ))
    }
}

fn status_style(s: &DiffStatus, p: Palette) -> (SharedString, &'static str, Hsla) {
    match s {
        DiffStatus::LeftOnly => ("Only left".into(), "arrow-left", p.accent),
        DiffStatus::RightOnly => ("Only right".into(), "arrow-right", p.accent),
        DiffStatus::Same => ("Same".into(), "equal", p.muted),
        DiffStatus::Different => ("Different".into(), "equal-not", p.warning),
        DiffStatus::Unreadable(_) => ("Unreadable".into(), "circle-alert", p.danger),
        DiffStatus::ChangedDuringScan => ("Changed".into(), "clock-alert", p.danger),
    }
}

impl Render for CompareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let count = |f: fn(&DiffStatus) -> bool| self.rows.iter().filter(|(r, _)| f(&r.status)).count();
        let summary = format!(
            "{} differ · {} only left · {} only right · {} same · {} unreadable or changed",
            count(|s| *s == DiffStatus::Different),
            count(|s| *s == DiffStatus::LeftOnly),
            count(|s| *s == DiffStatus::RightOnly),
            count(|s| *s == DiffStatus::Same),
            count(|s| matches!(s, DiffStatus::Unreadable(_) | DiffStatus::ChangedDuringScan)),
        );
        let running = self.running;
        let modes = segmented(
            "cmp-mode",
            &MODES,
            self.mode,
            running,
            cx,
            cx.listener(|v, m: &CompareMode, _, cx| {
                v.mode = *m;
                cx.notify();
            }),
        );
        let planned = self.rows.iter().filter(|(r, d)| *d != Direction::Skip && r.allows(*d)).count();
        let list = uniform_list(
            "cmp-rows",
            self.visible.len(),
            cx.processor(move |v: &mut CompareView, range: std::ops::Range<usize>, _, cx| {
                let p = palette(cx);
                range.filter_map(|i| v.visible.get(i).copied()).map(|ix| v.render_row(ix, p, cx)).collect()
            }),
        )
        .h_full();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Left folder", Input::new(&self.left))))
                    .child(div().flex_1().child(field("Right folder", Input::new(&self.right)))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(modes)
                    .child(div().flex_1())
                    .child(
                        Checkbox::new("hide-same")
                            .label("Hide equal items")
                            .checked(self.hide_same)
                            .on_click(cx.listener(|v, c: &bool, _, cx| v.set_hide_same(*c, cx))),
                    )
                    .child(match running {
                        true => Button::new("cmp-cancel").label("Stop").on_click(cx.listener(|v, _, _, cx| v.cancel(cx))),
                        false => Button::new("cmp-start").primary().label("Compare").on_click(cx.listener(|v, _, _, cx| v.start(cx))),
                    }),
            )
            .when(running, |d| d.child(Progress::new("cmp-progress").loading(true)))
            .child(
                h_flex()
                    .text_sm()
                    .text_color(p.muted)
                    .child(summary)
                    .when(self.cancelled, |d| d.child(div().ml_2().text_color(p.warning).child("· Stopped: results are incomplete"))),
            )
            .child(
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
                            .child(table_cell(STATUS_W).child("Status"))
                            .child(table_cell(None).debug_selector(|| "cmp-h-path".into()).child("Path"))
                            .child(table_cell(SIDE_W).child("Left"))
                            .child(table_cell(SIDE_W).child("Right"))
                            .child(table_cell(COPY_W).debug_selector(|| "cmp-h-copy".into()).child("Copy")),
                    )
                    .child(div().h(px(320.)).child(list)),
            )
            .when(self.show_plan, |d| d.child(self.render_plan(p)))
            .child(
                footer(cx)
                    .child(Button::new("cmp-close").label("Close").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(
                        Button::new("cmp-preview")
                            .label(if self.show_plan { "Hide plan" } else { "Preview plan" })
                            .disabled(running || self.rows.is_empty())
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.show_plan = !v.show_plan;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cmp-apply")
                            .primary()
                            .label(format!("Copy {planned} item(s)"))
                            .disabled(running || planned == 0 || !self.show_plan)
                            .on_click(cx.listener(|v, _, window, cx| v.apply_plan(window, cx))),
                    ),
            )
    }
}
