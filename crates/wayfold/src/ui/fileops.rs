//! File command handlers (F01–F05, P03, v0.3 tags, R01). Mutations run on worker pools;
//! the UI only plans, confirms and shows outcomes.

use crate::app::actions::*;
use crate::ui::browser::icon;
use crate::ui::clipboard::ClipMode;
use crate::ui::theme::palette;
use crate::ui::workspace::Workspace;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use wayfold_core::fs::size::{SizeTotals, folder_size};
use wayfold_core::fs::{Entry, format_size, tags};
use wayfold_core::ops::{self, ItemOutcome, TransferMode, UndoAction};
use wayfold_core::platform;
use wayfold_core::pool::{self, CancelToken};

#[derive(Clone, Debug)]
pub enum SizeState {
    Calculating(SizeTotals),
    Done(SizeTotals),
}

/// Spacious dialog footer: right-aligned actions separated by a rule (v0.3).
pub fn footer(cx: &App) -> Div {
    let p = palette(cx);
    h_flex().w_full().gap_2().justify_end().pt_4().mt_2().border_t_1().border_color(p.line)
}

/// Byte length of the part of `name` a rename preselects: before the last extension dot,
/// the whole name for folders, dotfiles and names without extension.
fn stem_len(name: &str, is_dir: bool) -> usize {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !is_dir => dot,
        _ => name.len(),
    }
}

/// Tags dialog chip: adds the standard tag `standard` to `path` (`on`) or removes it. On a German
/// Mac the red tag is "Rot"; a custom tag that only shares its color is kept.
pub(crate) fn set_standard_tag(path: &Path, standard: &tags::Tag, english: &str, on: bool) -> std::io::Result<()> {
    let mut t = tags::read(path);
    if !on {
        t.retain(|x| !x.is_standard(standard, english))
    } else if !t.iter().any(|x| x.is_standard(standard, english)) {
        t.push(standard.clone())
    }
    tags::write(path, &t)
}

/// Tags dialog chip state: selected when every entry has the standard tag (a click then removes it).
pub(crate) fn all_have_standard_tag(entries: &[Entry], standard: &tags::Tag, english: &str) -> bool {
    entries.iter().all(|e| e.tags.iter().any(|t| t.is_standard(standard, english)))
}

pub fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    v_flex().gap_2().my_2().child(div().text_sm().child(label)).child(control)
}

/// One option of a [`segmented`] control: element id, label, value.
pub type Segment<T> = (&'static str, &'static str, T);

/// Segmented control (D-2): a bordered group of exclusive options on gpui-component's ButtonGroup.
/// The active segment is accent-tinted with a bold label and a check icon, so the state never rests
/// on color alone; ButtonGroup reports it to screen readers as the pressed (toggled) state, and each
/// segment is a Tab stop with the house focus ring that Space activates (in a dialog, Return stays the
/// dialog's confirm key).
pub fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[Segment<T>],
    active: T,
    disabled: bool,
    cx: &App,
    on_select: impl Fn(&T, &mut Window, &mut App) + 'static,
) -> ButtonGroup {
    let p = palette(cx);
    let on_select = Rc::new(on_select);
    debug_assert_eq!(options.iter().filter(|o| o.2 == active).count(), 1, "segmented {id}: exactly one option must be active");
    // A selected button never shows hover, so the tint needs no hover color; `active` is its fill.
    let tint = ButtonCustomVariant::new(cx).color(p.accent).foreground(p.text).active(p.accent.opacity(0.22));
    ButtonGroup::new(id).children(options.iter().map(|&(seg_id, label, value)| {
        let on_select = on_select.clone();
        let on = value == active;
        // Every segment reserves the check slot (invisible when off), so labels don't shift on toggle.
        let check = if on { icon("check") } else { icon("check").text_color(transparent_black()) };
        Button::new(seg_id)
            .small()
            .label(label)
            .selected(on)
            .disabled(disabled)
            .icon(check)
            // Button draws a border only for the default variant or `outline()`; the outline keeps the group's frame.
            .when(on, |b| b.custom(tint).outline().font_weight(FontWeight::BOLD))
            .on_click(move |_, window, cx| on_select(&value, window, cx))
    }))
}

impl Workspace {
    pub fn register_file_actions<E: InteractiveElement>(&self, el: E, cx: &mut Context<Self>) -> E {
        el.on_action(cx.listener(|w, _: &CopyItems, _, cx| w.set_clipboard(ClipMode::Copy, cx)))
            .on_action(cx.listener(|w, _: &CutItems, _, cx| w.set_clipboard(ClipMode::Cut, cx)))
            .on_action(cx.listener(|w, _: &ClearClipboard, _, cx| w.clear_clipboard(cx)))
            .on_action(cx.listener(|w, _: &PasteItems, window, cx| w.paste(window, cx)))
            .on_action(cx.listener(|w, _: &TrashItems, _, cx| w.trash_selected(cx)))
            .on_action(cx.listener(|w, _: &RenameItem, window, cx| w.rename_dialog(window, cx)))
            .on_action(cx.listener(|w, _: &NewFolder, window, cx| w.create(true, window, cx)))
            .on_action(cx.listener(|w, _: &NewFile, window, cx| w.create(false, window, cx)))
            .on_action(cx.listener(|w, _: &DuplicateItems, window, cx| {
                for e in w.selected_entries() {
                    let h = std::rc::Rc::new(ops::duplicate(&e.path));
                    w.track_transfer_handle(
                        h,
                        format!("Duplicate {}", e.display_name()),
                        e.path.parent().unwrap_or(Path::new("/")).to_path_buf(),
                        window,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|w, _: &MakeAlias, _, cx| w.make_alias(cx)))
            .on_action(cx.listener(|w, _: &Undo, _, cx| w.undo(cx)))
            .on_action(cx.listener(|w, _: &BatchRename, window, cx| w.batch_rename_dialog(window, cx)))
            .on_action(cx.listener(|w, _: &OpenWith, window, cx| w.open_with_dialog(window, cx)))
            .on_action(cx.listener(|w, _: &RevealOriginal, _, cx| w.reveal_original(cx)))
            .on_action(cx.listener(|w, _: &GetInfo, _, cx| w.compute_sizes(cx)))
            .on_action(cx.listener(|w, _: &EditTags, window, cx| w.tags_dialog(window, cx)))
            .on_action(cx.listener(|w, _: &ConnectToServer, window, cx| w.connect_dialog(window, cx)))
            .on_action(cx.listener(|w, _: &CompareFolders, window, cx| crate::ui::tools::open_compare(w, window, cx)))
            .on_action(cx.listener(|w, _: &AnalyzeDiskUsage, window, cx| crate::ui::tools::open_disk_usage(w, window, cx)))
            .on_action(cx.listener(|w, _: &CreateArchive, window, cx| crate::ui::tools::open_create_archive(w, window, cx)))
            .on_action(cx.listener(|w, _: &ExtractArchive, window, cx| crate::ui::tools::open_extract_archive(w, window, cx)))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
    }

    /// Paste targets the active pane's current folder, never the selected child (02-design).
    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((mode, sources)) = self.paste_sources() else {
            self.notify_status("Clipboard is empty", cx);
            return;
        };
        let dest = self.tab().location.clone();
        if self.refuse_into_itself(&sources, &dest, cx) {
            return;
        }
        let mode = if mode == ClipMode::Cut { TransferMode::Move } else { TransferMode::Copy };
        self.start_transfer(mode, sources, dest, window, cx);
    }

    /// Drag & drop: same volume moves, other volumes copy (Finder convention).
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, dest: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.parent() != Some(dest.as_path()) && *p != dest).collect();
        if paths.is_empty() || self.refuse_into_itself(&paths, &dest, cx) {
            return;
        }
        let same_volume = |a: &Path, b: &Path| {
            use std::os::unix::fs::MetadataExt;
            matches!((std::fs::symlink_metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev())
        };
        let mode = if paths.iter().all(|p| same_volume(p, &dest)) { TransferMode::Move } else { TransferMode::Copy };
        self.start_transfer(mode, paths, dest, window, cx);
    }

    /// Refuses up front, with a status message, when `dest` is one of the folders in `sources` or
    /// lies inside one; the transfer core still refuses it per item as a safety net.
    fn refuse_into_itself(&mut self, sources: &[PathBuf], dest: &Path, cx: &mut Context<Self>) -> bool {
        let dest = std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
        let resolve_parent = |p: &PathBuf| match (p.parent().and_then(|d| std::fs::canonicalize(d).ok()), p.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => p.clone(),
        };
        // Full canonicalize fixes case and Unicode form; a dropped symlink resolves only its parent,
        // so it is compared as the link, not its target.
        let resolve = |p: &PathBuf| {
            if p.is_symlink() { resolve_parent(p) } else { std::fs::canonicalize(p).unwrap_or_else(|_| resolve_parent(p)) }
        };
        // Path::starts_with compares whole components, so /a/foobar is not inside /a/foo.
        if !sources.iter().any(|s| dest.starts_with(resolve(s))) {
            return false;
        }
        self.notify_status("A folder can't be put inside itself", cx);
        true
    }

    pub fn track_transfer_handle(
        &mut self,
        handle: Rc<ops::TransferHandle>,
        label: String,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.transfers.push(crate::ui::transfers::TransferView {
            id: handle.id.clone(),
            label,
            dest,
            mode: TransferMode::Copy,
            state: ops::TransferState::Planning,
            totals: None,
            bytes_done: 0,
            items_done: 0,
            current: None,
            errors: vec![],
            sources: vec![],
            failed: vec![],
            started: std::time::Instant::now(),
            handle: handle.clone(),
        });
        self.watch_transfer_pub(handle, window, cx);
    }

    /// Move to Trash (reversible; no permanent delete).
    fn trash_selected(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.selected_entries().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            return;
        }
        let dir = self.tab().location.clone();
        self.run_op(dir, cx, move || ops::trash(&paths));
    }

    /// Run a blocking op on the right pool, then refresh, record undo and report failures.
    fn run_op(
        &mut self,
        refresh: PathBuf,
        cx: &mut Context<Self>,
        f: impl FnOnce() -> (Vec<ops::ItemResult>, Option<UndoAction>) + Send + 'static,
    ) {
        let fut = pool::for_path(&refresh).run(f);
        cx.spawn(async move |this, cx| {
            let Some((results, undo)) = fut.await else {
                // The job panicked on its worker thread; never end an operation (or undo) silently.
                this.update(cx, |w, cx| w.notify_status("The operation failed unexpectedly", cx)).ok();
                return;
            };
            this.update(cx, |w, cx| {
                let failures: Vec<String> = results
                    .iter()
                    .filter_map(|r| match &r.outcome {
                        ItemOutcome::Failed(e) => Some(format!("{}: {e}", r.path.display())),
                        ItemOutcome::Skipped(e) => Some(format!("{}: {e}", r.path.display())),
                        ItemOutcome::Done(_) => None,
                    })
                    .collect();
                if let Some(u) = undo {
                    w.undo_stack.push(u);
                }
                w.status = Some(if failures.is_empty() {
                    "Done".into()
                } else {
                    format!("{} failed: {}", failures.len(), failures.join("; ")).into()
                });
                w.load(refresh, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.undo_stack.pop() else {
            self.notify_status("Nothing to undo", cx);
            return;
        };
        let dir = self.tab().location.clone();
        self.run_op(dir, cx, move || (ops::undo(&action), None));
    }

    fn create(&mut self, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.tab().location.clone();
        let base = if folder { "untitled folder" } else { "untitled.txt" };
        let fut = pool::for_path(&dir).run(move || {
            let free = dir.join(base);
            let target = if free.symlink_metadata().is_err() { free } else { ops::unique_name(&dir, base.as_ref()) };
            let name = target.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| OsString::from(base));
            let r = if folder { ops::create_folder(&dir, &name) } else { ops::create_file(&dir, &name) };
            r.map(|(path, undo)| (wayfold_core::fs::stat_entry(&path), undo))
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(r) = fut.await else { return };
            this.update_in(cx, |w, window, cx| match r {
                Ok((entry, undo)) => {
                    w.undo_stack.push(undo);
                    let loc = w.tab().location.clone();
                    let path = entry.path.clone();
                    w.cache.upsert(&loc, entry);
                    w.tab_mut().select_only(&path);
                    // Like Finder: start renaming the new item.
                    w.rename_dialog(window, cx);
                }
                Err(e) => w.notify_status(format!("Could not create: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.focused_entry() else { return };
        if self.tab().selection.len() > 1 {
            return self.batch_rename_dialog(window, cx);
        }
        let name = e.display_name();
        let stem = stem_len(&name, e.is_dir());
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value(name.clone());
            // Like Finder: typing replaces the name but keeps the extension.
            state.set_selected_range(0..stem, cx);
            state
        });
        let this = cx.entity().downgrade();
        let path = e.path.clone();
        let first = Cell::new(true);
        let submit = {
            let (input, this, path) = (input.clone(), this.clone(), path.clone());
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let new = input.read(cx).value().to_string();
                if new.is_empty() || new.contains('/') {
                    return;
                }
                window.close_dialog(cx);
                let (path, this) = (path.clone(), this.clone());
                let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
                let _ = this.update(cx, |w, cx| {
                    let p2 = path.clone();
                    w.run_op(dir, cx, move || match ops::rename(&p2, new.as_ref()) {
                        Ok(u) => (vec![], Some(u)),
                        Err(e) => (vec![ops::ItemResult { path: p2.clone(), outcome: ItemOutcome::Failed(e.to_string()) }], None),
                    });
                });
            })
        };
        let submit2 = submit.clone();
        self._subscriptions.push(cx.subscribe_in(&input, window, move |_, _, ev: &gpui_kit::component::input::InputEvent, window, cx| {
            if let gpui_kit::component::input::InputEvent::PressEnter { .. } = ev {
                // Deferred: this runs inside a Workspace update and `submit` updates it again (abort).
                let submit2 = submit2.clone();
                window.defer(cx, move |window, cx| submit2(window, cx));
            }
        }));
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let submit = submit.clone();
            dialog.title("Rename").w(px(560.)).child(field("New name", Input::new(&input))).footer(
                footer(cx)
                    .child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("ok").primary().label("Rename").on_click(move |_, window, cx| submit(window, cx))),
            )
        });
    }

    fn make_alias(&mut self, cx: &mut Context<Self>) {
        let entries = self.selected_entries();
        let dir = self.tab().location.clone();
        self.run_op(dir, cx, move || {
            let mut out = vec![];
            let mut undo = None;
            for e in entries {
                let parent = e.path.parent().unwrap_or(Path::new("/"));
                let link = ops::unique_name(parent, OsString::from(format!("{} alias", e.display_name())).as_os_str());
                match ops::create_symlink(&e.path, &link) {
                    Ok(u) => undo = Some(u),
                    Err(err) => out.push(ops::ItemResult { path: e.path.clone(), outcome: ItemOutcome::Failed(err.to_string()) }),
                }
            }
            (out, undo)
        });
    }

    fn reveal_original(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.focused_entry() else { return };
        match platform::resolve_original(&e.path) {
            Some(orig) if orig.exists() => {
                if let Some(parent) = orig.parent() {
                    self.navigate(parent.to_path_buf(), cx);
                    self.tab_mut().select_only(&orig);
                }
            }
            Some(orig) => self.notify_status(format!("Original not found: {}", orig.display()), cx),
            None => self.notify_status("Not an alias or symbolic link", cx),
        }
    }

    // ----- folder size (P03) -----------------------------------------------------------

    pub fn folder_size_label(&self, path: &Path) -> SharedString {
        match self.folder_sizes.get(path) {
            None => "—".into(),
            Some(SizeState::Calculating(t)) => format!("calculating… {}", format_size(t.bytes)).into(),
            Some(SizeState::Done(t)) if t.errors > 0 => format!("{} (partial)", format_size(t.bytes)).into(),
            Some(SizeState::Done(t)) => format_size(t.bytes).into(),
        }
    }

    /// Compute sizes of selected folders (or the current folder) asynchronously; cancellable
    /// by navigating away or recomputing.
    pub fn compute_sizes(&mut self, cx: &mut Context<Self>) {
        let mut targets: Vec<PathBuf> = self.selected_entries().into_iter().filter(|e| e.is_dir()).map(|e| e.path).collect();
        if targets.is_empty() {
            targets.push(self.tab().location.clone());
        }
        for dir in targets {
            self.compute_size(dir, cx);
        }
        self.preview_open = true;
        cx.notify();
    }

    pub fn compute_size(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.folder_sizes.get(&dir), Some(SizeState::Calculating(_))) {
            return;
        }
        self.folder_sizes.insert(dir.clone(), SizeState::Calculating(SizeTotals::default()));
        let (tx, rx) = async_channel::unbounded::<(bool, SizeTotals)>();
        let cancel = CancelToken::new();
        let (d2, c2) = (dir.clone(), cancel.clone());
        pool::for_path(&dir).execute(move || {
            let tx2 = tx.clone();
            let t = folder_size(&d2, &c2, |t| {
                let _ = tx2.try_send((false, t));
            });
            let _ = tx.send_blocking((true, t));
        });
        let task = cx.spawn(async move |this, cx| {
            let _guard = crate::ui::workspace::CancelGuard(cancel);
            while let Ok((done, t)) = rx.recv().await {
                let key = dir.clone();
                if this
                    .update(cx, |w, cx| {
                        w.folder_sizes.insert(key, if done { SizeState::Done(t) } else { SizeState::Calculating(t) });
                        cx.notify();
                    })
                    .is_err()
                    || done
                {
                    return;
                }
            }
        });
        self.size_tasks.push(task);
    }

    // ----- dialogs ---------------------------------------------------------------------

    fn open_with_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.focused_entry().filter(|e| !e.is_dir() || e.extension() == "app") else {
            self.notify_status("Select a file to open", cx);
            return;
        };
        let path = e.path.clone();
        let fut = pool::local().run({
            let p = path.clone();
            move || platform::list_open_with(&p)
        });
        cx.spawn_in(window, async move |_, cx| {
            let apps = fut.await.unwrap_or_default();
            cx.update(|window, cx| {
                window.open_dialog(cx, move |dialog, _, cx| {
                    let p = palette(cx);
                    dialog
                        .title("Open With")
                        .w(px(560.))
                        .child(
                            v_flex()
                                .id("apps")
                                .gap_1()
                                .max_h(px(360.))
                                .overflow_y_scroll()
                                .when(apps.is_empty(), |d| d.child(div().text_color(p.muted).child("No registered applications")))
                                .children(apps.iter().enumerate().map(|(i, app)| {
                                    let (app, file) = (app.clone(), path.clone());
                                    let name = app.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                    Button::new(("app", i))
                                        .ghost()
                                        .w_full()
                                        .justify_start()
                                        .label(if i == 0 { format!("{name} (default)") } else { name })
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            if let Err(e) = platform::open_with(&file, &app) {
                                                window.push_notification(format!("Could not open: {e}"), cx);
                                            }
                                        })
                                })),
                        )
                        .footer(footer(cx).child(Button::new("close").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx))))
                });
            })
            .ok();
        })
        .detach();
    }

    /// Tags: toggling a color applies it to all selected items, or removes it when all have it.
    fn tags_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.selected_entries();
        if entries.is_empty() {
            self.notify_status("Select files or folders first", cx);
            return;
        }
        let standards = self.standard_tags.clone();
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = palette(cx);
            dialog
                .title("Tags")
                .w(px(560.))
                .child(
                    div()
                        .text_color(p.muted)
                        .child(SharedString::from(format!("Apply or remove tags on {} selected item(s).", entries.len()))),
                )
                .child(h_flex().flex_wrap().gap_2().my_4().children(standards.iter().map(|(name, rgb_color, standard)| {
                    let all = all_have_standard_tag(&entries, standard, name);
                    let (entries, this, name, standard) = (entries.clone(), this.clone(), *name, standard.clone());
                    Button::new(SharedString::from(format!("tag-{name}")))
                        .selected(all)
                        .child(h_flex().gap_2().child(div().size(px(11.)).rounded_full().bg(rgb(*rgb_color))).child(standard.name.clone()))
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let (entries, standard) = (entries.clone(), standard.clone());
                            let _ = this.update(cx, |w, cx| {
                                let dir = w.tab().location.clone();
                                w.run_op(dir, cx, move || {
                                    let results = entries
                                        .iter()
                                        .filter_map(|e| {
                                            set_standard_tag(&e.path, &standard, name, !all).err().map(|err| ops::ItemResult {
                                                path: e.path.clone(),
                                                outcome: ItemOutcome::Failed(err.to_string()),
                                            })
                                        })
                                        .collect();
                                    (results, None)
                                });
                            });
                        })
                })))
                .child(div().text_xs().text_color(p.muted).child("Tags are stored as Finder tags and preserve custom tags."))
                .footer(footer(cx).child(Button::new("close").label("Done").on_click(|_, window, cx| window.close_dialog(cx))))
        });
    }

    fn connect_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value("smb://"));
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let p = palette(cx);
            let input2 = input.clone();
            dialog
                .title("Connect to Server")
                .w(px(560.))
                .child(field("Server address", Input::new(&input)))
                .child(div().text_sm().text_color(p.muted).child(
                    "macOS shows its own sign-in and mounts the share under /Volumes. Mounted shares appear in the sidebar under Network.",
                ))
                .footer(footer(cx).child(Button::new("cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx))).child(
                    Button::new("connect").primary().label("Connect").on_click(move |_, window, cx| {
                        let addr = input2.read(cx).value().to_string();
                        match platform::connect_to_server(&addr) {
                            Ok(()) => window.close_dialog(cx),
                            Err(e) => window.push_notification(e, cx),
                        }
                    }),
                ))
        });
    }

    fn batch_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.selected_entries().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            self.notify_status("Select files first", cx);
            return;
        }
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| crate::ui::batch_rename::BatchRenameView::new(paths, workspace, window, cx));
        window.open_dialog(cx, move |dialog, _, _| dialog.title("Batch Rename").w(px(720.)).child(view.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::stem_len;

    #[test]
    fn rename_preselects_the_stem() {
        assert_eq!(stem_len("report.final.pdf", false), "report.final".len());
        assert_eq!(stem_len("untitled folder", true), "untitled folder".len());
        assert_eq!(stem_len("my.dir", true), "my.dir".len());
        assert_eq!(stem_len(".bashrc", false), ".bashrc".len());
        assert_eq!(stem_len("Makefile", false), "Makefile".len());
        assert_eq!(stem_len("Grüße.txt", false), "Grüße".len());
    }
}
