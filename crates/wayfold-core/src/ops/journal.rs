//! Crash-safe operation journal: append-only JSONL in `data_dir()/journal.jsonl`.
//! An intent is fsynced before its mutation starts; a done record follows when it finished
//! (or was rolled back). `lock_and_recover()` at startup cleans up only what an interrupted
//! operation provably owns and never replays a deletion.

use super::{exists, next_id, rename_excl};
use crate::storage::{data_dir, path_bytes, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Name prefix of transfer temp copies; recovery deletes only paths with this prefix.
pub(crate) const TEMP_PREFIX: &str = ".wayfold-tmp-";
/// Name prefix of rename hops; these hold user data and are renamed back, never deleted.
pub(crate) const HOP_PREFIX: &str = ".wayfold-rn-";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JournalOp {
    Copy,
    Move,
    Rename,
    CreateFolder,
    CreateFile,
    Symlink,
    Trash,
    Restore,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Record {
    Intent {
        id: String,
        op: JournalOp,
        #[serde(with = "path_bytes")]
        from: PathBuf,
        #[serde(with = "path_bytes")]
        to: PathBuf,
        /// Path the operation owns while it runs (temp copy or rename hop); empty when none.
        #[serde(with = "path_bytes", default, skip_serializing_if = "is_empty")]
        temp: PathBuf,
    },
    Done {
        id: String,
    },
}

fn is_empty(p: &Path) -> bool {
    p.as_os_str().is_empty()
}

/// What `recover()` did. `unresolved` lists interrupted operations it left alone.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub removed_temp: Vec<PathBuf>,
    /// (hop, original): rename hops moved back to their original name.
    pub restored: Vec<(PathBuf, PathBuf)>,
    /// (op, from, to, owned temp or rename hop that still holds data; empty when none).
    pub unresolved: Vec<(JournalOp, PathBuf, PathBuf, PathBuf)>,
}

/// Held for the process lifetime: a shared `flock` on `journal.lock` that keeps later
/// instances from running recovery (and deleting this instance's live temps).
pub struct JournalLock {
    _file: fs::File,
}

static LOCK: Mutex<()> = Mutex::new(());

const JOURNAL_FILE: &str = "journal.jsonl";

fn journal_path() -> PathBuf {
    data_dir().join(JOURNAL_FILE)
}

/// Record an intent (fsynced) and return its id. Mutate only after this succeeds.
pub(crate) fn intent(op: JournalOp, from: &Path, to: &Path, temp: &Path) -> io::Result<String> {
    let id = next_id();
    let rec = Record::Intent { id: id.clone(), op, from: from.into(), to: to.into(), temp: temp.into() };
    append(&journal_path(), &rec, true)?;
    Ok(id)
}

/// Mark an intent finished. A lost done record only makes recovery report it; never unsafe.
pub(crate) fn done(id: &str) {
    if let Err(e) = append(&journal_path(), &Record::Done { id: id.to_owned() }, false) {
        tracing::warn!("journal done record failed: {e}");
    }
}

/// Run `f` between an intent and its done record.
pub(crate) fn journaled<T>(op: JournalOp, from: &Path, to: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let id = intent(op, from, to, Path::new(""))?;
    let r = f();
    done(&id);
    r
}

fn append(path: &Path, rec: &Record, sync: bool) -> io::Result<()> {
    let mut line = serde_json::to_vec(rec).map_err(io::Error::other)?;
    line.push(b'\n');
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&line)?;
    if sync { f.sync_data() } else { Ok(()) }
}

/// Startup: takes the journal lock and, only if no other instance holds it, runs recovery
/// (which also compacts the journal). Call once before any operation starts and keep the
/// returned lock alive. `None` (lock file unusable) means recovery was skipped.
pub fn lock_and_recover() -> Option<JournalLock> {
    match lock_and_recover_in(&data_dir()) {
        Ok((lock, Some(report))) => {
            tracing::info!("journal recovery: {report:?}");
            Some(lock)
        }
        Ok((lock, None)) => {
            tracing::info!("another Wayfold instance is running; journal recovery skipped");
            Some(lock)
        }
        Err(e) => {
            tracing::warn!("journal lock unavailable, recovery skipped: {e}");
            None
        }
    }
}

fn lock_and_recover_in(dir: &Path) -> io::Result<(JournalLock, Option<RecoveryReport>)> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("journal.lock"))?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid open descriptor owned by `file`.
    let alone = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    let report = alone.then(|| recover_at(&dir.join(JOURNAL_FILE)));
    // Downgrade (or wait out another instance's recovery); held until the process exits.
    // SAFETY: as above.
    if unsafe { libc::flock(fd, libc::LOCK_SH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((JournalLock { _file: file }, report))
}

pub(crate) fn recover_at(path: &Path) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(text) = fs::read_to_string(path) else { return report };
    // A torn last line from a crash fails to parse and is skipped.
    let records: Vec<Record> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let finished: HashSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            Record::Done { id } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let mut keep = Vec::new();
    for rec in &records {
        let Record::Intent { id, op, from, to, temp } = rec else { continue };
        if finished.contains(id.as_str()) {
            continue;
        }
        if !recover_one(*op, from, to, temp, &mut report) {
            report.unresolved.push((*op, from.clone(), to.clone(), temp.clone()));
            keep.push(rec);
        }
    }
    let mut out = Vec::new();
    for rec in keep {
        out.extend(serde_json::to_vec(rec).unwrap_or_default());
        out.push(b'\n');
    }
    if let Err(e) = write_atomic(path, &out) {
        tracing::warn!("journal compaction failed: {e}");
    }
    report
}

/// Returns true when nothing is left to do for this intent.
fn recover_one(op: JournalOp, from: &Path, to: &Path, temp: &Path, report: &mut RecoveryReport) -> bool {
    let name = temp.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if is_empty(temp) || !exists(temp) {
        // Nothing owned is left on disk; single-syscall ops are complete or never happened.
        // A move is settled once its data is at the source or the destination.
        return !matches!(op, JournalOp::Move) || exists(from) || exists(to);
    }
    match op {
        // A move whose source is gone may have published this data without the rename reaching
        // the disk (e.g. the volume was unplugged): the temp is then the only copy, so keep it.
        JournalOp::Move if name.starts_with(TEMP_PREFIX) && !exists(from) => false,
        JournalOp::Copy | JournalOp::Move if name.starts_with(TEMP_PREFIX) => {
            let md = fs::symlink_metadata(temp);
            let r = if md.is_ok_and(|m| m.is_dir()) { fs::remove_dir_all(temp) } else { fs::remove_file(temp) };
            match r {
                Ok(()) => {
                    report.removed_temp.push(temp.to_path_buf());
                    // A move whose temp copy was still unpublished never touched its source.
                    true
                }
                Err(e) => {
                    tracing::warn!("cannot remove {}: {e}", temp.display());
                    false
                }
            }
        }
        // Back to the original name, or on to the target when a swap partner took the original.
        JournalOp::Rename if name.starts_with(HOP_PREFIX) => {
            let Some(dest) = [from, to].into_iter().find(|d| rename_excl(temp, d).is_ok()) else { return false };
            report.restored.push((temp.to_path_buf(), dest.to_path_buf()));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_records(path: &Path, recs: &[Record]) {
        for r in recs {
            append(path, r, false).unwrap();
        }
    }

    fn intent_rec(id: &str, op: JournalOp, from: &Path, temp: &Path) -> Record {
        Record::Intent { id: id.into(), op, from: from.into(), to: from.with_extension("to"), temp: temp.into() }
    }

    #[test]
    fn f03_journal_recovery_removes_only_orphan_temp_files() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        let orphan = t.path().join(format!("{TEMP_PREFIX}a-0"));
        let finished = t.path().join(format!("{TEMP_PREFIX}b-0"));
        let not_ours = t.path().join("user-file");
        let orphan_dir = t.path().join(format!("{TEMP_PREFIX}c-0"));
        fs::write(&orphan, b"partial").unwrap();
        fs::write(&finished, b"x").unwrap();
        fs::write(&not_ours, b"keep").unwrap();
        fs::create_dir_all(orphan_dir.join("sub")).unwrap();
        let src = t.path().join("src");
        fs::write(&src, b"source").unwrap();
        write_records(
            &j,
            &[
                intent_rec("1", JournalOp::Copy, &src, &orphan),
                intent_rec("2", JournalOp::Copy, &src, &finished),
                Record::Done { id: "2".into() },
                intent_rec("3", JournalOp::Move, &src, &not_ours),
                intent_rec("4", JournalOp::Move, &src, &orphan_dir),
            ],
        );
        fs::OpenOptions::new().append(true).open(&j).unwrap().write_all(b"{\"t\":\"intent\",\"id").unwrap();

        let r = recover_at(&j);
        assert_eq!(r.removed_temp, vec![orphan.clone(), orphan_dir.clone()]);
        assert!(!orphan.exists() && !orphan_dir.exists());
        assert!(finished.exists(), "completed operations are not touched");
        assert_eq!(fs::read(&not_ours).unwrap(), b"keep", "only prefixed temp paths are deleted");
        assert_eq!(r.unresolved.len(), 1);
        // The unresolved intent stays in the compacted journal; a second run reports it again.
        assert_eq!(recover_at(&j).unresolved.len(), 1);
    }

    #[test]
    fn f04_recovery_moves_rename_hop_back_instead_of_deleting() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        let original = t.path().join("photo.jpg");
        let hop = t.path().join(format!("{HOP_PREFIX}x"));
        fs::write(&hop, b"data").unwrap();
        write_records(&j, &[intent_rec("1", JournalOp::Rename, &original, &hop)]);
        let r = recover_at(&j);
        assert_eq!(r.restored, vec![(hop.clone(), original.clone())]);
        assert_eq!(fs::read(&original).unwrap(), b"data");
        assert!(r.unresolved.is_empty());
    }

    #[test]
    fn f03_recovery_keeps_move_temp_when_source_is_gone() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        let temp = t.path().join(format!("{TEMP_PREFIX}m-0"));
        fs::write(&temp, b"only copy").unwrap();
        let gone = t.path().join("moved-source");
        write_records(&j, &[intent_rec("1", JournalOp::Move, &gone, &temp)]);
        let r = recover_at(&j);
        assert_eq!(fs::read(&temp).unwrap(), b"only copy", "an unpublished move whose source is gone is not deleted");
        assert!(r.removed_temp.is_empty());
        assert_eq!(r.unresolved, vec![(JournalOp::Move, gone.clone(), gone.with_extension("to"), temp.clone())]);
    }

    #[test]
    fn f03_settled_move_without_temp_is_dropped_from_the_journal() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        let src = t.path().join("moved");
        fs::write(src.with_extension("to"), b"published").unwrap();
        let temp = t.path().join(format!("{TEMP_PREFIX}m-1"));
        write_records(&j, &[intent_rec("1", JournalOp::Move, &src, &temp)]);
        assert!(recover_at(&j).unresolved.is_empty(), "published move: data is at the destination");
        assert!(recover_at(&j).unresolved.is_empty(), "and the journal was compacted");
    }

    #[test]
    fn f04_recovery_finishes_swap_when_original_name_is_taken() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        // Swap a<->b interrupted after a's hop reached "b": b's hop cannot go back to "b".
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        let hop = t.path().join(format!("{HOP_PREFIX}b"));
        fs::write(&b, b"was a").unwrap();
        fs::write(&hop, b"was b").unwrap();
        let rec = Record::Intent { id: "1".into(), op: JournalOp::Rename, from: b.clone(), to: a.clone(), temp: hop.clone() };
        write_records(&j, &[rec]);
        let r = recover_at(&j);
        assert_eq!(r.restored, vec![(hop.clone(), a.clone())]);
        assert_eq!(fs::read(&a).unwrap(), b"was b");
        assert_eq!(fs::read(&b).unwrap(), b"was a");
        assert!(r.unresolved.is_empty());
    }

    #[test]
    fn f04_unrestorable_hop_is_reported_with_its_path() {
        let t = tempfile::tempdir().unwrap();
        let j = t.path().join("journal.jsonl");
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        let hop = t.path().join(format!("{HOP_PREFIX}x"));
        for p in [&a, &b, &hop] {
            fs::write(p, b"x").unwrap();
        }
        let rec = Record::Intent { id: "1".into(), op: JournalOp::Rename, from: a.clone(), to: b.clone(), temp: hop.clone() };
        write_records(&j, &[rec]);
        assert_eq!(recover_at(&j).unresolved, vec![(JournalOp::Rename, a, b, hop.clone())]);
        assert!(hop.exists());
    }

    #[test]
    fn second_instance_skips_recovery_while_first_holds_the_lock() {
        let t = tempfile::tempdir().unwrap();
        let live_temp = t.path().join(format!("{TEMP_PREFIX}live-0"));
        let src = t.path().join("src");
        fs::write(&src, b"s").unwrap();

        let (first, report) = lock_and_recover_in(t.path()).unwrap();
        assert!(report.is_some(), "the only instance recovers");
        // The first instance starts a copy after its recovery.
        fs::write(&live_temp, b"in progress").unwrap();
        write_records(&t.path().join(JOURNAL_FILE), &[intent_rec("1", JournalOp::Copy, &src, &live_temp)]);

        let (second, report) = lock_and_recover_in(t.path()).unwrap();
        assert!(report.is_none(), "a second instance must not recover");
        assert!(live_temp.exists(), "the first instance's live temp survives");
        drop(first);
        assert!(lock_and_recover_in(t.path()).unwrap().1.is_none(), "any running instance blocks recovery");
        drop(second);
        let (_third, report) = lock_and_recover_in(t.path()).unwrap();
        assert_eq!(report.unwrap().removed_temp, vec![live_temp]);
    }
}
