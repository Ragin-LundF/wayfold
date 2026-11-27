//! The transfer engine. One worker per transfer, running on a pool thread.
//!
//! Per top-level source: validate → resolve conflict → either `rename` (same-volume move) or
//! chunked copy into an owned temp sibling, publish with `RENAME_EXCL`, and for moves fsync
//! the destination folder, then delete each source entry only if it is unchanged. On error or cancel the source stays and only
//! the temp path this task created is removed.

use super::{ConflictAction, Control, TransferEvent, TransferMode, TransferRequest, TransferState, Tuning};
use crate::ops::journal::{self, JournalOp, TEMP_PREFIX, journaled};
use crate::ops::trash::move_to_trash;
use crate::ops::{
    FileIdentity, ItemOutcome, ItemResult, RenamedItem, UndoAction, copy_name, cstr, exists, identity, rename_excl, unique_name,
};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_SLICE: Duration = Duration::from_millis(500);

enum Stop {
    Cancelled,
    /// The item is finished and its outcome was already reported (e.g. skipped by `decide`).
    Reported,
    Io(io::Error),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Io(e)
    }
}

/// A source entry copied during a move, with the state it had; deleted only if unchanged.
struct Copied {
    path: PathBuf,
    id: FileIdentity,
    /// `Some((len, mtime))` for files; folders and links are checked by identity only
    /// (a folder's mtime changes when its children are removed).
    file: Option<(u64, Option<SystemTime>)>,
    is_dir: bool,
}

/// Where to write after conflict resolution: the target and, for Replace, the identity of
/// the existing item that may go to the trash.
type WritePlan = (PathBuf, Option<FileIdentity>);

pub(super) struct Worker {
    id: String,
    req: TransferRequest,
    tx: async_channel::Sender<TransferEvent>,
    ctl: Arc<Control>,
    tuning: Tuning,
    buf: Vec<u8>,
    dest_dev: u64,
    bytes_done: u64,
    items_done: u64,
    current: PathBuf,
    last_emit: Instant,
    sticky: Option<ConflictAction>,
    // shortcut: one PathBuf per entry of the top-level item being moved (memory grows with tree size),
    // upgrade to re-walking the source after publish if moves of multi-million-entry trees matter.
    copied: Vec<Copied>,
    moved: Vec<PathBuf>,
    renamed: Vec<RenamedItem>,
    failed: bool,
}

impl Worker {
    pub(super) fn new(
        id: String,
        req: TransferRequest,
        tx: async_channel::Sender<TransferEvent>,
        ctl: Arc<Control>,
        tuning: Tuning,
    ) -> Self {
        Self {
            id,
            req,
            tx,
            ctl,
            buf: vec![0; tuning.chunk.max(4096)],
            tuning,
            dest_dev: 0,
            bytes_done: 0,
            items_done: 0,
            current: PathBuf::new(),
            last_emit: Instant::now(),
            sticky: None,
            copied: Vec::new(),
            moved: Vec::new(),
            renamed: Vec::new(),
            failed: false,
        }
    }

    pub(super) fn run(mut self) {
        self.send(TransferEvent::State(TransferState::Planning));
        let outcome = self.run_items();
        if self.req.mode == TransferMode::Move {
            let moved = std::mem::take(&mut self.moved);
            self.send(TransferEvent::Moved(moved));
        }
        if !self.renamed.is_empty() {
            let renamed = std::mem::take(&mut self.renamed);
            self.send(TransferEvent::Undo(UndoAction::Renamed(renamed)));
        }
        self.emit_progress();
        let state = match outcome {
            Err(Stop::Cancelled) => TransferState::Cancelled,
            _ if self.failed => TransferState::Failed,
            _ => TransferState::Completed,
        };
        self.send(TransferEvent::State(state));
    }

    fn run_items(&mut self) -> Result<(), Stop> {
        let dest = self.req.dest_dir.clone();
        match fs::metadata(&dest) {
            Ok(md) if md.is_dir() => self.dest_dev = md.dev(),
            _ => {
                self.item(ItemResult::failed(&dest, "the destination is not a folder"));
                return Ok(());
            }
        }
        let sources = self.req.sources.clone();
        let mut sizes = Vec::with_capacity(sources.len());
        for s in &sources {
            sizes.push(self.measure(s)?);
        }
        let (bytes, items) = sizes.iter().fold((0, 0), |a, s| (a.0 + s.0, a.1 + s.1));
        self.send(TransferEvent::Totals { bytes, items });
        let to_copy: u64 = sources.iter().zip(&sizes).filter(|(s, _)| self.needs_copy(s)).map(|(_, z)| z.0).sum();
        if free_space(&dest).is_some_and(|free| to_copy > free) {
            let why = format!("not enough free space ({} needed)", crate::fs::format_size(to_copy));
            self.item(ItemResult::failed(&dest, why));
            return Ok(());
        }
        self.send(TransferEvent::State(TransferState::Running));
        for (i, (src, size)) in sources.iter().zip(sizes).enumerate() {
            self.checkpoint()?;
            let base = (self.bytes_done, self.items_done);
            self.transfer_one(i, src)?;
            // Skipped/failed/renamed items count as processed so the bar ends at 100%.
            self.bytes_done = self.bytes_done.max(base.0 + size.0);
            self.items_done = self.items_done.max(base.1 + size.1);
            self.progress();
        }
        Ok(())
    }

    /// (regular-file bytes, entries) below `p`, never following symlinks.
    fn measure(&mut self, p: &Path) -> Result<(u64, u64), Stop> {
        let Ok(md) = fs::symlink_metadata(p) else { return Ok((0, 0)) };
        if !md.is_dir() {
            return Ok((if md.is_file() { md.len() } else { 0 }, 1));
        }
        self.checkpoint()?;
        let mut total = (0, 1);
        for e in fs::read_dir(p).into_iter().flatten().flatten() {
            let s = self.measure(&e.path())?;
            total = (total.0 + s.0, total.1 + s.1);
        }
        Ok(total)
    }

    fn transfer_one(&mut self, index: usize, src: &Path) -> Result<(), Stop> {
        let Ok(md) = fs::symlink_metadata(src) else {
            self.item(ItemResult::failed(src, "the item no longer exists"));
            return Ok(());
        };
        let Some(name) = src.file_name() else {
            self.item(ItemResult::failed(src, "cannot transfer this item"));
            return Ok(());
        };
        let dest = self.req.dest_dir.clone();
        if md.is_dir() && is_inside(&dest, src) {
            self.item(ItemResult::failed(src, "a folder cannot be put inside itself"));
            return Ok(());
        }
        let src_id = FileIdentity { dev: md.dev(), ino: md.ino() };
        let mut target = dest.join(name);
        if identity(&target).ok() == Some(src_id) {
            match self.req.mode {
                TransferMode::Move => {
                    self.item(ItemResult::new(src, ItemOutcome::Skipped("already in this folder".into())));
                    return Ok(());
                }
                TransferMode::Copy => target = copy_name(&dest, name),
            }
        }
        if !self.needs_copy(src) { self.move_by_rename(index, src, src_id, target) } else { self.copy_and_publish(index, src, target) }
    }

    /// Copies and moves to another volume need data copied; same-volume moves are renames.
    fn needs_copy(&self, src: &Path) -> bool {
        self.req.mode == TransferMode::Copy || self.tuning.force_copy || fs::symlink_metadata(src).is_ok_and(|m| m.dev() != self.dest_dev)
    }

    fn move_by_rename(&mut self, index: usize, src: &Path, src_id: FileIdentity, mut target: PathBuf) -> Result<(), Stop> {
        loop {
            let Some((to, replace)) = self.decide(src, target)? else { return Ok(()) };
            let r = journaled(JournalOp::Move, src, &to, || self.publish(src, &to, replace));
            match r {
                Ok(()) => {
                    self.moved.push(src.to_path_buf());
                    self.renamed.push(RenamedItem { from: src.to_path_buf(), to: to.clone(), identity: src_id });
                    self.item(ItemResult::new(src, ItemOutcome::Done(to)));
                    return Ok(());
                }
                // Created after the check: ask again, never overwrite.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => target = to,
                // Already decided (and a replaced item put back): copy with the same plan, don't ask again.
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => return self.copy_planned(index, src, (to, replace)),
                Err(e) => {
                    self.item(ItemResult::failed(src, e));
                    return Ok(());
                }
            }
        }
    }

    fn copy_and_publish(&mut self, index: usize, src: &Path, target: PathBuf) -> Result<(), Stop> {
        let Some(plan) = self.decide(src, target)? else { return Ok(()) };
        self.copy_planned(index, src, plan)
    }

    fn copy_planned(&mut self, index: usize, src: &Path, (mut to, mut replace): WritePlan) -> Result<(), Stop> {
        let temp = self.req.dest_dir.join(format!("{TEMP_PREFIX}{}-{index}", self.id));
        let op = if self.req.mode == TransferMode::Move { JournalOp::Move } else { JournalOp::Copy };
        let jid = match journal::intent(op, src, &to, &temp) {
            Ok(id) => id,
            Err(e) => {
                self.item(ItemResult::failed(src, format!("journal unavailable: {e}")));
                return Ok(());
            }
        };
        self.copied.clear();
        let mut result = self.copy_entry(src, &temp);
        // A locked (`uchg`/`uappnd`) item cannot be renamed: unlock the temp, lock it again once published.
        let locked = if result.is_ok() { unlock(&temp) } else { None };
        while result.is_ok() {
            match self.publish(&temp, &to, replace) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match self.decide(src, to.clone()) {
                    Ok(Some(plan)) => (to, replace) = plan,
                    Ok(None) => result = Err(Stop::Reported),
                    Err(stop) => result = Err(stop),
                },
                Err(e) => result = Err(e.into()),
            }
        }
        if result.is_ok()
            && let Some(flags) = locked
        {
            set_flags(&to, flags).unwrap_or_else(|e| tracing::warn!("cannot lock {} again: {e}", to.display()));
        }
        match result {
            Ok(()) => {
                if self.req.mode != TransferMode::Move {
                    self.item(ItemResult::new(src, ItemOutcome::Done(to)));
                } else if let Err(e) = sync_dir(to.parent().unwrap_or(&self.req.dest_dir)) {
                    // Deleting the source before the copy is durable could lose the only copy.
                    let why = format!("copied to {}, but it could not be saved safely; the original was kept: {e}", to.display());
                    self.item(ItemResult::failed(src, why));
                } else {
                    self.remove_sources(src, &to);
                }
                journal::done(&jid);
                Ok(())
            }
            Err(stop) => {
                remove_owned(&temp);
                journal::done(&jid);
                match stop {
                    Stop::Cancelled => Err(Stop::Cancelled),
                    Stop::Reported => Ok(()),
                    Stop::Io(e) => {
                        self.item(ItemResult::failed(src, e));
                        Ok(())
                    }
                }
            }
        }
    }

    /// Resolves a conflict at `target`; `None` means the item is finished (skipped or failed
    /// and already reported).
    fn decide(&mut self, src: &Path, target: PathBuf) -> Result<Option<WritePlan>, Stop> {
        let Ok(existing) = identity(&target) else { return Ok(Some((target, None))) };
        let action = match self.sticky {
            Some(a) => a,
            None => self.ask(src, &target)?,
        };
        Ok(match action {
            ConflictAction::Skip => {
                self.item(ItemResult::new(src, ItemOutcome::Skipped("an item with this name exists".into())));
                None
            }
            ConflictAction::KeepBoth => {
                let dir = target.parent().unwrap_or(Path::new("/"));
                Some((unique_name(dir, target.file_name().unwrap_or_default()), None))
            }
            ConflictAction::Replace if is_inside(src, &target) => {
                self.item(ItemResult::failed(src, "cannot replace a folder that contains the item"));
                None
            }
            ConflictAction::Replace => Some((target, Some(existing))),
        })
    }

    /// Renames `from` to the free name `to`. For Replace the existing item goes to the trash
    /// first; if the rename then fails it is put back, or the error says where it is.
    fn publish(&self, from: &Path, to: &Path, replace: Option<FileIdentity>) -> io::Result<()> {
        let trashed = trash_replaced(to, replace)?;
        let placed = if self.tuning.fail_publish {
            Err(io::Error::other("simulated publish failure"))
        } else if self.tuning.exdev_once.replace(false) {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        } else {
            rename_excl(from, to)
        };
        let (Err(e), Some(trashed)) = (&placed, trashed) else { return placed };
        if journaled(JournalOp::Restore, &trashed, to, || rename_excl(&trashed, to)).is_ok() {
            return placed;
        }
        // Not `AlreadyExists`: the caller must report this, not ask about the conflict again.
        Err(io::Error::other(format!("{e}; the existing item was moved to the Trash ({})", trashed.display())))
    }

    fn ask(&mut self, src: &Path, existing: &Path) -> Result<ConflictAction, Stop> {
        let ctl = self.ctl.clone();
        let mut c = ctl.lock();
        c.answer = None;
        self.send(TransferEvent::Conflict { source: src.to_path_buf(), existing: existing.to_path_buf() });
        self.send(TransferEvent::State(TransferState::WaitingConflict));
        loop {
            // Nobody left to answer (all receivers dropped): stop instead of blocking a worker forever.
            if c.cancelled || self.tx.is_closed() {
                return Err(Stop::Cancelled);
            }
            if let Some(a) = c.answer.take() {
                if a.apply_to_remaining {
                    self.sticky = Some(a.action);
                }
                self.send(TransferEvent::State(TransferState::Running));
                return Ok(a.action);
            }
            c = ctl.cv.wait_timeout(c, WAIT_SLICE).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Safe point: honours pause and cancel.
    fn checkpoint(&mut self) -> Result<(), Stop> {
        let ctl = self.ctl.clone();
        let mut c = ctl.lock();
        if c.paused && !c.cancelled {
            self.send(TransferEvent::State(TransferState::Paused));
            while c.paused && !c.cancelled && !self.tx.is_closed() {
                c = ctl.cv.wait_timeout(c, WAIT_SLICE).unwrap_or_else(|e| e.into_inner()).0;
            }
            if !c.cancelled {
                self.send(TransferEvent::State(TransferState::Running));
            }
        }
        if c.cancelled || (c.paused && self.tx.is_closed()) { Err(Stop::Cancelled) } else { Ok(()) }
    }

    /// Copies `src` to `dst` without following links. Errors below a folder are reported per
    /// item and do not stop the folder; errors on `src` itself are returned.
    fn copy_entry(&mut self, src: &Path, dst: &Path) -> Result<(), Stop> {
        let md = fs::symlink_metadata(src)?;
        let ft = md.file_type();
        self.current = src.to_path_buf();
        if ft.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            copy_metadata(src, dst, true);
        } else if ft.is_file() {
            self.copy_file(src, dst, md.len())?;
        } else if ft.is_dir() {
            let entries = fs::read_dir(src)?;
            fs::create_dir(dst)?;
            for entry in entries {
                self.checkpoint()?;
                let name = match entry {
                    Ok(e) => e.file_name(),
                    Err(e) => {
                        self.item(ItemResult::failed(src, e));
                        continue;
                    }
                };
                match self.copy_entry(&src.join(&name), &dst.join(&name)) {
                    Err(Stop::Io(e)) => self.item(ItemResult::failed(&src.join(&name), e)),
                    other => other?,
                }
            }
            // After the children, so the folder's own times survive.
            copy_metadata(src, dst, false);
        } else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "special files are not copied").into());
        }
        if self.req.mode == TransferMode::Move {
            let file = ft.is_file().then(|| (md.len(), md.modified().ok()));
            let id = FileIdentity { dev: md.dev(), ino: md.ino() };
            self.copied.push(Copied { path: src.to_path_buf(), id, file, is_dir: ft.is_dir() });
        }
        self.items_done += 1;
        self.progress();
        Ok(())
    }

    fn copy_file(&mut self, src: &Path, dst: &Path, expected_len: u64) -> Result<(), Stop> {
        let mut r = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(src)?;
        let mut w = OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst)?;
        let mut buf = std::mem::take(&mut self.buf);
        let res = self.copy_data(&mut r, &mut w, &mut buf, expected_len);
        self.buf = buf;
        drop(w);
        if res.is_err() {
            let _ = fs::remove_file(dst);
            return res;
        }
        copy_metadata(src, dst, false);
        if fs::symlink_metadata(dst)?.len() != expected_len {
            let _ = fs::remove_file(dst);
            return Err(io::Error::other("the copy has the wrong size").into());
        }
        Ok(())
    }

    fn copy_data(&mut self, r: &mut File, w: &mut File, buf: &mut [u8], expected_len: u64) -> Result<(), Stop> {
        let mut written = 0u64;
        loop {
            self.checkpoint()?;
            let n = r.read(buf)?;
            if n == 0 {
                break;
            }
            if self.tuning.fail_write_after.is_some_and(|limit| written + n as u64 > limit) {
                return Err(io::Error::other("simulated write failure").into());
            }
            w.write_all(&buf[..n])?;
            written += n as u64;
            self.bytes_done += n as u64;
            self.progress();
            if !self.tuning.chunk_delay.is_zero() {
                std::thread::sleep(self.tuning.chunk_delay);
            }
        }
        if written != expected_len {
            return Err(io::Error::other("the source changed while it was copied").into());
        }
        w.sync_all()?;
        Ok(())
    }

    /// Deletes copied source entries (recorded children first) that are unchanged since they were copied.
    fn remove_sources(&mut self, src: &Path, to: &Path) {
        for c in std::mem::take(&mut self.copied) {
            let unchanged = fs::symlink_metadata(&c.path).is_ok_and(|md| {
                FileIdentity { dev: md.dev(), ino: md.ino() } == c.id
                    && c.file.is_none_or(|(len, mtime)| md.len() == len && md.modified().ok() == mtime)
            });
            if !unchanged {
                self.item(ItemResult::failed(&c.path, "changed during the move; the original was kept"));
                continue;
            }
            let r = if c.is_dir { fs::remove_dir(&c.path) } else { fs::remove_file(&c.path) };
            match r {
                // A folder still holding items that were not moved stays; those items were reported.
                Err(e) if c.is_dir && e.raw_os_error() == Some(libc::ENOTEMPTY) => {}
                Err(e) => self.item(ItemResult::failed(&c.path, e)),
                Ok(()) => {}
            }
        }
        if exists(src) {
            let why = format!("partly moved to {}; the remaining items stayed in place", to.display());
            self.item(ItemResult::failed(src, why));
        } else {
            self.moved.push(src.to_path_buf());
            self.item(ItemResult::new(src, ItemOutcome::Done(to.to_path_buf())));
        }
    }

    fn item(&mut self, r: ItemResult) {
        if matches!(r.outcome, ItemOutcome::Failed(_)) {
            self.failed = true;
        }
        self.send(TransferEvent::Item(r));
    }

    fn progress(&mut self) {
        if self.last_emit.elapsed() >= PROGRESS_INTERVAL {
            self.emit_progress();
        }
    }

    fn emit_progress(&mut self) {
        self.last_emit = Instant::now();
        let current = self.current.clone();
        self.send(TransferEvent::Progress { bytes_done: self.bytes_done, items_done: self.items_done, current });
    }

    fn send(&self, e: TransferEvent) {
        let _ = self.tx.try_send(e);
    }
}

/// Moves the existing target to the trash for Replace, but only if it is still the item the
/// user saw in the conflict dialog. Returns where it landed.
fn trash_replaced(target: &Path, replace: Option<FileIdentity>) -> io::Result<Option<PathBuf>> {
    let Some(expected) = replace else { return Ok(None) };
    match identity(target) {
        Ok(id) if id == expected => journaled(JournalOp::Trash, target, target, || move_to_trash(target)).map(Some),
        Ok(_) => Err(io::Error::other("the existing item changed; it was not replaced")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Makes a folder's rename durable (macOS `fsync` does not flush the drive cache).
fn sync_dir(dir: &Path) -> io::Result<()> {
    let f = File::open(dir)?;
    // SAFETY: valid open descriptor owned by `f`.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
        return Ok(());
    }
    f.sync_all()
}

/// Clears `uchg`/`uappnd` on `path`; returns the original flags when it changed them.
fn unlock(path: &Path) -> Option<u32> {
    let flags = fs::symlink_metadata(path).ok()?.st_flags();
    let locked = libc::UF_IMMUTABLE | libc::UF_APPEND;
    if flags & locked == 0 {
        return None;
    }
    set_flags(path, flags & !locked).map(|()| flags).ok()
}

fn set_flags(path: &Path, flags: u32) -> io::Result<()> {
    let c = cstr(path)?;
    // SAFETY: valid NUL-terminated path; lchflags does not follow a final symlink.
    if unsafe { libc::lchflags(c.as_ptr(), flags) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Bytes available to this user on the volume of `dir`, when the kernel reports it.
fn free_space(dir: &Path) -> Option<u64> {
    let c = cstr(dir).ok()?;
    // SAFETY: statfs fills the zeroed struct for a valid NUL-terminated path.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail.saturating_mul(u64::from(s.f_bsize)))
}

/// `dest` is `src` or below it (resolving links in both).
fn is_inside(dest: &Path, src: &Path) -> bool {
    match (fs::canonicalize(dest), fs::canonicalize(src)) {
        (Ok(d), Ok(s)) => d.starts_with(s),
        _ => dest.starts_with(src),
    }
}

/// Removes a temp path this task created (file or folder tree), including copied locked or
/// read-only entries.
fn remove_owned(temp: &Path) {
    make_removable(temp);
    let r = match fs::symlink_metadata(temp) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(temp),
        Ok(_) => fs::remove_file(temp),
        Err(_) => Ok(()),
    };
    if let Err(e) = r {
        tracing::warn!("cannot remove temp {}: {e}", temp.display());
    }
}

/// Clears user flags and grants the owner write access to folders, never following links.
fn make_removable(path: &Path) {
    let Ok(md) = fs::symlink_metadata(path) else { return };
    if md.st_flags() & (libc::UF_IMMUTABLE | libc::UF_APPEND) != 0 {
        let _ = set_flags(path, 0);
    }
    if !md.is_dir() {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode((md.mode() & 0o7777) | 0o700));
    for e in fs::read_dir(path).into_iter().flatten().flatten() {
        make_removable(&e.path());
    }
}

/// Copies ACLs, mode, flags, times, xattrs and resource forks. Unsupported metadata on the
/// target volume (e.g. ACLs on FAT) is logged, not fatal.
fn copy_metadata(src: &Path, dst: &Path, nofollow: bool) {
    let (Ok(s), Ok(d)) = (cstr(src), cstr(dst)) else { return };
    let flags = libc::COPYFILE_METADATA | if nofollow { libc::COPYFILE_NOFOLLOW } else { 0 };
    // SAFETY: valid NUL-terminated paths; a null state is allowed.
    if unsafe { libc::copyfile(s.as_ptr(), d.as_ptr(), std::ptr::null_mut(), flags) } != 0 {
        tracing::warn!("metadata not fully copied to {}: {}", dst.display(), io::Error::last_os_error());
    }
}
