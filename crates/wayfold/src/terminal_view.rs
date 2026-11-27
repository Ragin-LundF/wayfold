//! Terminal panel (T01/T02). Owned by the terminal implementer; mounted by `ui::workspace`.
//!
//! Renders `wayfold_core::terminal` sessions: a PTY grid per session tab, and a header with
//! the follow mode, bound pane, prompt state and "Sync now". Sessions live as long as the
//! panel entity, so hiding the panel never kills a shell.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use wayfold_core::pool;
use wayfold_core::terminal::{
    CellStyle, FollowController, FollowMode, Integration, KeyMods, PromptState, Session, SessionConfig, TerminalSnapshot, encode_key,
};

mod colors {
    pub const TERMINAL_BG: u32 = 0x111419;
    pub const TEXT: u32 = 0xa8d4b0;
    pub const PANEL: u32 = 0x1d2128;
    pub const LINE: u32 = 0x303641;
    pub const MUTED: u32 = 0x98a3b4;
    pub const ACCENT: u32 = 0x78a9ff;
}

const FONT_SIZE: f32 = 12.;
const LINE_HEIGHT: f32 = 16.;
const MODES: [(FollowMode, &str); 3] =
    [(FollowMode::Independent, "Independent"), (FollowMode::FollowBrowser, "Follow browser"), (FollowMode::TwoWay, "Two-way")];

pub enum TerminalPanelEvent {
    Hide,
    /// The user changed directory in the shell (two-way mode only).
    CwdChanged(PathBuf),
}

struct TerminalTab {
    id: u64,
    session: Rc<Session>,
    follow: FollowController,
    _changes: Task<()>,
}

pub struct TerminalPanel {
    tabs: Vec<TerminalTab>,
    active: usize,
    next_id: u64,
    browser_path: PathBuf,
    binding_label: SharedString,
    shell_integration: bool,
    mode_select: Entity<SelectState<Vec<&'static str>>>,
    focus_handle: FocusHandle,
    scroll_remainder: f32,
    error: Option<SharedString>,
    /// Spawns still running on the pool; avoids a duplicate tab when the last one closes meanwhile.
    pending_spawns: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPanel {
    pub fn new(start_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let labels = MODES.iter().map(|(_, label)| *label).collect();
        let mode_select = cx.new(|cx| SelectState::new(labels, Some(IndexPath::default()), window, cx));
        let subscription = cx.subscribe_in(&mode_select, window, |this, _, event: &SelectEvent<Vec<&'static str>>, _, cx| {
            let SelectEvent::Confirm(Some(label)) = event else {
                return;
            };
            if let Some((mode, _)) = MODES.iter().find(|(_, l)| l == label) {
                this.set_mode(*mode, cx);
            }
        });
        // Deferred so a `set_shell_integration` right after construction applies to the first shell.
        cx.defer_in(window, |this, window, cx| this.add_session(true, window, cx));
        Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            browser_path: start_dir,
            binding_label: "Pane 1".into(),
            shell_integration: false,
            mode_select,
            focus_handle: cx.focus_handle(),
            scroll_remainder: 0.,
            error: None,
            pending_spawns: 0,
            _subscriptions: vec![subscription],
        }
    }

    /// The user's opt-in for shell integration; applies to sessions opened afterwards.
    pub fn set_shell_integration(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.shell_integration = enabled;
        cx.notify();
    }

    pub fn set_binding_label(&mut self, label: SharedString, cx: &mut Context<Self>) {
        self.binding_label = label;
        cx.notify();
    }

    /// The bound pane moved; the active session follows it unless independent.
    pub fn browser_location_changed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.browser_path = path.clone();
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(bytes) = tab.follow.browser_moved(path)
        {
            tab.session.write(bytes);
        }
        cx.notify();
    }

    /// Explicit sync: cd the active shell to `path` as soon as its prompt is idle.
    pub fn sync_now(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(bytes) = tab.follow.request_cd(path)
        {
            tab.session.write(bytes);
        }
        cx.notify();
    }

    /// Spawns on a worker: checking and entering the start directory may block on a stalled
    /// network mount.
    /// `focus`: move keyboard focus to the new session (not when it replaces a closed last tab
    /// while the panel hides).
    fn add_session(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_spawns += 1;
        let config =
            SessionConfig { cwd: self.browser_path.clone(), shell: None, shell_integration: self.shell_integration, rows: 12, cols: 100 };
        let spawned = pool::for_path(&config.cwd).run(move || Session::spawn(config));
        cx.spawn_in(window, async move |this, cx| {
            let result = spawned.await.unwrap_or_else(|| Err(std::io::Error::other("worker stopped")));
            this.update_in(cx, |panel, window, cx| panel.session_spawned(result, focus, window, cx)).ok();
        })
        .detach();
    }

    fn session_spawned(&mut self, result: std::io::Result<Session>, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_spawns -= 1;
        let session = match result {
            Ok(session) => {
                self.error = None;
                Rc::new(session)
            }
            Err(error) => {
                self.error = Some(format!("Could not start the shell: {error}").into());
                cx.notify();
                return;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let changes = session.changes();
        let task = cx.spawn(async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this.update(cx, |panel, cx| panel.session_changed(id, cx)).is_err() {
                    break;
                }
            }
        });
        let follow = FollowController::new(session.integration());
        self.tabs.push(TerminalTab { id, session, follow, _changes: task });
        self.activate(self.tabs.len() - 1, window, cx);
        if focus {
            self.focus_handle.focus(window, cx);
        }
    }

    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            cx.emit(TerminalPanelEvent::Hide);
            if self.pending_spawns == 0 {
                self.add_session(false, window, cx);
            }
            return;
        }
        self.activate(self.active.min(self.tabs.len() - 1), window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        let mode = self.tabs.get(index).map_or(FollowMode::Independent, |tab| tab.follow.mode());
        let row = MODES.iter().position(|(m, _)| *m == mode).unwrap_or(0);
        self.mode_select.update(cx, |select, cx| select.set_selected_index(Some(IndexPath::default().row(row)), window, cx));
        cx.notify();
    }

    fn set_mode(&mut self, mode: FollowMode, cx: &mut Context<Self>) {
        let browser_path = self.browser_path.clone();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.follow.set_mode(mode);
            if let Some(bytes) = tab.follow.browser_moved(browser_path) {
                tab.session.write(bytes);
            }
        }
        cx.notify();
    }

    fn session_changed(&mut self, id: u64, cx: &mut Context<Self>) {
        let is_active = self.tabs.get(self.active).is_some_and(|tab| tab.id == id);
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        for event in tab.session.take_events() {
            let output = tab.follow.on_shell_event(event);
            if let Some(bytes) = output.write {
                tab.session.write(bytes);
            }
            if let Some(path) = output.user_cwd.filter(|_| is_active) {
                cx.emit(TerminalPanelEvent::CwdChanged(path));
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if modifiers.platform && keystroke.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let bytes = tab.session.paste_bytes(&text);
                tab.follow.on_user_input(&bytes);
                tab.session.write(bytes);
            }
            cx.stop_propagation();
            return;
        }
        let mods = KeyMods { control: modifiers.control, alt: modifiers.alt, shift: modifiers.shift, platform: modifiers.platform };
        if let Some(bytes) = encode_key(&keystroke.key, keystroke.key_char.as_deref(), mods, tab.session.app_cursor()) {
            tab.follow.on_user_input(&bytes);
            tab.session.write(bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        self.scroll_remainder += event.delta.pixel_delta(px(LINE_HEIGHT)).y / px(LINE_HEIGHT);
        let lines = self.scroll_remainder.trunc();
        if lines != 0. {
            self.scroll_remainder -= lines;
            tab.session.scroll(lines as i32);
            cx.notify();
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tabs.get(self.active);
        let unsupported = tab.and_then(|tab| match tab.session.integration() {
            Integration::Unsupported(reason) => Some(SharedString::from(reason.clone())),
            Integration::Supported(_) => None,
        });
        let cwd = tab.and_then(|tab| tab.follow.cwd().map(display_path)).unwrap_or_default();
        let status = tab.map(|tab| prompt_status(&tab.follow)).unwrap_or_default();
        let sync_path = self.browser_path.clone();

        h_flex()
            .flex_none()
            .h(px(32.))
            .px_2()
            .gap_2()
            .bg(rgb(colors::PANEL))
            .border_b_1()
            .border_color(rgb(colors::LINE))
            .text_sm()
            .child(div().font_weight(FontWeight::BOLD).child("Terminal"))
            .child(self.render_session_tabs(cx))
            .child(div().min_w_0().truncate().text_color(rgb(colors::MUTED)).child(cwd))
            .child(div().flex_1())
            .when_some(self.error.clone(), |row, error| row.child(div().text_color(rgb(colors::MUTED)).child(error)))
            .when_some(unsupported.clone(), |row, reason| row.child(div().truncate().text_color(rgb(colors::MUTED)).child(reason)))
            .child(div().text_color(rgb(colors::ACCENT)).child(status))
            .child(div().text_color(rgb(colors::MUTED)).child(format!("Bound to {}", self.binding_label)))
            .child(Select::new(&self.mode_select).small().w(px(140.)).accessibility_label("Follow mode").disabled(unsupported.is_some()))
            .child(
                Button::new("terminal-sync-now")
                    .small()
                    .label("Sync now")
                    .disabled(unsupported.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| this.sync_now(sync_path.clone(), cx))),
            )
            .child(
                Button::new("terminal-close")
                    .small()
                    .ghost()
                    .label("×")
                    .tooltip("Hide terminal")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalPanelEvent::Hide))),
            )
    }

    fn render_session_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let exited = if tab.session.is_exited() { " (exited)" } else { "" };
                h_flex()
                    .child(
                        Button::new(("terminal-tab", tab.id))
                            .xsmall()
                            .label(format!("{}{exited}", index + 1))
                            .when(index == self.active, |button| button.primary())
                            .on_click(cx.listener(move |this, _, window, cx| this.activate(index, window, cx))),
                    )
                    .child(
                        Button::new(("terminal-tab-close", tab.id))
                            .xsmall()
                            .ghost()
                            .label("×")
                            .tooltip("Close session")
                            .on_click(cx.listener(move |this, _, window, cx| this.close_session(index, window, cx))),
                    )
            }))
            .child(
                Button::new("terminal-new-session")
                    .xsmall()
                    .ghost()
                    .label("+")
                    .tooltip("New session")
                    .on_click(cx.listener(|this, _, window, cx| this.add_session(true, window, cx))),
            )
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let family = cx.theme().mono_font_family.clone();
        let mut grid = div().id("terminal-grid").relative().flex_1().min_h_0().overflow_hidden().px_2().py_1();
        let Some(tab) = self.tabs.get(self.active) else {
            return grid;
        };
        let snapshot = tab.session.snapshot();
        let session = Rc::clone(&tab.session);
        let measure_font = font(family.clone());
        grid = grid.child(
            canvas(
                move |bounds, window, _| {
                    let text_system = window.text_system();
                    let font_id = text_system.resolve_font(&measure_font);
                    let cell_width = text_system.advance(font_id, px(FONT_SIZE), 'M').map_or(px(7.), |size| size.width);
                    let cols = (bounds.size.width / cell_width).floor().max(1.) as u16;
                    let rows = (bounds.size.height / px(LINE_HEIGHT)).floor().max(1.) as u16;
                    session.resize(rows, cols);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
        grid.children(snapshot_rows(&snapshot, &family))
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().bg(rgb(colors::TERMINAL_BG)).text_color(rgb(colors::TEXT)).child(self.render_header(cx)).child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .track_focus(&self.focus_handle)
                .key_context("Terminal")
                .on_key_down(cx.listener(Self::on_key_down))
                .on_scroll(cx.listener(Self::on_scroll))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)))
                .child(self.render_grid(cx)),
        )
    }
}

fn prompt_status(follow: &FollowController) -> String {
    let busy = if follow.prompt_state() == PromptState::Busy { "Busy" } else { "" };
    match follow.queued() {
        Some(path) => format!("{busy} Waiting to cd {}", display_path(path)).trim().to_owned(),
        None => busy.to_owned(),
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn snapshot_rows(snapshot: &TerminalSnapshot, family: &SharedString) -> Vec<AnyElement> {
    let cursor = snapshot.cursor.filter(|_| !snapshot.exited);
    snapshot
        .rows
        .iter()
        .enumerate()
        .map(|(row_index, row)| {
            let cursor_col = cursor.filter(|(row, _)| *row == row_index).map(|(_, col)| col);
            let mut text = String::with_capacity(row.len());
            let mut runs: Vec<TextRun> = Vec::new();
            let mut current: Option<(CellStyle, usize)> = None;
            for (col, cell) in row.iter().enumerate() {
                let mut style = cell.style;
                if cursor_col == Some(col) {
                    style.inverse = !style.inverse;
                }
                let start = text.len();
                text.push(cell.ch);
                let len = text.len() - start;
                match &mut current {
                    Some((run_style, run_len)) if *run_style == style => *run_len += len,
                    _ => {
                        if let Some((run_style, run_len)) = current.take() {
                            runs.push(text_run(run_style, run_len, family));
                        }
                        current = Some((style, len));
                    }
                }
            }
            if let Some((run_style, run_len)) = current {
                runs.push(text_run(run_style, run_len, family));
            }
            div()
                .h(px(LINE_HEIGHT))
                .text_size(px(FONT_SIZE))
                .line_height(px(LINE_HEIGHT))
                .whitespace_nowrap()
                .child(StyledText::new(text).with_runs(runs))
                .into_any_element()
        })
        .collect()
}

fn text_run(style: CellStyle, len: usize, family: &SharedString) -> TextRun {
    let mut fg = style.fg.unwrap_or(colors::TEXT);
    let mut bg = style.bg;
    if style.inverse {
        let swapped = bg.unwrap_or(colors::TERMINAL_BG);
        bg = Some(fg);
        fg = swapped;
    }
    let mut run_font = font(family.clone());
    if style.bold {
        run_font.weight = FontWeight::BOLD;
    }
    if style.italic {
        run_font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font: run_font,
        color: rgb(fg).into(),
        background_color: bg.map(|bg| rgb(bg).into()),
        underline: style.underline.then(|| UnderlineStyle { thickness: px(1.), color: None, wavy: false }),
        strikethrough: None,
    }
}

/// Test seam: the headless UI tests read follow state and the screen, and pick a mode the way
/// the dropdown does.
#[cfg(test)]
impl TerminalPanel {
    pub(crate) fn choose_mode(&self, label: &'static str, cx: &mut App) {
        self.mode_select.update(cx, |_, cx| cx.emit(SelectEvent::Confirm(Some(label))));
    }

    pub(crate) fn active_follow(&self) -> Option<&FollowController> {
        self.tabs.get(self.active).map(|tab| &tab.follow)
    }

    /// The active session's text, scrollback included. The session only shows one screen, so this
    /// scrolls to the top and back down a line at a time; the live screen is reached when the view,
    /// cursor included, equals the one taken at the bottom.
    pub(crate) fn active_text(&self) -> String {
        use wayfold_core::terminal::SCROLLBACK_LINES;
        let Some(tab) = self.tabs.get(self.active) else {
            return String::new();
        };
        let session = &tab.session;
        let view_key = |view: &TerminalSnapshot| (view.text_lines(), view.cursor);
        let live = view_key(&session.snapshot());
        session.scroll(SCROLLBACK_LINES as i32);
        let mut view = session.snapshot();
        let mut lines = view.text_lines();
        let mut steps = 0;
        while view_key(&view) != live {
            assert!(steps < SCROLLBACK_LINES, "the live screen changed while reading the scrollback");
            session.scroll(-1);
            view = session.snapshot();
            lines.extend(view.text_lines().pop());
            steps += 1;
        }
        lines.join("\n")
    }

    /// Drops every session (killing its shell) without spawning a new one, unlike closing the last tab.
    pub(crate) fn close_all_sessions(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear();
        cx.notify();
    }
}
