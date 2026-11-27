//! U13: transfer queue state through the handlers the queue buttons call. Timing is avoided: a
//! name conflict holds the worker (WaitingConflict) until the test acts, so no copy needs slowing down.

use super::harness::{Env, dialog_open, keys, open, settle, settle_until};
use crate::ui::clipboard::{ClipMode, FileClipboard};
use crate::ui::tools_archive::{CreateArchiveView, JobView as _};
use crate::ui::workspace::Workspace;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AppContext as _, Entity, ParentElement as _, TestAppContext, VisualTestContext};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use wayfold_core::ops::{ConflictAction, ConflictResolution, TransferMode, TransferState};

fn states(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<TransferState> {
    ws.read_with(cx, |w, _| w.transfers.iter().map(|t| t.state).collect())
}

/// Settles until transfer `ix` of the queue is in `state`.
pub(super) fn wait_state(ws: &Entity<Workspace>, cx: &mut VisualTestContext, ix: usize, state: TransferState) {
    settle_until(ws, cx, &format!("transfer {ix} {state:?}"), |cx| states(ws, cx).get(ix) == Some(&state));
}

fn start(ws: &Entity<Workspace>, cx: &mut VisualTestContext, mode: TransferMode, sources: Vec<PathBuf>, dest: &Path) {
    let dest = dest.to_path_buf();
    ws.update_in(cx, |w, window, cx| w.start_transfer(mode, sources, dest, window, cx));
}

/// `out/` holds a different `a.txt`, so copying `work/a.txt` there waits for a conflict answer.
fn conflict_env(files: &[&str]) -> (Env, PathBuf) {
    let env = Env::new(files);
    let out = env.work().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("a.txt"), b"existing").unwrap();
    (env, out)
}

fn dir_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

pub(super) fn put_on_clipboard(ws: &Entity<Workspace>, cx: &mut VisualTestContext, mode: ClipMode, path: PathBuf) {
    // Set directly: Copy/Cut would also write the real system pasteboard.
    ws.update(cx, |w, _| w.clipboard = Some(FileClipboard { mode, sources: [path].into(), revision: 1, pasteboard_change: None }));
}

#[gpui_kit::test]
fn u13_paste_starts_a_copy_job_that_completes(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let out = env.work().join("out");
    std::fs::create_dir(&out).unwrap();
    let (ws, cx) = open(cx, &env);
    put_on_clipboard(&ws, cx, ClipMode::Copy, env.work().join("a.txt"));
    ws.update(cx, |w, cx| w.navigate(out.clone(), cx));

    keys(&ws, cx, "cmd-v");
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert_eq!(ws.read_with(cx, |w, _| w.transfers[0].mode), TransferMode::Copy);
    assert!(ws.read_with(cx, |w, _| w.queue_open), "the queue opens when a job starts");
    assert_eq!(dir_names(&out), ["a.txt"]);
    assert!(env.work().join("a.txt").exists(), "copy keeps the source");
}

#[gpui_kit::test]
fn u13_paste_after_cut_moves_and_clears_the_cut_mark(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let out = env.work().join("out");
    std::fs::create_dir(&out).unwrap();
    let (ws, cx) = open(cx, &env);
    put_on_clipboard(&ws, cx, ClipMode::Cut, env.work().join("a.txt"));
    ws.update(cx, |w, cx| w.navigate(out.clone(), cx));

    keys(&ws, cx, "cmd-v");
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert_eq!(ws.read_with(cx, |w, _| w.transfers[0].mode), TransferMode::Move);
    assert_eq!(dir_names(&out), ["a.txt"]);
    assert!(!env.work().join("a.txt").exists(), "move removes the source");
    assert!(ws.read_with(cx, |w, _| w.clipboard.is_none()), "moved items lose their cut mark");
}

#[gpui_kit::test]
fn u13_pause_holds_the_job_and_resume_completes_it(cx: &mut TestAppContext) {
    let (env, out) = conflict_env(&["a.txt", "b.txt"]);
    let (ws, cx) = open(cx, &env);
    start(&ws, cx, TransferMode::Copy, vec![env.work().join("a.txt"), env.work().join("b.txt")], &out);
    wait_state(&ws, cx, 0, TransferState::WaitingConflict);
    assert!(dialog_open(cx), "the conflict dialog opens");

    // What the Pause button and then the dialog's Skip button call.
    ws.read_with(cx, |w, _| {
        w.transfers[0].handle.pause();
        w.transfers[0].handle.resolve(ConflictResolution { action: ConflictAction::Skip, apply_to_remaining: false });
    });
    wait_state(&ws, cx, 0, TransferState::Paused);

    assert_eq!(dir_names(&out), ["a.txt"], "b.txt is not copied while paused");
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"existing");

    ws.read_with(cx, |w, _| w.transfers[0].handle.resume());
    wait_state(&ws, cx, 0, TransferState::Completed);
    assert_eq!(dir_names(&out), ["a.txt", "b.txt"]);
}

#[gpui_kit::test]
fn u13_cancel_leaves_source_and_destination_untouched(cx: &mut TestAppContext) {
    let (env, out) = conflict_env(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    start(&ws, cx, TransferMode::Move, vec![env.work().join("a.txt")], &out);
    wait_state(&ws, cx, 0, TransferState::WaitingConflict);
    assert!(dialog_open(cx), "the conflict dialog opens");

    ws.read_with(cx, |w, _| w.transfers[0].handle.cancel());
    wait_state(&ws, cx, 0, TransferState::Cancelled);

    assert_eq!(std::fs::read(env.work().join("a.txt")).unwrap(), b"x", "source intact");
    assert_eq!(dir_names(&out), ["a.txt"], "no partial or temp file in the destination");
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"existing");
    let status = ws.read_with(cx, |w, _| w.status.as_deref().unwrap_or_default().to_owned());
    assert_eq!(status, "a.txt cancelled; sources kept");
}

#[gpui_kit::test]
fn u13_retry_after_failure_into_read_only_folder(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let out = env.work().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(out.join("probe"), b"x").is_ok() {
        std::fs::remove_file(out.join("probe")).unwrap();
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: running as root");
        return;
    }
    let (ws, cx) = open(cx, &env);
    start(&ws, cx, TransferMode::Copy, vec![env.work().join("a.txt")], &out);
    // Make the folder writable again before asserting, so a failure here cannot leave it locked.
    settle_until(&ws, cx, "transfer 0 final", |cx| states(&ws, cx).first().is_some_and(|s| s.is_final()));
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(states(&ws, cx), [TransferState::Failed]);
    let (failed, id) = ws.read_with(cx, |w, _| (w.transfers[0].failed.clone(), w.transfers[0].id.clone()));
    assert_eq!(failed, [env.work().join("a.txt")]);
    assert!(dir_names(&out).is_empty(), "no partial file after the failure");

    ws.update_in(cx, |w, window, cx| w.retry_transfer(&id, window, cx));
    wait_state(&ws, cx, 1, TransferState::Completed);
    assert_eq!(dir_names(&out), ["a.txt"]);
    assert_eq!(ws.read_with(cx, |w, _| w.transfers[1].sources.clone()), [env.work().join("a.txt")]);
}

#[gpui_kit::test]
fn u13_conflict_dialog_closed_by_another_dialog_skips_the_item(cx: &mut TestAppContext) {
    let (env, out) = conflict_env(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    start(&ws, cx, TransferMode::Copy, vec![env.work().join("a.txt")], &out);
    wait_state(&ws, cx, 0, TransferState::WaitingConflict);
    assert!(dialog_open(cx), "the conflict dialog opens");

    // `close_dialog` closes the newest dialog without its `on_close`, as a dialog below would.
    cx.update(|window, cx| window.close_dialog(cx));
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert!(!dialog_open(cx));
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"existing", "skipped, not replaced");
    assert_eq!(dir_names(&out), ["a.txt"]);
}

#[gpui_kit::test]
fn u13_archive_dialog_finishing_below_a_conflict_dialog_leaves_it_open(cx: &mut TestAppContext) {
    let (env, out) = conflict_env(&["a.txt"]);
    let (ws, cx) = open(cx, &env);
    // Opened the way `tools::open_create_archive` does.
    let weak = ws.downgrade();
    let (sources, dest_dir) = (vec![env.work().join("a.txt")], env.work().to_path_buf());
    let view = cx.update(|window, cx| {
        let view = cx.new(|cx| CreateArchiveView::new(sources, dest_dir, weak, window, cx));
        let child = view.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.title("Create Archive").child(child.clone()));
        let own = window.focused(cx);
        view.update(cx, |v, _| v.own_dialog.handle = own);
        view
    });
    settle(&ws, cx);
    let in_archive_dialog = |cx: &mut VisualTestContext| {
        view.update_in(cx, |v, window, cx| v.own_dialog.handle.as_ref().is_some_and(|f| f.contains_focused(window, cx)))
    };
    assert!(in_archive_dialog(cx), "the archive dialog is open and focused");

    start(&ws, cx, TransferMode::Copy, vec![env.work().join("a.txt")], &out);
    wait_state(&ws, cx, 0, TransferState::WaitingConflict);
    settle(&ws, cx);
    assert!(!in_archive_dialog(cx), "the conflict dialog is on top");

    let archive = env.work().join("a.txt.zip");
    view.update_in(cx, |v, window, cx| v.finished(&Ok(archive), window, cx));
    // Checked before any settle: closing the conflict dialog would hand focus back at once.
    assert!(!in_archive_dialog(cx), "the conflict dialog still holds focus");
    settle(&ws, cx);
    assert_eq!(states(&ws, cx), [TransferState::WaitingConflict], "the conflict dialog is still waiting");
    assert!(dialog_open(cx));

    // Answer the conflict: once its dialog is gone, the finished archive dialog closes itself.
    ws.read_with(cx, |w, _| w.transfers[0].handle.resolve(ConflictResolution { action: ConflictAction::Skip, apply_to_remaining: false }));
    cx.update(|window, cx| window.close_dialog(cx));
    wait_state(&ws, cx, 0, TransferState::Completed);
    settle(&ws, cx);
    assert!(!dialog_open(cx), "the archive dialog closed once it was topmost again");
}
