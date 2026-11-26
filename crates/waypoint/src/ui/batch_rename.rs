//! Batch rename modal (F04): editable rules, live before/after grid, Apply disabled while any
//! row has a problem. Planning and applying run on the worker pool; undo is recorded.

use crate::ui::fileops::{field, footer};
use crate::ui::theme::palette;
use crate::ui::workspace::Workspace;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex, WindowExt as _};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use std::path::PathBuf;
use waypoint_core::ops::{self, CaseRule, ItemOutcome, RenameProblem, RenameRow, RenameRules, Sequence};
use waypoint_core::pool;

pub struct BatchRenameView {
    paths: Vec<PathBuf>,
    workspace: WeakEntity<Workspace>,
    find: Entity<InputState>,
    replace: Entity<InputState>,
    prefix: Entity<InputState>,
    suffix: Entity<InputState>,
    start: Entity<InputState>,
    regex: bool,
    sequence: bool,
    case: Option<CaseRule>,
    rows: Vec<RenameRow>,
    error: Option<String>,
    planning: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl BatchRenameView {
    pub fn new(paths: Vec<PathBuf>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mk = |ph: &str, v: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (ph, v) = (ph.to_string(), v.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
        };
        let find = mk("Find (text or regex)", "", window, cx);
        let replace = mk("Replace with", "", window, cx);
        let prefix = mk("Prefix", "", window, cx);
        let suffix = mk("Suffix (before extension)", "", window, cx);
        let start = mk("Start", "1", window, cx);
        let subs = [&find, &replace, &prefix, &suffix, &start]
            .into_iter()
            .map(|i| {
                cx.subscribe(i, |this: &mut Self, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        this.replan(cx);
                    }
                })
            })
            .collect();
        let mut this = Self {
            paths,
            workspace,
            find,
            replace,
            prefix,
            suffix,
            start,
            regex: false,
            sequence: false,
            case: None,
            rows: vec![],
            error: None,
            planning: None,
            _subscriptions: subs,
        };
        this.replan(cx);
        this
    }

    fn rules(&self, cx: &App) -> RenameRules {
        let v = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let start_text = v(&self.start);
        RenameRules {
            find: v(&self.find),
            replace: v(&self.replace),
            regex: self.regex,
            prefix: v(&self.prefix),
            suffix: v(&self.suffix),
            sequence: self.sequence.then(|| Sequence {
                start: start_text.trim().parse().unwrap_or(1),
                step: 1,
                padding: start_text.trim().len().max(2),
            }),
            case: self.case,
        }
    }

    fn replan(&mut self, cx: &mut Context<Self>) {
        let rules = self.rules(cx);
        let paths = self.paths.clone();
        let fut = pool::local().run(move || ops::plan_batch_rename(&paths, &rules).map_err(|e| e.to_string()));
        // Replacing the task drops the previous plan, so an older plan never overwrites a newer one.
        self.planning = Some(cx.spawn(async move |this, cx| {
            let Some(r) = fut.await else { return };
            this.update(cx, |v, cx| {
                match r {
                    Ok(rows) => {
                        v.rows = rows;
                        v.error = None;
                    }
                    Err(e) => v.error = Some(format!("Invalid regular expression: {e}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn can_apply(&self) -> bool {
        self.error.is_none() && !self.rows.is_empty() && self.rows.iter().all(|r| r.problem.is_none()) && self.rows.iter().any(|r| r.changes())
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_apply() {
            return;
        }
        let rows = self.rows.clone();
        let ws = self.workspace.clone();
        let dir = self.paths[0].parent().map(PathBuf::from).unwrap_or_default();
        window.close_dialog(cx);
        let fut = pool::for_path(&dir).run(move || ops::apply_batch_rename(&rows));
        cx.spawn(async move |_, cx| {
            let Some(r) = fut.await else { return };
            ws.update(cx, |w, cx| {
                match r {
                    Ok((results, undo)) => {
                        let failed: Vec<String> = results
                            .iter()
                            .filter_map(|r| match &r.outcome {
                                ItemOutcome::Failed(e) => Some(format!("{}: {e}", r.path.display())),
                                _ => None,
                            })
                            .collect();
                        w.undo_stack.push(undo);
                        w.status = Some(if failed.is_empty() { format!("Renamed {} item(s)", results.len()).into() } else { format!("{} rename(s) failed: {}", failed.len(), failed.join("; ")).into() });
                    }
                    Err(p) => w.status = Some(format!("Rename blocked: {p:?}").into()),
                }
                w.load(dir, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn problem_text(p: RenameProblem) -> &'static str {
    match p {
        RenameProblem::Empty => "Empty name",
        RenameProblem::Invalid => "Invalid name",
        RenameProblem::DuplicateInBatch => "Duplicate name in batch",
        RenameProblem::Exists => "Name already exists",
        RenameProblem::Missing => "Source missing",
    }
}

impl Render for BatchRenameView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let name = |b: &PathBuf| b.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let blocked = self.rows.iter().filter(|r| r.problem.is_some()).count();
        let case_button = |id: &'static str, label: &'static str, rule: Option<CaseRule>, current: Option<CaseRule>, cx: &mut Context<Self>| {
            Button::new(id).small().label(label).selected(current == rule).on_click(cx.listener(move |v, _, _, cx| {
                v.case = rule;
                v.replan(cx);
            }))
        };
        let current_case = self.case;
        v_flex()
            .gap_2()
            .child(h_flex().gap_3().child(div().flex_1().child(field("Find", Input::new(&self.find)))).child(div().flex_1().child(field("Replace with", Input::new(&self.replace)))))
            .child(
                Checkbox::new("regex")
                    .label("Use regular expression ($1 in replacement)")
                    .checked(self.regex)
                    .on_click(cx.listener(|v, c: &bool, _, cx| {
                        v.regex = *c;
                        v.replan(cx);
                    })),
            )
            .child(h_flex().gap_3().child(div().flex_1().child(field("Prefix", Input::new(&self.prefix)))).child(div().flex_1().child(field("Suffix", Input::new(&self.suffix)))))
            .child(
                h_flex()
                    .gap_3()
                    .items_end()
                    .child(
                        Checkbox::new("seq")
                            .label("Number sequentially from")
                            .checked(self.sequence)
                            .on_click(cx.listener(|v, c: &bool, _, cx| {
                                v.sequence = *c;
                                v.replan(cx);
                            })),
                    )
                    .child(div().w(px(90.)).child(Input::new(&self.start)))
                    .child(div().flex_1())
                    .child(case_button("case-none", "Keep case", None, current_case, cx))
                    .child(case_button("case-lower", "lower", Some(CaseRule::Lower), current_case, cx))
                    .child(case_button("case-upper", "UPPER", Some(CaseRule::Upper), current_case, cx))
                    .child(case_button("case-title", "Title", Some(CaseRule::Title), current_case, cx)),
            )
            .child(
                v_flex()
                    .id("rename-grid")
                    .mt_2()
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(p.line)
                    .rounded_md()
                    .child(
                        h_flex()
                            .px_3()
                            .py_1()
                            .bg(p.panel)
                            .text_xs()
                            .text_color(p.muted)
                            .child(div().flex_1().child("Before"))
                            .child(div().flex_1().child("After"))
                            .child(div().w(px(170.)).child("Status")),
                    )
                    .children(self.rows.iter().take(500).map(|r| {
                        h_flex()
                            .px_3()
                            .py_1()
                            .text_sm()
                            .border_t_1()
                            .border_color(p.line.opacity(0.5))
                            .child(div().flex_1().truncate().child(name(&r.before)))
                            .child(div().flex_1().truncate().child(name(&r.after)))
                            .child(match r.problem {
                                Some(pr) => div().w(px(170.)).text_color(p.danger).child(format!("⚠ {}", problem_text(pr))),
                                None => div().w(px(170.)).text_color(p.muted).child(if r.changes() { "OK" } else { "Unchanged" }),
                            })
                    })),
            )
            .when_some(self.error.clone(), |d, e| d.child(div().text_color(p.danger).child(e)))
            .when(blocked > 0, |d| d.child(div().text_color(p.warning).child(format!("⚠ {blocked} conflicting or invalid name(s): Apply is disabled"))))
            .child(
                footer(cx)
                    .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("apply").primary().label("Apply rename").disabled(!self.can_apply()).on_click(cx.listener(|v, _, window, cx| v.apply(window, cx)))),
            )
    }
}
