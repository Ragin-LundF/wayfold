//! Archive dialogs (F08). Create: name + format for the selected items. Extract: read-only
//! virtualized entry list, bounded preview, destination, password when needed. Jobs run on
//! the pools; the dialog shows progress and can stop them or let them finish in the
//! background (the result is reported on the workspace either way).

use crate::ui::fileops::{field, footer};
use crate::ui::theme::palette;
use crate::ui::workspace::{CancelGuard, Workspace};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputContentType, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use wayfold_core::archive::{self, ArchiveEntry, ArchiveError, ArchiveEvent, ArchiveFormat, ArchiveJob, EntryKind, Limits, Listing};
use wayfold_core::fs::format_size;
use wayfold_core::pool::{self, CancelToken};

const PREVIEW_BYTES: usize = 16 << 10;
const ROW_H: f32 = 26.;

/// A running job as seen by its dialog.
pub(crate) struct JobState {
    cancel: CancelToken,
    progress: archive::Progress,
}

/// Dialogs that run an archive job.
pub(crate) trait JobView: 'static + Sized {
    fn job_mut(&mut self) -> &mut Option<JobState>;
    /// Handle the result while the dialog is still open; return true if it was shown here.
    fn finished(&mut self, result: &Result<PathBuf, ArchiveError>, window: &mut Window, cx: &mut Context<Self>) -> bool;
}

/// Follow `job` until done. Progress goes to the dialog if it is still open; the result is
/// always reported on the workspace, so closing the dialog lets the job finish in the background.
fn watch_job<V: JobView>(
    view: &mut V,
    job: ArchiveJob,
    label: &'static str,
    refresh: PathBuf,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let events = job.events.clone();
    *view.job_mut() = Some(JobState { cancel: job.cancel, progress: Default::default() });
    cx.notify();
    cx.spawn_in(window, async move |this, cx| {
        while let Ok(ev) = events.recv().await {
            let result = match ev {
                ArchiveEvent::Progress(p) => {
                    let _ = this.update(cx, |v, cx| {
                        if let Some(j) = v.job_mut() {
                            j.progress = p;
                        }
                        cx.notify();
                    });
                    continue;
                }
                ArchiveEvent::Done(r) => r,
            };
            let shown = this
                .update_in(cx, |v, window, cx| {
                    *v.job_mut() = None;
                    cx.notify();
                    v.finished(&result, window, cx)
                })
                .unwrap_or(false);
            let _ = workspace.update_in(cx, |w, window, cx| {
                match &result {
                    Ok(path) => w.status = Some(format!("{label}: {}", path.display()).into()),
                    Err(ArchiveError::Cancelled) => w.status = Some(format!("{label}: cancelled").into()),
                    Err(e) => {
                        w.status = Some(format!("{label} failed: {e}").into());
                        if !shown {
                            window.push_notification(format!("{label} failed: {e}"), cx);
                        }
                    }
                }
                w.load(refresh.clone(), cx);
                cx.notify();
            });
            return;
        }
    })
    .detach();
}

fn job_progress(job: &JobState, total: Option<u64>) -> impl IntoElement {
    let bar = Progress::new("archive-progress");
    let bar = match total.filter(|&t| t > 0) {
        Some(t) => bar.value((job.progress.bytes as f32 / t as f32 * 100.).min(100.)),
        None => bar.loading(true),
    };
    v_flex().gap_1().child(bar).child(div().text_xs().truncate().child(format!(
        "{} · {} item(s) · {}",
        format_size(job.progress.bytes),
        job.progress.entries,
        job.progress.current
    )))
}

/// The dialog's own focus handle, taken right after `open_dialog` (which focuses the new dialog).
/// `close_dialog` closes the newest dialog, which may be one opened on top of this one (a paste
/// conflict), so this dialog closes only while focus is inside it, i.e. while it is the topmost one.
/// A close skipped that way is kept pending and done once the dialog is topmost again.
#[derive(Default)]
pub(crate) struct OwnDialog {
    pub(crate) handle: Option<FocusHandle>,
    close_pending: bool,
}

impl OwnDialog {
    fn is_topmost(&self, window: &Window, cx: &App) -> bool {
        self.handle.as_ref().is_some_and(|f| f.contains_focused(window, cx))
    }

    fn close(&mut self, window: &mut Window, cx: &mut App) {
        if self.is_topmost(window, cx) {
            window.close_dialog(cx);
        } else {
            self.close_pending = true;
        }
    }

    /// Called from render: the dialog finished under another one, which is gone now.
    fn close_if_pending(&mut self, window: &mut Window, cx: &mut App) {
        if self.close_pending && self.is_topmost(window, cx) {
            self.close_pending = false;
            window.defer(cx, |window, cx| window.close_dialog(cx));
        }
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ----- create ----------------------------------------------------------------------------

pub struct CreateArchiveView {
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    workspace: WeakEntity<Workspace>,
    name: Entity<InputState>,
    format: ArchiveFormat,
    job: Option<JobState>,
    pub(crate) own_dialog: OwnDialog,
    error: Option<String>,
}

impl CreateArchiveView {
    pub fn new(
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default = match sources.as_slice() {
            [one] => name_of(one),
            _ => "Archive".to_string(),
        };
        let name = cx.new(|cx| InputState::new(window, cx).default_value(default));
        Self { sources, dest_dir, workspace, name, format: ArchiveFormat::Zip, job: None, own_dialog: OwnDialog::default(), error: None }
    }

    fn file_name(&self, cx: &App) -> OsString {
        let base = self.name.read(cx).value().trim().to_string();
        let ext = self.format.extension();
        if base.to_lowercase().ends_with(ext) { base.into() } else { format!("{base}{ext}").into() }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.file_name(cx);
        if self.name.read(cx).value().trim().is_empty() || wayfold_core::ops::validate_name(&name).is_err() {
            self.error = Some("Enter a valid archive name".into());
            cx.notify();
            return;
        }
        self.error = None;
        let job = archive::start_create(archive::CreateRequest {
            sources: self.sources.clone(),
            dest: self.dest_dir.join(name),
            format: self.format,
        });
        let (dir, ws) = (self.dest_dir.clone(), self.workspace.clone());
        watch_job(self, job, "Archive created", dir, ws, window, cx);
    }
}

impl JobView for CreateArchiveView {
    fn job_mut(&mut self) -> &mut Option<JobState> {
        &mut self.job
    }

    fn finished(&mut self, result: &Result<PathBuf, ArchiveError>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match result {
            Ok(_) => {
                self.own_dialog.close(window, cx);
                false
            }
            Err(e) => {
                self.error = Some(e.to_string());
                true
            }
        }
    }
}

impl Render for CreateArchiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.own_dialog.close_if_pending(window, cx);
        let p = palette(cx);
        let running = self.job.is_some();
        let current = self.format;
        let shown: Vec<String> = self.sources.iter().take(8).map(|s| name_of(s)).collect();
        let more = self.sources.len().saturating_sub(shown.len());
        v_flex()
            .gap_2()
            .child(field("Archive name", Input::new(&self.name).disabled(running)))
            .child(h_flex().gap_2().children(ArchiveFormat::CREATE.into_iter().map(|f| {
                Button::new(f.label()).small().label(f.label()).selected(current == f).disabled(running).on_click(cx.listener(
                    move |v, _, _, cx| {
                        v.format = f;
                        cx.notify();
                    },
                ))
            })))
            .child(div().text_sm().text_color(p.muted).child(format!(
                "Will be saved as {} in {}",
                self.file_name(cx).to_string_lossy(),
                self.dest_dir.display()
            )))
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_1()
                    .border_color(p.line)
                    .rounded_md()
                    .text_sm()
                    .child(div().text_xs().text_color(p.muted).child(format!("{} selected item(s)", self.sources.len())))
                    .children(shown.into_iter().map(|n| div().truncate().child(n)))
                    .when(more > 0, |d| d.child(div().text_color(p.muted).child(format!("and {more} more")))),
            )
            .when_some(self.job.as_ref(), |d, j| d.child(job_progress(j, None)))
            .when_some(self.error.clone(), |d, e| d.child(div().text_color(p.danger).child(format!("⚠ {e}"))))
            .child(
                footer(cx)
                    .when(running, |f| {
                        f.child(Button::new("arc-bg").label("Run in background").on_click(|_, window, cx| window.close_dialog(cx))).child(
                            Button::new("arc-stop").label("Stop").on_click(cx.listener(|v, _, _, cx| {
                                if let Some(j) = &v.job {
                                    j.cancel.cancel();
                                }
                                cx.notify();
                            })),
                        )
                    })
                    .when(!running, |f| {
                        f.child(Button::new("arc-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx))).child(
                            Button::new("arc-create")
                                .primary()
                                .label("Create")
                                .on_click(cx.listener(|v, _, window, cx| v.create(window, cx))),
                        )
                    }),
            )
    }
}

// ----- extract ---------------------------------------------------------------------------

enum ListState {
    Loading,
    Ready(Listing),
    Failed(ArchiveError),
}

pub struct ExtractArchiveView {
    archive: PathBuf,
    format: Option<ArchiveFormat>,
    dest_dir: PathBuf,
    workspace: WeakEntity<Workspace>,
    name: Entity<InputState>,
    password: Entity<InputState>,
    needs_password: bool,
    listing: ListState,
    selected: Option<usize>,
    preview: Option<String>,
    job: Option<JobState>,
    pub(crate) own_dialog: OwnDialog,
    error: Option<String>,
    /// Dropping the dialog (or relisting) stops a running listing.
    list_cancel: Option<CancelGuard>,
    _tasks: Vec<Task<()>>,
}

impl ExtractArchiveView {
    pub fn new(archive: PathBuf, dest_dir: PathBuf, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let out = archive::output_name(&archive).to_string_lossy().into_owned();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(out));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Password"));
        let mut this = Self {
            format: ArchiveFormat::detect(&archive),
            archive,
            dest_dir,
            workspace,
            name,
            password,
            needs_password: false,
            listing: ListState::Loading,
            selected: None,
            preview: None,
            job: None,
            own_dialog: OwnDialog::default(),
            error: None,
            list_cancel: None,
            _tasks: Vec::new(),
        };
        this.load_listing(cx);
        this
    }

    /// Never logged or kept beyond the request.
    fn password(&self, cx: &App) -> Option<String> {
        let p = self.password.read(cx).value().to_string();
        (!p.is_empty()).then_some(p)
    }

    fn load_listing(&mut self, cx: &mut Context<Self>) {
        self.listing = ListState::Loading;
        let (path, pw) = (self.archive.clone(), self.password(cx));
        let cancel = CancelToken::new();
        self.list_cancel = Some(CancelGuard(cancel.clone()));
        let fut = pool::cpu().run(move || archive::list_entries(&path, pw.as_deref(), &cancel));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Some(r) = fut.await else { return };
            this.update(cx, |v, cx| {
                match r {
                    Ok(l) => {
                        v.needs_password |= l.entries.iter().any(|e| e.encrypted);
                        v.listing = ListState::Ready(l);
                    }
                    Err(e) => {
                        v.needs_password |= matches!(e, ArchiveError::PasswordRequired | ArchiveError::WrongPassword);
                        v.listing = ListState::Failed(e);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn entries(&self) -> &[ArchiveEntry] {
        match &self.listing {
            ListState::Ready(l) => &l.entries,
            _ => &[],
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.preview = None;
        let Some(entry) = self.entries().get(ix).filter(|e| e.kind == EntryKind::File) else {
            cx.notify();
            return;
        };
        let (path, pw, name) = (self.archive.clone(), self.password(cx), entry.name.clone());
        let fut = pool::cpu().run(move || archive::preview_entry(&path, pw.as_deref(), &name, PREVIEW_BYTES));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let Some(r) = fut.await else { return };
            this.update(cx, |v, cx| {
                if v.selected == Some(ix) {
                    v.preview = Some(match r {
                        Ok(bytes) => preview_text(&bytes),
                        Err(e) => e.to_string(),
                    });
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn extract(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name: OsString = self.name.read(cx).value().trim().to_string().into();
        if wayfold_core::ops::validate_name(&name).is_err() {
            self.error = Some("Enter a valid folder name".into());
            cx.notify();
            return;
        }
        self.error = None;
        let job = archive::start_extract(archive::ExtractRequest {
            archive: self.archive.clone(),
            dest_dir: self.dest_dir.clone(),
            name,
            password: self.password(cx),
            limits: Limits::default(),
        });
        let (dir, ws) = (self.dest_dir.clone(), self.workspace.clone());
        watch_job(self, job, "Extracted", dir, ws, window, cx);
    }
}

/// Text when the bytes look like text, else a short binary note.
fn preview_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let suspicious = text.chars().filter(|c| *c == '\u{FFFD}' || (c.is_control() && !c.is_whitespace())).count();
    if suspicious * 20 > text.chars().count().max(1) { format!("Binary data ({} bytes previewed)", bytes.len()) } else { text.into_owned() }
}

impl JobView for ExtractArchiveView {
    fn job_mut(&mut self) -> &mut Option<JobState> {
        &mut self.job
    }

    fn finished(&mut self, result: &Result<PathBuf, ArchiveError>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match result {
            Ok(_) => {
                self.own_dialog.close(window, cx);
                false
            }
            Err(e) => {
                self.needs_password |= matches!(e, ArchiveError::PasswordRequired | ArchiveError::WrongPassword);
                self.error = Some(e.to_string());
                true
            }
        }
    }
}

impl Render for ExtractArchiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.own_dialog.close_if_pending(window, cx);
        let p = palette(cx);
        let running = self.job.is_some();
        let count = self.entries().len();
        let total: u64 = self.entries().iter().filter_map(|e| e.size).sum();
        let list = uniform_list(
            "arc-entries",
            count,
            cx.processor(|v: &mut ExtractArchiveView, range: std::ops::Range<usize>, _, cx| {
                let p = palette(cx);
                let selected = v.selected;
                range
                    .filter_map(|i| v.entries().get(i).map(|e| (i, e.clone())))
                    .map(|(i, e)| {
                        let kind = match e.kind {
                            EntryKind::Dir => "Folder".to_string(),
                            EntryKind::Symlink => "Link".to_string(),
                            EntryKind::HardLink => "Hard link".to_string(),
                            EntryKind::Special => "Special (blocked)".to_string(),
                            EntryKind::File => e.size.map(format_size).unwrap_or_else(|| "—".into()),
                        };
                        h_flex()
                            .id(("arc-row", i))
                            .h(px(ROW_H))
                            .px_2()
                            .gap_2()
                            .text_sm()
                            .cursor_pointer()
                            .when(selected == Some(i), |d| d.bg(p.selection))
                            .hover(|s| s.bg(p.hover))
                            .child(div().flex_1().truncate().child(e.name.clone()))
                            .when(e.encrypted, |d| d.child(div().text_xs().text_color(p.muted).child("encrypted")))
                            .child(div().w(px(110.)).text_color(p.muted).child(kind))
                            .on_click(cx.listener(move |v, _, _, cx| v.select(i, cx)))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .h_full();
        let status = match &self.listing {
            ListState::Loading => "Reading archive…".to_string(),
            ListState::Ready(l) if l.truncated => format!("First {count} entries shown"),
            ListState::Ready(_) => format!("{count} entries · {}", format_size(total)),
            ListState::Failed(e) => e.to_string(),
        };
        let listing_failed = matches!(self.listing, ListState::Failed(_));
        v_flex()
            .gap_2()
            .child(div().text_sm().text_color(p.muted).child(format!(
                "{} · {}",
                name_of(&self.archive),
                self.format.map_or("Unknown", |f| f.label())
            )))
            .child(
                h_flex().gap_2().h(px(240.)).child(div().flex_1().h_full().border_1().border_color(p.line).rounded_md().child(list)).child(
                    div()
                        .id("arc-preview")
                        .w(px(300.))
                        .h_full()
                        .p_2()
                        .border_1()
                        .border_color(p.line)
                        .rounded_md()
                        .overflow_y_scroll()
                        .text_xs()
                        .font_family("monospace")
                        .child(self.preview.clone().unwrap_or_else(|| "Select a file to preview its beginning.".into())),
                ),
            )
            .child(div().text_xs().text_color(if listing_failed { p.danger } else { p.muted }).child(status))
            .child(field("Extract into a new folder named", Input::new(&self.name).disabled(running)))
            .child(
                div().text_xs().text_color(p.muted).child(format!("in {} (a free name is chosen if it exists)", self.dest_dir.display())),
            )
            .when(self.needs_password, |d| {
                d.child(field(
                    "Password",
                    Input::new(&self.password).content_type(InputContentType::Password).mask_toggle().disabled(running),
                ))
                .when(listing_failed, |d| {
                    d.child(
                        Button::new("arc-unlock")
                            .small()
                            .label("Read with password")
                            .on_click(cx.listener(|v, _, _, cx| v.load_listing(cx))),
                    )
                })
            })
            .when_some(self.job.as_ref(), |d, j| d.child(job_progress(j, Some(total))))
            .when_some(self.error.clone(), |d, e| d.child(div().text_color(p.danger).child(format!("⚠ {e}"))))
            .child(
                footer(cx)
                    .when(running, |f| {
                        f.child(Button::new("arc-bg").label("Run in background").on_click(|_, window, cx| window.close_dialog(cx))).child(
                            Button::new("arc-stop").label("Stop").on_click(cx.listener(|v, _, _, cx| {
                                if let Some(j) = &v.job {
                                    j.cancel.cancel();
                                }
                                cx.notify();
                            })),
                        )
                    })
                    .when(!running, |f| {
                        f.child(Button::new("arc-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx))).child(
                            Button::new("arc-extract")
                                .primary()
                                .label("Extract")
                                .disabled(self.format.is_none())
                                .on_click(cx.listener(|v, _, window, cx| v.extract(window, cx))),
                        )
                    }),
            )
    }
}
