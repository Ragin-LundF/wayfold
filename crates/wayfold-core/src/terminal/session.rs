//! One shell in a PTY (T01): spawns it, parses its output into a grid with scrollback,
//! takes input and resizes. Reading and writing run on their own threads, so the UI never
//! blocks on the shell.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{ChildKiller, MasterPty, PtySize, native_pty_system};

use super::osc::{OscScanner, ShellEvent};
use super::shell::{Integration, login_shell, shell_command};
use super::snapshot::{TerminalSnapshot, capture};

pub const SCROLLBACK_LINES: usize = 10_000;

pub struct SessionConfig {
    pub cwd: PathBuf,
    /// `None`: the user's login shell.
    pub shell: Option<PathBuf>,
    /// The user's opt-in for OSC 7 / OSC 133 shell integration.
    pub shell_integration: bool,
    pub rows: u16,
    pub cols: u16,
}

pub struct Session {
    shared: Arc<Mutex<Shared>>,
    input: mpsc::Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    integration: Integration,
    changed: async_channel::Receiver<()>,
}

struct Shared {
    term: Term<Responder>,
    parser: Processor,
    scanner: OscScanner,
    events: Vec<ShellEvent>,
    exited: bool,
}

/// Sends the terminal's answers to queries (cursor position, device attributes) to the shell.
struct Responder(mpsc::Sender<Vec<u8>>);

impl EventListener for Responder {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let _ = self.0.send(text.into_bytes());
        }
    }
}

struct GridSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl Session {
    pub fn spawn(config: SessionConfig) -> io::Result<Self> {
        let shell = config.shell.unwrap_or_else(login_shell);
        let nonce = new_nonce();
        let (mut command, integration) = shell_command(&shell, config.shell_integration, &nonce);
        let cwd = start_dir(&config.cwd);
        // Shells keep a matching $PWD as the logical path, so OSC 7 reports the browser's path.
        command.env("PWD", &cwd);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        if ["LANG", "LC_ALL", "LC_CTYPE"].iter().all(|name| std::env::var_os(name).is_none()) {
            // Apps launched from Finder have no locale; the line editor needs UTF-8.
            command.env("LANG", "en_US.UTF-8");
        }

        let (rows, cols) = (config.rows.max(1), config.cols.max(1));
        let pair = native_pty_system().openpty(pty_size(rows, cols)).map_err(io::Error::other)?;
        let mut child = pair.slave.spawn_command(command).map_err(io::Error::other)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;

        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        let (changed_tx, changed) = async_channel::bounded(1);
        let term_config = Config { scrolling_history: SCROLLBACK_LINES, osc52: Osc52::Disabled, ..Config::default() };
        let size = GridSize { rows: usize::from(rows), cols: usize::from(cols) };
        let shared = Arc::new(Mutex::new(Shared {
            term: Term::new(term_config, &size, Responder(input.clone())),
            parser: Processor::new(),
            scanner: OscScanner::new(local_host_name(), &nonce),
            events: Vec::new(),
            exited: false,
        }));

        thread::Builder::new().name("terminal-writer".into()).spawn(move || write_loop(writer, input_rx))?;
        let killer = child.clone_killer();
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new().name("terminal-reader".into()).spawn(move || {
            read_loop(reader, &reader_shared, &changed_tx);
            let _ = child.wait();
            lock(&reader_shared).exited = true;
            let _ = changed_tx.try_send(());
        })?;

        Ok(Self { shared, input, master: pair.master, killer, integration, changed })
    }

    /// Fires (coalesced) whenever the screen, shell events or exit state changed.
    pub fn changes(&self) -> async_channel::Receiver<()> {
        self.changed.clone()
    }

    pub fn integration(&self) -> &Integration {
        &self.integration
    }

    /// Sends input bytes; jumps back to the live screen if scrolled into history.
    pub fn write(&self, bytes: Vec<u8>) {
        lock(&self.shared).term.scroll_display(Scroll::Bottom);
        let _ = self.input.send(bytes);
    }

    /// The bytes to `write` for pasting text: bracketed when the program asked for it, so
    /// newlines do not run commands.
    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    /// Whether arrow keys must use application mode (DECCKM).
    pub fn app_cursor(&self) -> bool {
        self.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let mut shared = lock(&self.shared);
        if shared.term.screen_lines() == usize::from(rows) && shared.term.columns() == usize::from(cols) {
            return;
        }
        shared.term.resize(GridSize { rows: usize::from(rows), cols: usize::from(cols) });
        let _ = self.master.resize(pty_size(rows, cols));
    }

    /// Scrolls the view into the scrollback; positive goes up.
    pub fn scroll(&self, lines: i32) {
        lock(&self.shared).term.scroll_display(Scroll::Delta(lines));
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let shared = lock(&self.shared);
        capture(&shared.term, shared.exited)
    }

    pub fn is_exited(&self) -> bool {
        lock(&self.shared).exited
    }

    /// Shell-integration events since the last call, in order.
    pub fn take_events(&self) -> Vec<ShellEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }

    fn mode(&self) -> TermMode {
        *lock(&self.shared).term.mode()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn read_loop(mut reader: Box<dyn Read + Send>, shared: &Mutex<Shared>, changed: &async_channel::Sender<()>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let bytes = &buf[..count];
        let mut guard = lock(shared);
        let Shared { term, parser, scanner, events, .. } = &mut *guard;
        scanner.scan(bytes, events);
        parser.advance(term, bytes);
        drop(guard);
        let _ = changed.try_send(());
    }
}

fn write_loop(mut writer: Box<dyn Write + Send>, input: mpsc::Receiver<Vec<u8>>) {
    while let Ok(bytes) = input.recv() {
        if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
            return;
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn start_dir(cwd: &Path) -> PathBuf {
    if cwd.is_dir() {
        return cwd.to_path_buf();
    }
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// 128 random bits as hex: signs the shell-integration markers of one session.
fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for its length; arc4random_buf cannot fail.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn local_host_name() -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates on success.
    let result = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if result != 0 {
        return Vec::new();
    }
    buf.iter().take_while(|&&b| b != 0).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(session: &Session, what: &str, condition: impl Fn(&TerminalSnapshot) -> bool) -> TerminalSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = session.snapshot();
            if condition(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {:#?}", snapshot.text_lines());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn spawn(shell: &str, cwd: &Path, integration: bool) -> Session {
        let config =
            SessionConfig { cwd: cwd.to_path_buf(), shell: Some(shell.into()), shell_integration: integration, rows: 24, cols: 80 };
        Session::spawn(config).expect("spawn shell")
    }

    #[test]
    fn sh_echo_appears_in_snapshot_and_exit_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = spawn("/bin/sh", dir.path(), true);
        assert!(matches!(session.integration(), Integration::Unsupported(_)));
        session.write(b"echo hi$((40+2))\r".to_vec());
        wait_for(&session, "echo output", |snap| snap.text_lines().iter().any(|line| line == "hi42"));
        session.resize(10, 40);
        assert_eq!(session.snapshot().rows.len(), 10);
        assert_eq!(session.snapshot().rows[0].len(), 40);
        session.write(b"exit\r".to_vec());
        wait_for(&session, "exit", |snap| snap.exited);
    }

    #[test]
    fn scrollback_is_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = spawn("/bin/sh", dir.path(), false);
        session.write(b"i=0; while [ $i -lt 10100 ]; do echo line$i; i=$((i+1)); done; echo done\r".to_vec());
        wait_for(&session, "loop end", |snap| snap.text_lines().iter().any(|line| line == "done"));
        let shared = lock(&session.shared);
        assert!(shared.term.grid().history_size() <= SCROLLBACK_LINES);
        assert!(shared.term.grid().history_size() >= SCROLLBACK_LINES - 24);
    }

    #[test]
    fn zsh_integration_reports_cwd_and_prompt() {
        let root = tempfile::tempdir().expect("tempdir");
        let start = root.path().join("start dir");
        let target = root.path().join("it's a \"dir\"\nwith newline");
        std::fs::create_dir(&start).expect("start");
        std::fs::create_dir(&target).expect("target");
        // An empty ZDOTDIR keeps the user's own config out of the test.
        let session = {
            let home = root.path().join("home");
            std::fs::create_dir(&home).expect("home");
            // SAFETY: test-only; read by the integration setup during this spawn only.
            unsafe { std::env::set_var("ZDOTDIR", &home) };
            spawn("/bin/zsh", &start, true)
        };
        assert_eq!(session.integration(), &Integration::Supported(super::super::shell::ShellKind::Zsh));
        let mut follow = super::super::follow::FollowController::new(session.integration());
        follow.set_mode(super::super::follow::FollowMode::TwoWay);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut injected = false;
        loop {
            for event in session.take_events() {
                let out = follow.on_shell_event(event);
                assert_eq!(out.user_cwd, None, "own cd must not come back as a user change");
                if let Some(bytes) = out.write {
                    session.write(bytes);
                }
            }
            if follow.cwd() == Some(start.as_path()) && !injected {
                injected = true;
                if let Some(bytes) = follow.browser_moved(target.clone()) {
                    session.write(bytes);
                }
            }
            if injected && follow.cwd() == Some(target.as_path()) && follow.prompt_state() != super::super::follow::PromptState::Busy {
                break;
            }
            assert!(Instant::now() < deadline, "no cwd report: {:?} {:#?}", follow.cwd(), session.snapshot().text_lines());
            thread::sleep(Duration::from_millis(20));
        }

        // Output of a command cannot forge markers, and the nonce never reaches children.
        session.write(
            b"printf '\\e]133;D\\a\\e]7;file:///forged\\a\\e]133;A\\a'; printf 'env-%s\\n' \"$(env | grep -c WAYFOLD_NONCE)\"\r".to_vec(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !session.snapshot().text_lines().iter().any(|line| line == "env-0") {
            for event in session.take_events() {
                follow.on_shell_event(event);
            }
            assert!(Instant::now() < deadline, "{:#?}", session.snapshot().text_lines());
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(100));
        for event in session.take_events() {
            follow.on_shell_event(event);
        }
        assert_eq!(follow.cwd(), Some(target.as_path()));
    }
}
