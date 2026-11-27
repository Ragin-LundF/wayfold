//! Transfer queue drawer and conflict dialog (F03). Workers report through events; this file
//! only turns them into view state and forwards user decisions.

use crate::ui::browser::icon;
use crate::ui::theme::{QUEUE_H, palette};
use crate::ui::workspace::Workspace;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use wayfold_core::fs::format_size;
use wayfold_core::ops::{
    self, ConflictAction, ConflictResolution, ItemOutcome, TransferEvent, TransferHandle, TransferMode, TransferRequest, TransferState,
};

pub struct TransferView {
    pub id: String,
    pub label: String,
    pub dest: PathBuf,
    pub mode: TransferMode,
    pub state: TransferState,
    pub totals: Option<(u64, u64)>,
    pub bytes_done: u64,
    pub items_done: u64,
    pub current: Option<PathBuf>,
    pub errors: Vec<String>,
    /// Submitted sources and the paths that failed (for Retry).
    pub sources: Vec<PathBuf>,
    pub failed: Vec<PathBuf>,
    pub started: Instant,
    pub handle: Rc<TransferHandle>,
}

impl TransferView {
    fn speed_eta(&self) -> Option<String> {
        let (total, _) = self.totals?;
        let secs = self.started.elapsed().as_secs_f64();
        if secs < 1.0 || self.bytes_done == 0 {
            return None;
        }
        let rate = self.bytes_done as f64 / secs;
        let left = total.saturating_sub(self.bytes_done) as f64 / rate;
        Some(format!("{}/s · {:.0}s left", format_size(rate as u64), left))
    }
}

impl Workspace {
    /// Retry (F03): resubmit the sources that had failures; existing partial results go
    /// through the normal conflict dialog, nothing is overwritten silently.
    pub fn retry_transfer(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.transfers.iter().find(|t| t.id == id) else { return };
        let retry: Vec<PathBuf> = t.sources.iter().filter(|s| t.failed.iter().any(|f| f.starts_with(s))).cloned().collect();
        let (mode, dest) = (t.mode, t.dest.clone());
        self.start_transfer(mode, retry, dest, window, cx);
    }

    pub fn active_transfer_count(&self) -> usize {
        self.transfers.iter().filter(|t| !t.state.is_final()).count()
    }

    /// Submit a transfer: the request (sources + destination) is captured now (F01/F03).
    pub fn start_transfer(
        &mut self,
        mode: TransferMode,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let label = match sources.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            many => format!("{} items", many.len()),
        };
        let handle = Rc::new(ops::start_transfer(TransferRequest { mode, sources: sources.clone(), dest_dir: dest.clone() }));
        self.transfers.push(TransferView {
            id: handle.id.clone(),
            label,
            dest,
            mode,
            state: TransferState::Planning,
            totals: None,
            bytes_done: 0,
            items_done: 0,
            current: None,
            errors: vec![],
            sources,
            failed: vec![],
            started: Instant::now(),
            handle: handle.clone(),
        });
        self.queue_open = true;
        self.watch_transfer(handle, window, cx);
        cx.notify();
    }

    pub fn watch_transfer_pub(&mut self, handle: Rc<TransferHandle>, window: &mut Window, cx: &mut Context<Self>) {
        self.watch_transfer(handle, window, cx);
    }

    fn watch_transfer(&mut self, handle: Rc<TransferHandle>, window: &mut Window, cx: &mut Context<Self>) {
        let events = handle.events.clone();
        let id = handle.id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                let mut batch = vec![ev];
                while let Ok(e) = events.try_recv() {
                    batch.push(e);
                }
                let alive = this.update_in(cx, |w, window, cx| {
                    for ev in batch {
                        w.apply_transfer_event(&id, ev, window, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        self.transfer_tasks.push(task);
    }

    fn apply_transfer_event(&mut self, id: &str, ev: TransferEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) else { return };
        match ev {
            TransferEvent::State(s) => {
                t.state = s;
                if s.is_final() {
                    let dest = t.dest.clone();
                    let msg = match s {
                        TransferState::Completed => format!("{} finished", t.label),
                        TransferState::Failed => format!("{} finished with {} error(s)", t.label, t.errors.len()),
                        _ => format!("{} cancelled; sources kept", t.label),
                    };
                    self.status = Some(msg.into());
                    if self.cache.get(&dest).is_some() {
                        self.load(dest, cx);
                    }
                }
            }
            TransferEvent::Totals { bytes, items } => t.totals = Some((bytes, items)),
            TransferEvent::Progress { bytes_done, items_done, current } => {
                t.bytes_done = bytes_done;
                t.items_done = items_done;
                t.current = Some(current);
            }
            TransferEvent::Conflict { source, existing } => {
                let handle = t.handle.clone();
                open_conflict_dialog(handle, source, existing, window, cx);
            }
            TransferEvent::Item(r) => match r.outcome {
                ItemOutcome::Failed(e) => {
                    t.errors.push(format!("{}: {e}", r.path.display()));
                    t.failed.push(r.path.clone());
                }
                ItemOutcome::Skipped(why) => t.errors.push(format!("Skipped {}: {why}", r.path.display())),
                ItemOutcome::Done(_) => {}
            },
            TransferEvent::Moved(paths) => {
                self.clear_moved(&paths);
                for p in paths.iter().filter_map(|p| p.parent()) {
                    if self.cache.get(p).is_some() {
                        self.load(p.to_path_buf(), cx);
                    }
                }
            }
            TransferEvent::Undo(u) => self.undo_stack.push(u),
        }
    }
}

/// Conflict: skip, keep both, replace; "apply to remaining". Nothing is preselected and
/// Enter does not choose a destructive action.
fn open_conflict_dialog(handle: Rc<TransferHandle>, source: PathBuf, existing: PathBuf, window: &mut Window, cx: &mut App) {
    let apply_all = Rc::new(Cell::new(false));
    let unanswered = SkipIfUnanswered { handle, answered: Rc::new(Cell::new(false)) };
    window.open_dialog(cx, move |dialog, _, cx| {
        let (handle, answered) = (&unanswered.handle, &unanswered.answered);
        let p = palette(cx);
        let choose = |action: ConflictAction, handle: Rc<TransferHandle>, apply_all: Rc<Cell<bool>>, answered: Rc<Cell<bool>>| {
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                answered.set(true);
                handle.resolve(ConflictResolution { action, apply_to_remaining: apply_all.get() });
                window.close_dialog(cx);
            }
        };
        let (h, a, d) = (handle.clone(), apply_all.clone(), answered.clone());
        let checked = apply_all.get();
        let a2 = apply_all.clone();
        dialog
            .title("An item with this name already exists")
            .w(px(560.))
            .overlay_closable(false)
            .keyboard(false)
            .close_button(false)
            .child(
                v_flex()
                    .gap_3()
                    .p_2()
                    .child(div().text_color(p.muted).child(SharedString::from(format!("Source: {}", source.display()))))
                    .child(div().text_color(p.muted).child(SharedString::from(format!("Existing: {}", existing.display()))))
                    .child(div().child("Replace moves the existing item to the Trash."))
                    .child(Checkbox::new("apply-all").label("Apply to remaining conflicts").checked(checked).on_click(
                        move |v, window, _| {
                            a2.set(*v);
                            window.refresh();
                        },
                    )),
            )
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .pt_3()
                    .border_t_1()
                    .border_color(p.line)
                    .child(Button::new("skip").label("Skip").on_click(choose(ConflictAction::Skip, h.clone(), a.clone(), d.clone())))
                    .child(Button::new("keep").label("Keep Both").on_click(choose(
                        ConflictAction::KeepBoth,
                        h.clone(),
                        a.clone(),
                        d.clone(),
                    )))
                    .child(Button::new("replace").danger().label("Replace").on_click(choose(ConflictAction::Replace, h, a, d))),
            )
    });
}

/// Owned by the conflict dialog's builder, so it drops with the dialog. `close_dialog` closes the
/// newest dialog without running `on_close` (e.g. another dialog closing itself while this one is
/// on top): the transfer must still get an answer, and skipping replaces nothing. On window close
/// this Skip races the transfer's receiver being dropped, so the item may be skipped or the job cancelled.
struct SkipIfUnanswered {
    handle: Rc<TransferHandle>,
    answered: Rc<Cell<bool>>,
}

impl Drop for SkipIfUnanswered {
    fn drop(&mut self) {
        if !self.answered.get() {
            self.handle.resolve(ConflictResolution { action: ConflictAction::Skip, apply_to_remaining: false });
        }
    }
}

pub fn render_queue(w: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    v_flex()
        .id("queue")
        .h(px(QUEUE_H))
        .flex_none()
        .bg(p.panel)
        .border_t_1()
        .border_color(p.line)
        .child(
            h_flex()
                .px_4()
                .py_2()
                .gap_2()
                .border_b_1()
                .border_color(p.line)
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Transfer queue"))
                .child(div().flex_1())
                .child(Button::new("queue-clear").ghost().xsmall().label("Clear finished").on_click(cx.listener(|w, _, _, cx| {
                    w.transfers.retain(|t| !t.state.is_final());
                    cx.notify();
                })))
                .child(Button::new("queue-close").ghost().xsmall().icon(icon("x")).tooltip("Close").on_click(cx.listener(
                    |w, _, _, cx| {
                        w.queue_open = false;
                        cx.notify();
                    },
                ))),
        )
        .child(
            v_flex()
                .id("queue-list")
                .flex_1()
                .overflow_y_scroll()
                .px_4()
                .when(w.transfers.is_empty(), |d| {
                    d.child(div().py_3().text_color(p.muted).child("No transfers. Select, copy or cut, navigate, then paste."))
                })
                .children(w.transfers.iter().rev().map(|t| {
                    let pct = t.totals.map(|(b, _)| if b == 0 { 100. } else { t.bytes_done as f32 / b as f32 * 100. });
                    let running = matches!(
                        t.state,
                        TransferState::Running | TransferState::Planning | TransferState::Queued | TransferState::WaitingConflict
                    );
                    let paused = t.state == TransferState::Paused;
                    let h1 = t.handle.clone();
                    let h2 = t.handle.clone();
                    let verb = if t.mode == TransferMode::Move { "Move" } else { "Copy" };
                    let detail = match t.totals {
                        Some((b, n)) => format!(
                            "{:?} · {} / {} · {} / {} items{}",
                            t.state,
                            format_size(t.bytes_done),
                            format_size(b),
                            t.items_done,
                            n,
                            t.speed_eta().map(|s| format!(" · {s}")).unwrap_or_default()
                        ),
                        None => format!("{:?} · calculating…", t.state),
                    };
                    v_flex()
                        .py_2()
                        .gap_1()
                        .border_b_1()
                        .border_color(p.line.opacity(0.5))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().truncate().child(SharedString::from(format!(
                                    "{verb} {} → {}",
                                    t.label,
                                    t.dest.display()
                                ))))
                                .when(running || paused, |d| {
                                    d.child(
                                        Button::new(SharedString::from(format!("pause-{}", t.id)))
                                            .xsmall()
                                            .label(if paused { "Resume" } else { "Pause" })
                                            .on_click(move |_, _, _| if paused { h1.resume() } else { h1.pause() }),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("cancel-{}", t.id)))
                                            .xsmall()
                                            .label("Cancel")
                                            .on_click(move |_, _, _| h2.cancel()),
                                    )
                                })
                                .when(t.state == TransferState::Failed && !t.failed.is_empty(), |d| {
                                    let id = t.id.clone();
                                    d.child(
                                        Button::new(SharedString::from(format!("retry-{}", t.id)))
                                            .xsmall()
                                            .label("Retry failed")
                                            .on_click(cx.listener(move |w, _, window, cx| w.retry_transfer(&id, window, cx))),
                                    )
                                }),
                        )
                        .child(match pct {
                            Some(v) => Progress::new(SharedString::from(format!("prog-{}", t.id))).value(v).into_any_element(),
                            // Indeterminate until totals are known.
                            None => Progress::new(SharedString::from(format!("prog-{}", t.id))).loading(true).into_any_element(),
                        })
                        .child(div().text_xs().text_color(p.muted).child(SharedString::from(detail)))
                        .when_some(t.current.clone().filter(|_| running), |d, c| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(p.muted)
                                    .truncate()
                                    .child(SharedString::from(format!("Current: {}", c.display()))),
                            )
                        })
                        .children(
                            t.errors
                                .iter()
                                .take(5)
                                .map(|e| div().text_xs().text_color(p.danger).child(SharedString::from(format!("⚠ {e}")))),
                        )
                })),
        )
}
