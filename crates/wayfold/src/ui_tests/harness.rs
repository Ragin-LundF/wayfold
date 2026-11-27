//! Shared helpers for headless UI tests: isolated env, the real Workspace in a test window, waits.

use crate::ui::workspace::Workspace;
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, WindowOptions};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// HOME and WAYFOLD_DATA_DIR are process-wide: every UI test holds this lock for its whole run.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Temp dir with `home/`, `data/` and `work/`; HOME and the data dir point into it.
pub struct Env {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Env {
    pub fn new(files: &[&str]) -> Self {
        // A test that panicked while holding the lock leaves it poisoned; the env is reset below anyway.
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "data", "work"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        for f in files {
            std::fs::write(root.join("work").join(f), b"x").unwrap();
        }
        // SAFETY: only UI tests change these variables, and they are serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("HOME", root.join("home"));
            std::env::set_var("WAYFOLD_DATA_DIR", root.join("data"));
        }
        Self { _tmp: tmp, root, _guard: guard }
    }

    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }
}

/// Opens the real Workspace (with keymap and Root, as in `main`) and shows `env.work()`.
pub fn open<'a>(cx: &'a mut TestAppContext, env: &Env) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::ui::theme::apply(true, cx);
        crate::app::actions::bind_keys(cx);
    });
    cx.disable_accessibility();
    // Listing runs on real worker threads, so the scheduler may park while waiting for them.
    cx.executor().allow_parking();
    let (handle, ws) = cx
        .update(|cx| gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| Workspace::new(window, cx))))
        .expect("open test window");
    let vcx = VisualTestContext::from_window(handle, cx).into_mut();
    let work = env.work();
    ws.update(vcx, |w, cx| w.navigate(work, cx));
    settle(&ws, vcx);
    (ws, vcx)
}

/// Runs foreground work until the active tab's listing has finished (max 5 s of real time).
pub fn settle(ws: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let location = ws.read_with(cx, |w, _| w.tab().location.clone());
    settle_until(ws, cx, &format!("listing of {}", location.display()), |_| true);
}

/// Settles until the listing has finished and `done` holds (max 5 s of real time); ops like
/// rename or trash finish on a worker thread after the listing.
pub fn settle_until(ws: &Entity<Workspace>, cx: &mut VisualTestContext, what: &str, done: impl Fn(&mut VisualTestContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        let listed = ws.read_with(cx, |w, _| w.cache.get(&w.tab().location).is_some_and(|l| !l.loading));
        if listed && done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} did not happen within 5 s");
        // Yield to the worker threads; the loop exits as soon as the condition holds.
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub fn dialog_open(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| window.has_active_dialog(cx))
}

/// Sends GPUI keystrokes (layout independent), then settles.
pub fn keys(ws: &Entity<Workspace>, cx: &mut VisualTestContext, keystrokes: &str) {
    cx.simulate_keystrokes(keystrokes);
    settle(ws, cx);
}

/// File names of the visible rows, in display order.
pub fn names(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    ws.read_with(cx, |w, _| w.rows().iter().map(|e| e.name.to_string_lossy().into_owned()).collect())
}
