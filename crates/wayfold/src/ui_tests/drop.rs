//! U16: drop handlers. A real OS drag is not reachable headless, so the tests call
//! `Workspace::drop_paths`, the single function every `on_drop` listener (folder row, icon tile,
//! pane) forwards to. Temp dirs share one volume, so drops are moves.

use super::harness::{Env, keys, names, open, settle_until};
use super::transfers::{put_on_clipboard, wait_state};
use crate::ui::clipboard::ClipMode;
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use std::path::PathBuf;
use wayfold_core::ops::{TransferMode, TransferState};

fn drop_on(ws: &Entity<Workspace>, cx: &mut VisualTestContext, paths: Vec<PathBuf>, dest: PathBuf) {
    ws.update_in(cx, |w, window, cx| w.drop_paths(paths, dest, window, cx));
}

fn job_count(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |w, _| w.transfers.len())
}

fn assert_refused(ws: &Entity<Workspace>, cx: &mut VisualTestContext) {
    assert_eq!(job_count(ws, cx), 0);
    assert_eq!(ws.read_with(cx, |w, _| w.status.clone()).as_deref(), Some(INSIDE_ITSELF));
}

const INSIDE_ITSELF: &str = "A folder can't be put inside itself";

#[gpui_kit::test]
fn u16_drop_files_on_a_folder_row_moves_them_into_it(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt", "b.txt"]);
    let sub = env.work().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![env.work().join("a.txt"), env.work().join("b.txt")], sub.clone());
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert_eq!(ws.read_with(cx, |w, _| w.transfers[0].mode), TransferMode::Move);
    assert!(sub.join("a.txt").exists() && sub.join("b.txt").exists());
    settle_until(&ws, cx, "listing without moved files", |cx| names(&ws, cx) == ["sub"]);
}

#[gpui_kit::test]
fn u16_drop_on_the_pane_moves_into_its_folder(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let outside = env.root.join("home/c.txt");
    std::fs::write(&outside, b"x").unwrap();
    let (ws, cx) = open(cx, &env);

    // The pane's listener drops into the active tab's location.
    let dest = ws.read_with(cx, |w, _| w.tab().location.clone());
    drop_on(&ws, cx, vec![outside.clone()], dest);
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert!(!outside.exists());
    settle_until(&ws, cx, "dropped file listed", |cx| names(&ws, cx) == ["c.txt"]);
}

#[gpui_kit::test]
fn u16_drop_on_itself_or_its_own_folder_starts_no_job(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let sub = env.work().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![sub.clone()], sub.clone());
    drop_on(&ws, cx, vec![env.work().join("a.txt")], env.work());

    assert_eq!(job_count(&ws, cx), 0);
    assert!(sub.is_dir() && env.work().join("a.txt").exists());
}

#[gpui_kit::test]
fn u16_drop_folder_on_its_own_child_is_refused_without_a_job(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let sub = env.work().join("sub");
    std::fs::create_dir_all(sub.join("inner")).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![sub.clone()], sub.join("inner"));

    assert_refused(&ws, cx);
    assert!(sub.join("inner").is_dir() && !sub.join("inner/sub").exists());
}

#[gpui_kit::test]
fn u16_drop_folder_on_a_sibling_with_the_same_name_prefix_moves_it(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let (foo, foobar) = (env.work().join("foo"), env.work().join("foobar"));
    std::fs::create_dir(&foo).unwrap();
    std::fs::create_dir(&foobar).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![foo.clone()], foobar.clone());
    wait_state(&ws, cx, 0, TransferState::Completed);

    assert!(foobar.join("foo").is_dir() && !foo.exists());
}

#[gpui_kit::test]
fn u16_paste_after_cut_inside_the_cut_folder_is_refused(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let sub = env.work().join("sub");
    std::fs::create_dir_all(sub.join("inner")).unwrap();
    let (ws, cx) = open(cx, &env);
    put_on_clipboard(&ws, cx, ClipMode::Cut, sub.clone());
    ws.update(cx, |w, cx| w.navigate(sub.join("inner"), cx));

    keys(&ws, cx, "cmd-v");

    assert_refused(&ws, cx);
    assert!(!sub.join("inner/sub").exists());
}

#[gpui_kit::test]
fn u16_drop_refuses_the_whole_drop_when_one_folder_would_go_inside_itself(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt"]);
    let sub = env.work().join("sub");
    std::fs::create_dir_all(sub.join("inner")).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![env.work().join("a.txt"), sub.clone()], sub.join("inner"));

    assert_refused(&ws, cx);
    assert!(env.work().join("a.txt").exists() && !sub.join("inner/a.txt").exists());
}

#[gpui_kit::test]
fn u16_drop_folder_on_its_child_reached_through_a_symlink_is_refused(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let sub = env.work().join("sub");
    std::fs::create_dir_all(sub.join("inner")).unwrap();
    std::os::unix::fs::symlink(&sub, env.work().join("link")).unwrap();
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![sub.clone()], env.work().join("link/inner"));

    assert_refused(&ws, cx);
}

#[gpui_kit::test]
fn u16_drop_folder_given_with_the_wrong_case_on_its_child_is_refused(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let foo = env.work().join("Foo");
    std::fs::create_dir_all(foo.join("inner")).unwrap();
    let wrong_case = env.work().join("FOO");
    if !wrong_case.exists() {
        println!("skipped: the temp filesystem is case-sensitive");
        return;
    }
    let (ws, cx) = open(cx, &env);

    drop_on(&ws, cx, vec![wrong_case], foo.join("inner"));

    assert_refused(&ws, cx);
    assert!(foo.join("inner").is_dir() && !foo.join("inner/FOO").exists());
}
