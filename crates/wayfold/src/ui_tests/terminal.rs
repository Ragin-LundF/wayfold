//! U10: terminal follow modes through the app wiring (Workspace -> TerminalPanel -> FollowController
//! and back). A real `/bin/zsh` with shell integration runs in a PTY under the temp HOME, so the
//! cd lines and the signed OSC 7 / OSC 133 markers are the real ones. Injected cds are counted in
//! the terminal text, scrollback included (`cd -- ` only ever comes from the controller; the tests
//! type `cd x`).

use super::harness::{Env, keys, open, settle, settle_until};
use crate::terminal_view::TerminalPanel;
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::path::{Path, PathBuf};
use wayfold_core::terminal::{FollowMode, PromptState};

/// Opens the workspace on `env.work()` with the terminal shown and `mode` chosen in the dropdown.
fn open_terminal<'a>(
    cx: &'a mut TestAppContext,
    env: &Env,
    mode: &'static str,
) -> (Entity<Workspace>, Entity<TerminalPanel>, &'a mut VisualTestContext) {
    // SAFETY: serialized by the Env lock, and like HOME the SHELL/ZDOTDIR changes persist on purpose.
    // ZDOTDIR would point zsh at the user's real config.
    unsafe {
        std::env::set_var("SHELL", "/bin/zsh");
        std::env::remove_var("ZDOTDIR");
    }
    let (ws, cx) = open(cx, env);
    ws.update_in(cx, |w, window, cx| {
        w.prefs.shell_integration = true;
        w.toggle_terminal(window, cx);
    });
    let panel = ws.read_with(cx, |w, _| w.terminal.clone().expect("terminal panel"));
    let work = env.work();
    settle_until(&ws, cx, "idle zsh prompt in the work folder", |cx| {
        panel.read_with(cx, |p, _| p.active_follow().is_some_and(|f| f.prompt_state() == PromptState::Idle && f.cwd() == Some(&work)))
    });
    panel.update(cx, |p, cx| p.choose_mode(mode, cx));
    settle(&ws, cx);
    (ws, panel, cx)
}

fn shell_cwd(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> Option<PathBuf> {
    panel.read_with(cx, |p, _| p.active_follow().and_then(|f| f.cwd().map(Path::to_path_buf)))
}

fn injected_cds(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> usize {
    panel.read_with(cx, |p, _| p.active_text().matches("cd -- ").count())
}

fn wait_for_shell_cwd(ws: &Entity<Workspace>, panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext, dir: &Path) {
    settle_until(ws, cx, &format!("shell cwd {}", dir.display()), |cx| shell_cwd(panel, cx).as_deref() == Some(dir));
}

/// Kills the shell: the panel keeps its sessions until the test binary exits otherwise.
fn close_terminal(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) {
    panel.update(cx, |p, cx| p.close_all_sessions(cx));
    cx.run_until_parked();
}

fn location(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> PathBuf {
    ws.read_with(cx, |w, _| w.tab().location.clone())
}

fn mode(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> Option<FollowMode> {
    panel.read_with(cx, |p, _| p.active_follow().map(|f| f.mode()))
}

/// No cd was written: an injection leaves the prompt `Unknown` at once, so this needs no timing.
fn assert_nothing_injected(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) {
    panel.read_with(cx, |p, _| {
        let follow = p.active_follow().unwrap();
        assert_eq!(follow.prompt_state(), PromptState::Idle);
        assert_eq!(follow.queued(), None);
    });
}

#[gpui_kit::test]
fn u10_independent_never_follows_but_sync_now_does(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let a = env.work().join("a");
    std::fs::create_dir(&a).unwrap();
    let (ws, panel, cx) = open_terminal(cx, &env, "Independent");
    assert_eq!(mode(&panel, cx), Some(FollowMode::Independent));

    ws.update(cx, |w, cx| w.navigate(a.clone(), cx));
    settle(&ws, cx);
    assert_nothing_injected(&panel, cx);
    assert_eq!(shell_cwd(&panel, cx), Some(env.work()));
    assert_eq!(injected_cds(&panel, cx), 0);

    // "Sync now" works in every mode.
    panel.update(cx, |p, cx| p.sync_now(a.clone(), cx));
    wait_for_shell_cwd(&ws, &panel, cx, &a);
    assert_eq!(injected_cds(&panel, cx), 1);
    close_terminal(&panel, cx);
}

#[gpui_kit::test]
fn u10_follow_browser_cds_at_idle_prompt_and_queues_while_busy(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let (a, b) = (env.work().join("a"), env.work().join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir_all(b.join("c")).unwrap();
    let (ws, panel, cx) = open_terminal(cx, &env, "Follow browser");
    assert_eq!(mode(&panel, cx), Some(FollowMode::FollowBrowser));
    assert_eq!(injected_cds(&panel, cx), 0, "choosing the mode in the current folder types nothing");

    ws.update(cx, |w, cx| w.navigate(a.clone(), cx));
    wait_for_shell_cwd(&ws, &panel, cx, &a);
    assert_eq!(injected_cds(&panel, cx), 1);
    assert_eq!(location(&ws, cx), a);

    keys(&ws, cx, "s l e e p space 4 enter");
    settle_until(&ws, cx, "busy shell", |cx| panel.read_with(cx, |p, _| p.active_follow().unwrap().prompt_state() == PromptState::Busy));
    ws.update(cx, |w, cx| w.navigate(b.clone(), cx));
    settle(&ws, cx);
    assert_eq!(panel.read_with(cx, |p, _| p.active_follow().unwrap().queued().map(Path::to_path_buf)), Some(b.clone()));
    assert_eq!(injected_cds(&panel, cx), 1, "a running command never gets a cd typed into it");
    assert_eq!(shell_cwd(&panel, cx), Some(a.clone()));

    // Interrupt the sleep: its end brings the idle prompt, which runs the queued cd.
    keys(&ws, cx, "ctrl-c");
    wait_for_shell_cwd(&ws, &panel, cx, &b);
    assert_eq!(injected_cds(&panel, cx), 2);

    // Follow browser is one-way: a cd typed in the shell leaves the browser where it is.
    keys(&ws, cx, "c d space c enter");
    wait_for_shell_cwd(&ws, &panel, cx, &b.join("c"));
    settle(&ws, cx);
    assert_eq!(location(&ws, cx), b);
    assert_eq!(injected_cds(&panel, cx), 2);
    close_terminal(&panel, cx);
}

#[gpui_kit::test]
fn u10_follow_browser_quotes_odd_paths(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let odd = env.work().join("it's \"odd\" $HOME `x` !1\nline");
    std::fs::create_dir(&odd).unwrap();
    let (ws, panel, cx) = open_terminal(cx, &env, "Follow browser");

    ws.update(cx, |w, cx| w.navigate(odd.clone(), cx));
    wait_for_shell_cwd(&ws, &panel, cx, &odd);
    assert_eq!(injected_cds(&panel, cx), 1);
    // The cd ran as one literal path and the shell is idle.
    settle_until(&ws, cx, "idle prompt after the cd", |cx| {
        panel.read_with(cx, |p, _| p.active_follow().unwrap().prompt_state() == PromptState::Idle)
    });
    assert_nothing_injected(&panel, cx);
    close_terminal(&panel, cx);
}

#[gpui_kit::test]
fn u10_two_way_follows_both_directions_without_echo(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let a = env.work().join("a");
    std::fs::create_dir_all(a.join("b")).unwrap();
    let (ws, panel, cx) = open_terminal(cx, &env, "Two-way");
    assert_eq!(mode(&panel, cx), Some(FollowMode::TwoWay));

    // Browser -> shell: the injected cd's OSC 7 does not navigate the browser again.
    ws.update(cx, |w, cx| w.navigate(a.clone(), cx));
    wait_for_shell_cwd(&ws, &panel, cx, &a);
    settle(&ws, cx);
    assert_eq!(location(&ws, cx), a);
    assert_eq!(injected_cds(&panel, cx), 1);

    // Shell -> browser: the user's cd (signed OSC 7 from zsh) moves the bound pane.
    keys(&ws, cx, "c d space b enter");
    let b = a.join("b");
    settle_until(&ws, cx, "pane follows the shell", |cx| location(&ws, cx) == b);
    // ... and the pane's move is not typed back into the shell (no loop).
    settle_until(&ws, cx, "idle prompt in b", |cx| {
        panel.read_with(cx, |p, _| p.active_follow().unwrap().prompt_state() == PromptState::Idle)
    });
    assert_nothing_injected(&panel, cx);
    assert_eq!(shell_cwd(&panel, cx), Some(b.clone()));
    assert_eq!(injected_cds(&panel, cx), 1);
    assert_eq!(location(&ws, cx), b);
    close_terminal(&panel, cx);
}
