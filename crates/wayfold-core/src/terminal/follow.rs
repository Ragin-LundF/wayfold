//! Directory sync between the browser and a shell (T02). Pure state machine: it decides what
//! to type and what to report; the caller does the I/O. It only ever types at a verified idle,
//! empty prompt, so a running command never receives an injected `cd`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::osc::ShellEvent;
use super::shell::{Integration, ShellKind, cd_command};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FollowMode {
    #[default]
    Independent,
    FollowBrowser,
    TwoWay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptState {
    /// No verified prompt yet, or a command may be starting.
    Unknown,
    Idle,
    Busy,
}

/// What the caller must do after feeding a shell event.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FollowOutput {
    /// Bytes to write to the PTY.
    pub write: Option<Vec<u8>>,
    /// The user changed directory in the shell; set only in two-way mode.
    pub user_cwd: Option<PathBuf>,
}

pub struct FollowController {
    mode: FollowMode,
    shell: Option<ShellKind>,
    prompt: PromptState,
    line_dirty: bool,
    queued: Option<PathBuf>,
    injected: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

impl FollowController {
    pub fn new(integration: &Integration) -> Self {
        let shell = match integration {
            Integration::Supported(kind) => Some(*kind),
            Integration::Unsupported(_) => None,
        };
        Self {
            mode: FollowMode::Independent,
            shell,
            prompt: PromptState::Unknown,
            line_dirty: false,
            queued: None,
            injected: None,
            cwd: None,
        }
    }

    pub fn mode(&self) -> FollowMode {
        self.mode
    }

    /// Sync modes need integration; without it the mode stays independent.
    pub fn set_mode(&mut self, mode: FollowMode) {
        self.mode = if self.shell.is_some() { mode } else { FollowMode::Independent };
        if self.mode == FollowMode::Independent {
            self.queued = None;
        }
    }

    pub fn prompt_state(&self) -> PromptState {
        self.prompt
    }

    /// Directory waiting for the shell to become idle.
    pub fn queued(&self) -> Option<&Path> {
        self.queued.as_deref()
    }

    /// The shell's last reported directory.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The browser moved; follows it unless the mode is independent.
    pub fn browser_moved(&mut self, path: PathBuf) -> Option<Vec<u8>> {
        if self.mode == FollowMode::Independent {
            return None;
        }
        self.request_cd(path)
    }

    /// Explicit "Sync now": works in every mode, still only at an idle prompt.
    pub fn request_cd(&mut self, path: PathBuf) -> Option<Vec<u8>> {
        self.shell?;
        if !path.is_absolute() {
            return None;
        }
        if self.injected.is_none() && self.cwd.as_deref().is_some_and(|cwd| same_dir(cwd, &path)) {
            self.queued = None;
            return None;
        }
        self.queued = Some(path); // latest wins
        self.flush()
    }

    /// Call with every byte sequence written to the terminal for the user (keys and pastes,
    /// as sent, so a bracketed paste keeps its markers).
    pub fn on_user_input(&mut self, bytes: &[u8]) {
        let line_break = bytes.iter().rposition(|&b| b == b'\r' || b == b'\n' || b == 0x03);
        match line_break {
            Some(index) => {
                // Enter or Ctrl-C: a command may be starting; text after the last break
                // (a pasted `echo hi\nrm -rf `) is still on the new line.
                self.line_dirty = index + 1 < bytes.len();
                self.prompt = PromptState::Unknown;
            }
            None if !bytes.is_empty() => self.line_dirty = true,
            None => {}
        }
    }

    pub fn on_shell_event(&mut self, event: ShellEvent) -> FollowOutput {
        match event {
            // A running command printed it (only a signed `D` ends a command), or it is the
            // `B` of the prompt that just received our cd, which is still pending.
            ShellEvent::PromptStart | ShellEvent::InputStart
                if self.prompt == PromptState::Busy || (event == ShellEvent::InputStart && self.injected.is_some()) =>
            {
                FollowOutput::default()
            }
            ShellEvent::PromptStart | ShellEvent::InputStart => {
                if event == ShellEvent::PromptStart {
                    // The prompt after our cd: whatever happened to it, it is over.
                    self.injected = None;
                }
                // A dirty line stays dirty: text typed while busy lands in the new prompt.
                self.prompt = PromptState::Idle;
                FollowOutput { write: self.flush(), user_cwd: None }
            }
            ShellEvent::CommandStart => {
                self.prompt = PromptState::Busy;
                FollowOutput::default()
            }
            ShellEvent::CommandDone => {
                self.prompt = PromptState::Unknown;
                FollowOutput::default()
            }
            ShellEvent::Cwd(path) => self.on_cwd(path),
        }
    }

    fn on_cwd(&mut self, path: PathBuf) -> FollowOutput {
        let previous = self.cwd.replace(path.clone());
        let from_injection = self.injected.as_ref().is_some_and(|injected| same_dir(injected, &path));
        if from_injection {
            self.injected = None;
            return FollowOutput::default();
        }
        let changed = previous.is_some_and(|previous| !same_dir(&previous, &path));
        let user_cwd = (changed && self.mode == FollowMode::TwoWay).then_some(path);
        FollowOutput { write: None, user_cwd }
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        let shell = self.shell?;
        if self.prompt != PromptState::Idle || self.line_dirty {
            return None;
        }
        let path = self.queued.take()?;
        // Typed command is running until the next prompt; further requests queue up.
        self.prompt = PromptState::Unknown;
        let command = cd_command(shell, &path);
        self.injected = Some(path);
        Some(command)
    }
}

/// Lexical, never touching the disk (a stalled network mount would freeze the UI thread).
/// `/tmp`, `/var` and `/etc` are the same directories as their `/private` targets on macOS.
fn same_dir(a: &Path, b: &Path) -> bool {
    without_private_prefix(a) == without_private_prefix(b)
}

fn without_private_prefix(path: &Path) -> &Path {
    let Some(rest) = path.as_os_str().as_bytes().strip_prefix(b"/private") else {
        return path;
    };
    let rest = Path::new(OsStr::from_bytes(rest));
    if ["/tmp", "/var", "/etc"].iter().any(|dir| rest.starts_with(dir)) { rest } else { path }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_zsh(mode: FollowMode) -> FollowController {
        let mut follow = FollowController::new(&Integration::Supported(ShellKind::Zsh));
        follow.set_mode(mode);
        follow.on_shell_event(ShellEvent::Cwd("/start".into()));
        follow.on_shell_event(ShellEvent::PromptStart);
        follow.on_shell_event(ShellEvent::InputStart);
        follow
    }

    #[test]
    fn idle_prompt_gets_cd_immediately() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        assert_eq!(follow.browser_moved("/a b".into()), Some(b"cd -- $'/a b'\r".to_vec()));
        assert_eq!(follow.prompt_state(), PromptState::Unknown);
    }

    #[test]
    fn busy_shell_queues_latest_and_flushes_at_next_prompt() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"sleep 10\r");
        follow.on_shell_event(ShellEvent::CommandStart);
        assert_eq!(follow.prompt_state(), PromptState::Busy);
        assert_eq!(follow.browser_moved("/one".into()), None);
        assert_eq!(follow.browser_moved("/two".into()), None);
        assert_eq!(follow.queued(), Some(Path::new("/two")));
        assert_eq!(follow.on_shell_event(ShellEvent::CommandDone), FollowOutput::default());
        assert_eq!(follow.on_shell_event(ShellEvent::Cwd("/start".into())), FollowOutput::default());
        let out = follow.on_shell_event(ShellEvent::PromptStart);
        assert_eq!(out.write, Some(b"cd -- $'/two'\r".to_vec()));
        assert_eq!(follow.queued(), None);
    }

    #[test]
    fn dirty_line_is_never_overwritten() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"git sta");
        assert_eq!(follow.browser_moved("/x".into()), None);
        assert_eq!(follow.queued(), Some(Path::new("/x")));
        follow.on_user_input(b"\x7f");
        assert_eq!(follow.request_cd("/y".into()), None, "backspace may leave text; stay careful");
        follow.on_user_input(b"\x03");
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, Some(b"cd -- $'/y'\r".to_vec()));
    }

    #[test]
    fn text_typed_while_busy_blocks_injection_at_next_prompt() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"make\r");
        follow.on_shell_event(ShellEvent::CommandStart);
        follow.on_user_input(b"ls");
        assert_eq!(follow.browser_moved("/x".into()), None);
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, None);
        assert_eq!(follow.queued(), Some(Path::new("/x")));
    }

    #[test]
    fn unknown_state_never_injects() {
        let mut follow = FollowController::new(&Integration::Supported(ShellKind::Bash));
        follow.set_mode(FollowMode::TwoWay);
        assert_eq!(follow.browser_moved("/x".into()), None);
        let mut unsupported = FollowController::new(&Integration::Unsupported("no".into()));
        unsupported.set_mode(FollowMode::TwoWay);
        assert_eq!(unsupported.mode(), FollowMode::Independent);
        unsupported.on_shell_event(ShellEvent::PromptStart);
        assert_eq!(unsupported.request_cd("/x".into()), None);
    }

    #[test]
    fn injected_cd_is_not_reported_back_but_user_cd_is() {
        let mut follow = idle_zsh(FollowMode::TwoWay);
        assert!(follow.browser_moved("/target".into()).is_some());
        follow.on_shell_event(ShellEvent::CommandStart);
        follow.on_shell_event(ShellEvent::CommandDone);
        assert_eq!(follow.on_shell_event(ShellEvent::Cwd("/target".into())).user_cwd, None);
        follow.on_shell_event(ShellEvent::PromptStart);
        // Browser echoes the shell's own cwd back: nothing is typed.
        assert_eq!(follow.browser_moved("/target".into()), None);

        follow.on_user_input(b"cd /elsewhere\r");
        follow.on_shell_event(ShellEvent::CommandStart);
        follow.on_shell_event(ShellEvent::CommandDone);
        let out = follow.on_shell_event(ShellEvent::Cwd("/elsewhere".into()));
        assert_eq!(out.user_cwd, Some(PathBuf::from("/elsewhere")));
    }

    #[test]
    fn follow_mode_reports_no_user_cd_and_independent_ignores_browser() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        assert_eq!(follow.on_shell_event(ShellEvent::Cwd("/other".into())).user_cwd, None);
        let mut independent = idle_zsh(FollowMode::Independent);
        assert_eq!(independent.browser_moved("/x".into()), None);
        assert_eq!(independent.request_cd("/x".into()), Some(b"cd -- $'/x'\r".to_vec()), "sync now");
    }

    #[test]
    fn failed_injection_does_not_swallow_later_user_cd() {
        let mut follow = idle_zsh(FollowMode::TwoWay);
        assert!(follow.browser_moved("/missing".into()).is_some());
        follow.on_shell_event(ShellEvent::Cwd("/start".into()));
        follow.on_shell_event(ShellEvent::PromptStart);
        follow.on_user_input(b"cd /missing\r");
        let out = follow.on_shell_event(ShellEvent::Cwd("/missing".into()));
        assert_eq!(out.user_cwd, Some(PathBuf::from("/missing")));
    }

    #[test]
    fn text_pasted_after_a_line_break_keeps_the_line_dirty() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"echo hi\rrm -rf ");
        follow.on_shell_event(ShellEvent::CommandStart);
        follow.on_shell_event(ShellEvent::CommandDone);
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, None);
        assert_eq!(follow.browser_moved("/x".into()), None);
        assert_eq!(follow.queued(), Some(Path::new("/x")));

        let mut ended = idle_zsh(FollowMode::FollowBrowser);
        ended.on_user_input(b"echo a\necho b\n");
        ended.on_shell_event(ShellEvent::PromptStart);
        assert!(ended.browser_moved("/x".into()).is_some());
    }

    #[test]
    fn bracketed_paste_with_newlines_stays_dirty() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"\x1b[200~echo hi\nrm -rf \x1b[201~");
        assert_eq!(follow.browser_moved("/x".into()), None);
        follow.on_user_input(b"\x1b[200~ls\n\x1b[201~");
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, None);
    }

    #[test]
    fn input_start_of_the_flushing_prompt_does_not_allow_a_second_cd() {
        let mut follow = idle_zsh(FollowMode::TwoWay);
        follow.on_user_input(b"make\r");
        follow.on_shell_event(ShellEvent::CommandStart);
        assert_eq!(follow.browser_moved("/one".into()), None);
        follow.on_shell_event(ShellEvent::CommandDone);
        let flushed = follow.on_shell_event(ShellEvent::PromptStart);
        assert_eq!(flushed.write, Some(b"cd -- $'/one'\r".to_vec()));
        assert_eq!(follow.on_shell_event(ShellEvent::InputStart).write, None);
        assert_eq!(follow.prompt_state(), PromptState::Unknown);
        assert_eq!(follow.browser_moved("/two".into()), None, "cd still pending");
        assert_eq!(follow.on_shell_event(ShellEvent::Cwd("/one".into())).user_cwd, None);
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, Some(b"cd -- $'/two'\r".to_vec()));
    }

    #[test]
    fn prompt_markers_while_busy_wait_for_command_done() {
        let mut follow = idle_zsh(FollowMode::FollowBrowser);
        follow.on_user_input(b"ssh host\r");
        follow.on_shell_event(ShellEvent::CommandStart);
        assert_eq!(follow.browser_moved("/x".into()), None);
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, None);
        assert_eq!(follow.on_shell_event(ShellEvent::InputStart).write, None);
        assert_eq!(follow.prompt_state(), PromptState::Busy);
        follow.on_shell_event(ShellEvent::CommandDone);
        assert_eq!(follow.on_shell_event(ShellEvent::PromptStart).write, Some(b"cd -- $'/x'\r".to_vec()));
    }

    #[test]
    fn same_dir_is_lexical_and_knows_private_aliases() {
        assert!(same_dir(Path::new("/private/tmp/a"), Path::new("/tmp/a")));
        assert!(same_dir(Path::new("/var/x"), Path::new("/private/var/x")));
        assert!(same_dir(Path::new("/private/etc"), Path::new("/etc/")));
        assert!(!same_dir(Path::new("/private/tmpx"), Path::new("/tmpx")));
        assert!(!same_dir(Path::new("/private/Users"), Path::new("/Users")));
        assert!(!same_dir(Path::new("/privatetmp"), Path::new("/tmp")));
        // Nothing on disk: a missing path is compared, never resolved.
        assert!(same_dir(Path::new("/Volumes/stalled/a/./b"), Path::new("/Volumes/stalled/a/b")));
    }
}
