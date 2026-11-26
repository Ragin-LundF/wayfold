//! Internal file clipboard (F01). Copy/cut also writes file URLs to the system pasteboard;
//! when the pasteboard changes externally, the marks are invalidated.

use crate::ui::workspace::Workspace;
use gpui_kit::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipMode {
    Copy,
    Cut,
}

#[derive(Clone, Debug)]
pub struct FileClipboard {
    pub mode: ClipMode,
    pub sources: HashSet<PathBuf>,
    /// Bumped on every change, so stale paste plans can be detected.
    pub revision: u64,
    /// System pasteboard change count when we wrote it; differs → external replacement.
    pub pasteboard_change: Option<i64>,
}

impl Workspace {
    pub fn clip_mode_for(&self, path: &Path) -> Option<ClipMode> {
        self.clipboard.as_ref().filter(|c| c.sources.contains(path)).map(|c| c.mode)
    }

    pub fn set_clipboard(&mut self, mode: ClipMode, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.selected_entries().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            self.notify_status("Select items to copy or cut", cx);
            return;
        }
        let change = waypoint_core::platform::clipboard::write_file_urls(&paths);
        let revision = self.clipboard.as_ref().map_or(1, |c| c.revision + 1);
        let n = paths.len();
        self.clipboard = Some(FileClipboard { mode, sources: paths.into_iter().collect(), revision, pasteboard_change: change });
        let verb = if mode == ClipMode::Cut { "Cut" } else { "Copied" };
        self.notify_status(format!("{verb} {n} item(s)"), cx);
    }

    pub fn clear_clipboard(&mut self, cx: &mut Context<Self>) {
        self.clipboard = None;
        cx.notify();
    }

    /// Drop marks for sources that no longer exist, and all marks when another app replaced
    /// the system pasteboard (F01 reconcile).
    pub fn reconcile_clipboard(&mut self) {
        let Some(c) = &mut self.clipboard else { return };
        let now = waypoint_core::platform::clipboard::change_count();
        if c.pasteboard_change.is_some() && now != c.pasteboard_change {
            self.clipboard = None;
            return;
        }
        c.sources.retain(|p| p.symlink_metadata().is_ok());
        if c.sources.is_empty() {
            self.clipboard = None;
        }
    }

    /// Successful moves clear their cut marks; failed items stay marked.
    pub fn clear_moved(&mut self, moved: &[PathBuf]) {
        if let Some(c) = &mut self.clipboard {
            if c.mode == ClipMode::Cut {
                for p in moved {
                    c.sources.remove(p);
                }
                c.revision += 1;
                if c.sources.is_empty() {
                    self.clipboard = None;
                }
            }
        }
    }

    /// Sources to paste: our marks, or file URLs another app put on the pasteboard.
    pub fn paste_sources(&self) -> Option<(ClipMode, Vec<PathBuf>)> {
        if let Some(c) = &self.clipboard {
            let mut v: Vec<PathBuf> = c.sources.iter().cloned().collect();
            v.sort();
            return Some((c.mode, v));
        }
        let ext = waypoint_core::platform::clipboard::read_file_urls();
        (!ext.is_empty()).then_some((ClipMode::Copy, ext))
    }
}
