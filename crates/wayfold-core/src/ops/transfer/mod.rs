//! Copy/move transfers (F03) on the worker pools, with events for the transfer drawer.

#[cfg(test)]
mod tests;
mod worker;

use super::{ItemResult, UndoAction};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMode {
    Copy,
    Move,
}

/// Captured at submission; later clipboard or navigation changes do not affect it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRequest {
    pub mode: TransferMode,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    Planning,
    Queued,
    Running,
    Paused,
    WaitingConflict,
    /// Every item finished or was skipped.
    Completed,
    /// At least one item failed (see the `Item` events).
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictAction {
    Skip,
    KeepBoth,
    /// The existing target goes to the trash, never deleted permanently.
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictResolution {
    pub action: ConflictAction,
    pub apply_to_remaining: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferEvent {
    State(TransferState),
    /// Planning finished; before this the drawer shows indeterminate progress.
    Totals {
        bytes: u64,
        items: u64,
    },
    /// At most ~10 per second; bytes count inside files.
    Progress {
        bytes_done: u64,
        items_done: u64,
        current: PathBuf,
    },
    /// The worker waits for `TransferHandle::resolve`.
    Conflict {
        source: PathBuf,
        existing: PathBuf,
    },
    /// Outcome of a top-level source, or a nested entry that failed or was skipped.
    Item(ItemResult),
    /// Moves only, sent before the final state: sources fully moved (clear their cut marks).
    Moved(Vec<PathBuf>),
    /// Same-volume moves that can be undone, sent before the final state.
    Undo(UndoAction),
}

pub struct TransferHandle {
    pub id: String,
    pub events: async_channel::Receiver<TransferEvent>,
    ctl: Arc<Control>,
}

impl TransferHandle {
    pub fn pause(&self) {
        self.ctl.set(|c| c.paused = true);
    }
    pub fn resume(&self) {
        self.ctl.set(|c| c.paused = false);
    }
    pub fn cancel(&self) {
        self.ctl.set(|c| c.cancelled = true);
    }
    /// Answer the pending `Conflict` event.
    pub fn resolve(&self, resolution: ConflictResolution) {
        self.ctl.set(|c| c.answer = Some(resolution));
    }
}

/// Starts a transfer on the network pool if any path is on a network mount, else the local pool.
pub fn start_transfer(req: TransferRequest) -> TransferHandle {
    start_with(req, Tuning::default())
}

/// Internal knobs; tests use them to slow copies down and inject failures.
#[derive(Clone, Debug)]
pub(crate) struct Tuning {
    pub chunk: usize,
    pub chunk_delay: Duration,
    /// Simulated write failure once this many bytes of a file were written.
    pub fail_write_after: Option<u64>,
    /// Simulated failure of the final rename into place.
    pub fail_publish: bool,
    /// Use the copy-then-delete path for moves even on one volume (simulates cross-volume).
    pub force_copy: bool,
    /// The next rename into place fails with `EXDEV` (a rename across a firmlink or mount).
    pub exdev_once: std::cell::Cell<bool>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            chunk: 1 << 20,
            chunk_delay: Duration::ZERO,
            fail_write_after: None,
            fail_publish: false,
            force_copy: false,
            exdev_once: Default::default(),
        }
    }
}

pub(crate) fn start_with(req: TransferRequest, tuning: Tuning) -> TransferHandle {
    let id = super::next_id();
    let (tx, events) = async_channel::unbounded();
    let ctl = Arc::new(Control::default());
    let network = std::iter::once(&req.dest_dir).chain(&req.sources).any(|p| crate::fs::is_network_path(p));
    let pool = if network { crate::pool::remote() } else { crate::pool::local() };
    let _ = tx.send_blocking(TransferEvent::State(TransferState::Queued));
    let worker = worker::Worker::new(id.clone(), req, tx, ctl.clone(), tuning);
    pool.execute(move || worker.run());
    TransferHandle { id, events, ctl }
}

#[derive(Default)]
struct Ctl {
    paused: bool,
    cancelled: bool,
    answer: Option<ConflictResolution>,
}

#[derive(Default)]
struct Control {
    state: Mutex<Ctl>,
    cv: Condvar,
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, Ctl> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set(&self, f: impl FnOnce(&mut Ctl)) {
        f(&mut self.lock());
        self.cv.notify_all();
    }
}
