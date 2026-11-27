use super::*;
use crate::ops::{ItemOutcome, duplicate, test_env, undo};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

fn req(mode: TransferMode, sources: &[&Path], dest: &Path) -> TransferRequest {
    TransferRequest { mode, sources: sources.iter().map(|p| p.to_path_buf()).collect(), dest_dir: dest.to_path_buf() }
}

fn slow() -> Tuning {
    Tuning { chunk: 64 << 10, chunk_delay: Duration::from_millis(2), ..Tuning::default() }
}

/// Collects events until the final state, calling `on` for each one.
fn drain(h: &TransferHandle, mut on: impl FnMut(&TransferEvent, &TransferHandle)) -> Vec<TransferEvent> {
    let mut all = Vec::new();
    loop {
        let e = h.events.recv_blocking().expect("worker ended without a final state");
        on(&e, h);
        let last = matches!(e, TransferEvent::State(s) if s.is_final());
        all.push(e);
        if last {
            return all;
        }
    }
}

fn final_state(events: &[TransferEvent]) -> TransferState {
    match events.last() {
        Some(TransferEvent::State(s)) => *s,
        other => panic!("no final state: {other:?}"),
    }
}

fn items(events: &[TransferEvent]) -> Vec<&ItemResult> {
    events.iter().filter_map(|e| if let TransferEvent::Item(r) = e { Some(r) } else { None }).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

fn big_file(path: &Path, len: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    fs::write(path, &data).unwrap();
    data
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn f03_symlinks_copied_as_links_broken_links_and_cycles() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let dir = s.path().join("dir");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("file"), b"data").unwrap();
    symlink("file", dir.join("link")).unwrap();
    symlink("missing", dir.join("broken")).unwrap();
    symlink(".", dir.join("cycle")).unwrap();

    let ev = drain(&start_transfer(req(TransferMode::Copy, &[&dir], d.path())), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Completed, "{ev:?}");
    let out = d.path().join("dir");
    assert_eq!(fs::read(out.join("file")).unwrap(), b"data");
    assert_eq!(fs::read_link(out.join("link")).unwrap(), Path::new("file"));
    assert_eq!(fs::read_link(out.join("broken")).unwrap(), Path::new("missing"));
    assert_eq!(fs::read_link(out.join("cycle")).unwrap(), Path::new("."));
    assert!(ev.contains(&TransferEvent::Totals { bytes: 4, items: 5 }));
    assert_eq!(names(d.path()), vec!["dir"], "no temp left");
}

#[test]
fn f03_same_volume_move_is_a_rename_and_undoable() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = s.path().join("a.txt");
    fs::write(&a, b"a").unwrap();
    let ev = drain(&start_transfer(req(TransferMode::Move, &[&a], d.path())), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Completed);
    assert!(ev.contains(&TransferEvent::Moved(vec![a.clone()])));
    let undo_action = ev.iter().find_map(|e| if let TransferEvent::Undo(u) = e { Some(u.clone()) } else { None }).unwrap();
    assert!(!a.exists());
    assert!(matches!(undo(&undo_action)[0].outcome, ItemOutcome::Done(_)));
    assert_eq!(fs::read(&a).unwrap(), b"a");
}

#[test]
fn f03_rejects_folder_into_itself_and_missing_source() {
    test_env();
    let s = tempfile::tempdir().unwrap();
    let dir = s.path().join("dir");
    fs::create_dir_all(dir.join("sub")).unwrap();
    let missing = s.path().join("gone");
    let ev = drain(&start_transfer(req(TransferMode::Move, &[&dir, &missing], &dir.join("sub"))), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Failed);
    assert!(items(&ev).iter().all(|r| matches!(r.outcome, ItemOutcome::Failed(_))));
    assert!(dir.join("sub").is_dir());

    let ev = drain(&start_transfer(req(TransferMode::Copy, &[&dir], &dir.join("sub"))), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Failed);
    assert!(matches!(&items(&ev)[0].outcome, ItemOutcome::Failed(why) if why.contains("inside itself")), "{ev:?}");
    assert!(!dir.join("sub/dir").exists());
}

#[test]
fn f03_conflicts_skip_keep_both_replace_apply_to_remaining() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let srcs: Vec<_> = ["a", "b", "c"].iter().map(|n| s.path().join(n)).collect();
    for p in &srcs {
        fs::write(p, b"new").unwrap();
        fs::write(d.path().join(p.file_name().unwrap()), b"old").unwrap();
    }
    let answers = [
        ConflictResolution { action: ConflictAction::Skip, apply_to_remaining: false },
        ConflictResolution { action: ConflictAction::KeepBoth, apply_to_remaining: false },
        ConflictResolution { action: ConflictAction::Replace, apply_to_remaining: false },
    ];
    let mut asked = 0;
    let refs: Vec<&Path> = srcs.iter().map(|p| p.as_path()).collect();
    let ev = drain(&start_transfer(req(TransferMode::Copy, &refs, d.path())), |e, h| {
        if matches!(e, TransferEvent::Conflict { .. }) {
            h.resolve(answers[asked]);
            asked += 1;
        }
    });
    assert_eq!(final_state(&ev), TransferState::Completed, "{ev:?}");
    assert_eq!(fs::read(d.path().join("a")).unwrap(), b"old", "skip");
    assert_eq!(fs::read(d.path().join("b")).unwrap(), b"old", "keep both keeps the existing");
    assert_eq!(fs::read(d.path().join("b 2")).unwrap(), b"new");
    assert_eq!(fs::read(d.path().join("c")).unwrap(), b"new", "replace");
    assert!(ev.contains(&TransferEvent::State(TransferState::WaitingConflict)));

    // Apply to remaining: one question for three conflicts; the replaced files went to the trash.
    let mut asked = 0;
    let ev = drain(&start_transfer(req(TransferMode::Copy, &refs, d.path())), |e, h| {
        if matches!(e, TransferEvent::Conflict { .. }) {
            asked += 1;
            h.resolve(ConflictResolution { action: ConflictAction::KeepBoth, apply_to_remaining: true });
        }
    });
    assert_eq!(final_state(&ev), TransferState::Completed);
    assert_eq!(asked, 1);
    assert!(d.path().join("c 2").exists());
}

#[test]
fn f03_existing_target_race_asks_again_and_never_overwrites() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("big");
    let data = big_file(&src, 2 << 20);
    let target = d.path().join("big");
    let mut created = false;
    let h = start_with(req(TransferMode::Copy, &[&src], d.path()), slow());
    let ev = drain(&h, |e, h| match e {
        TransferEvent::Progress { .. } if !created => {
            fs::write(&target, b"intruder").unwrap(); // appears after planning, during the copy
            created = true;
        }
        TransferEvent::Conflict { .. } => h.resolve(ConflictResolution { action: ConflictAction::Skip, apply_to_remaining: false }),
        _ => {}
    });
    assert!(created);
    assert!(ev.iter().any(|e| matches!(e, TransferEvent::Conflict { .. })), "{ev:?}");
    assert_eq!(fs::read(&target).unwrap(), b"intruder");
    assert_eq!(fs::read(&src).unwrap(), data);
    assert_eq!(names(d.path()), vec!["big"], "temp copy removed");
}

#[test]
fn f03_cancel_mid_file_keeps_source_and_removes_temp() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("big");
    let data = big_file(&src, 4 << 20);
    let h = start_with(req(TransferMode::Move, &[&src], d.path()), Tuning { force_copy: true, ..slow() });
    let ev = drain(&h, |e, h| {
        if let TransferEvent::Progress { bytes_done, .. } = e
            && *bytes_done > 0
        {
            h.cancel();
        }
    });
    assert_eq!(final_state(&ev), TransferState::Cancelled);
    assert_eq!(fs::read(&src).unwrap(), data);
    assert!(names(d.path()).is_empty(), "{:?}", names(d.path()));
    assert!(ev.contains(&TransferEvent::Moved(vec![])), "cut mark stays");
}

#[test]
fn f03_cross_volume_move_failure_preserves_source() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let dir = s.path().join("dir");
    fs::create_dir(&dir).unwrap();
    big_file(&dir.join("f1"), 300_000);
    let file = s.path().join("file");
    big_file(&file, 300_000);
    let tuning = Tuning { force_copy: true, fail_write_after: Some(100_000), ..Tuning::default() };
    let ev = drain(&start_with(req(TransferMode::Move, &[&dir, &file], d.path()), tuning), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Failed);
    assert_eq!(fs::read(dir.join("f1")).unwrap().len(), 300_000);
    assert_eq!(fs::read(&file).unwrap().len(), 300_000);
    // Per-item outcome: the folder is published with what succeeded (nothing), never a partial file.
    assert_eq!(names(d.path()), vec!["dir"], "no temp and no target for the failed file");
    assert!(names(&d.path().join("dir")).is_empty());
    let failed: Vec<_> = items(&ev).iter().filter(|r| matches!(r.outcome, ItemOutcome::Failed(_))).map(|r| r.path.clone()).collect();
    assert_eq!(failed, vec![dir.join("f1"), dir.clone(), file.clone()]);
    assert!(ev.contains(&TransferEvent::Moved(vec![])));
}

#[test]
fn f03_cross_volume_move_copies_metadata_then_removes_source() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let dir = s.path().join("dir");
    fs::create_dir_all(dir.join("sub")).unwrap();
    let f = dir.join("sub/f");
    fs::write(&f, b"x").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();

    let tuning = Tuning { force_copy: true, ..Tuning::default() };
    let ev = drain(&start_with(req(TransferMode::Move, &[&dir], d.path()), tuning), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Completed, "{ev:?}");
    assert!(!dir.exists());
    assert!(ev.contains(&TransferEvent::Moved(vec![dir.clone()])));
    let md = fs::metadata(d.path().join("dir/sub/f")).unwrap();
    assert_eq!(md.modified().unwrap(), old);
    assert_eq!(md.permissions().mode() & 0o777, 0o640);
}

#[test]
fn f03_permission_denied_is_reported_per_item() {
    if is_root() {
        return;
    }
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let dir = s.path().join("dir");
    let locked = dir.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(dir.join("ok.txt"), b"ok").unwrap();
    fs::write(locked.join("secret"), b"").unwrap();
    let unreadable = s.path().join("unreadable.txt");
    fs::write(&unreadable, b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

    let ev = drain(&start_transfer(req(TransferMode::Copy, &[&dir, &unreadable], d.path())), |_, _| {});
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(final_state(&ev), TransferState::Failed);
    let failed: Vec<_> = items(&ev).into_iter().filter(|r| matches!(r.outcome, ItemOutcome::Failed(_))).map(|r| r.path.clone()).collect();
    assert_eq!(failed, vec![locked.clone(), unreadable.clone()]);
    assert_eq!(fs::read(d.path().join("dir/ok.txt")).unwrap(), b"ok", "the rest of the folder is copied");
    assert_eq!(names(d.path()), vec!["dir"]);
}

#[test]
fn f03_pause_and_resume() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("big");
    let data = big_file(&src, 4 << 20);
    let h = start_with(req(TransferMode::Copy, &[&src], d.path()), slow());
    let mut paused_at = None;
    let ev = drain(&h, |e, h| match e {
        TransferEvent::Progress { bytes_done, .. } if paused_at.is_none() && *bytes_done > 0 => h.pause(),
        TransferEvent::State(TransferState::Paused) => {
            let before = h.events.len();
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(h.events.len(), before, "no progress while paused");
            paused_at = Some(());
            h.resume();
        }
        _ => {}
    });
    assert!(paused_at.is_some());
    let states: Vec<_> = ev.iter().filter_map(|e| if let TransferEvent::State(s) = e { Some(*s) } else { None }).collect();
    assert!(states.windows(2).any(|w| w == [TransferState::Paused, TransferState::Running]), "{states:?}");
    assert_eq!(final_state(&ev), TransferState::Completed);
    assert_eq!(fs::read(d.path().join("big")).unwrap(), data);
}

#[test]
fn f03_byte_progress_within_one_large_file() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("large");
    let len = 64 << 20;
    big_file(&src, len);
    let tuning = Tuning { chunk: 1 << 20, chunk_delay: Duration::from_millis(5), ..Tuning::default() };
    let ev = drain(&start_with(req(TransferMode::Copy, &[&src], d.path()), tuning), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Completed);
    let partial: Vec<u64> = ev
        .iter()
        .filter_map(|e| match e {
            TransferEvent::Progress { bytes_done, current, .. } if *bytes_done > 0 && *bytes_done < len as u64 => {
                assert_eq!(current, &src);
                Some(*bytes_done)
            }
            _ => None,
        })
        .collect();
    assert!(partial.len() >= 2, "{partial:?}");
    assert!(partial.windows(2).all(|w| w[0] < w[1]));
    // Coalesced: far fewer events than the 64 chunks.
    assert!(partial.len() < 20, "{}", partial.len());
}

#[test]
fn f02_duplicate_uses_copy_names() {
    test_env();
    let s = tempfile::tempdir().unwrap();
    let a = s.path().join("a.txt");
    fs::write(&a, b"a").unwrap();
    for _ in 0..2 {
        assert_eq!(final_state(&drain(&duplicate(&a), |_, _| {})), TransferState::Completed);
    }
    assert_eq!(names(s.path()), vec!["a copy 2.txt", "a copy.txt", "a.txt"]);
}

/// Disk full on a real second volume (also exercises a real cross-volume move). Creates and
/// ejects a 16 MB RAM disk with `hdiutil`/`diskutil`; run with `--ignored`.
///
/// Manual checks (not automatable here): unplug a USB volume during a paste (expect Failed
/// items, source intact, UI responsive); paste a 10 GB synthetic fixture
/// (`mkfile 10g`) and keep navigating while it runs.
#[test]
#[ignore]
fn f02_disk_full_on_ram_disk_preserves_source() {
    use std::process::Command;
    test_env();
    let out = Command::new("hdiutil").args(["attach", "-nomount", "ram://32768"]).output().unwrap();
    let dev = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let ok = Command::new("diskutil").args(["erasevolume", "HFS+", "WayfoldFull", &dev]).status().unwrap();
    assert!(ok.success());
    let vol = Path::new("/Volumes/WayfoldFull");
    let s = tempfile::tempdir().unwrap();
    let src = s.path().join("too-big");
    let data = big_file(&src, 32 << 20);

    let ev = drain(&start_transfer(req(TransferMode::Move, &[&src], vol)), |_, _| {});
    let leftovers: Vec<_> = names(vol).into_iter().filter(|n| n.starts_with(crate::ops::journal::TEMP_PREFIX) || n == "too-big").collect();
    Command::new("hdiutil").args(["detach", &dev, "-force"]).status().unwrap();

    assert_eq!(final_state(&ev), TransferState::Failed, "{ev:?}");
    assert_eq!(fs::read(&src).unwrap(), data);
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn f03_not_enough_free_space_fails_before_copying() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let sparse = s.path().join("sparse");
    // 1 PB logical size, no blocks allocated (APFS sparse file).
    fs::File::create(&sparse).unwrap().set_len(1 << 50).unwrap();
    let ev = drain(&start_transfer(req(TransferMode::Copy, &[&sparse], d.path())), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Failed);
    assert_eq!(items(&ev)[0].path, d.path());
    assert!(names(d.path()).is_empty());
}

fn replace_all(e: &TransferEvent, h: &TransferHandle) {
    if matches!(e, TransferEvent::Conflict { .. }) {
        h.resolve(ConflictResolution { action: ConflictAction::Replace, apply_to_remaining: true });
    }
}

fn set_flags(p: &Path, flags: u32) {
    let c = crate::ops::cstr(p).unwrap();
    // SAFETY: valid NUL-terminated path.
    assert_eq!(unsafe { libc::lchflags(c.as_ptr(), flags) }, 0, "{}", std::io::Error::last_os_error());
}

fn flags(p: &Path) -> u32 {
    use std::os::macos::fs::MetadataExt;
    fs::symlink_metadata(p).unwrap().st_flags()
}

/// Runs a worker on this thread with a fixed id, so tests know its temp names.
fn run_worker(id: &str, r: TransferRequest, tuning: Tuning) -> Vec<TransferEvent> {
    let (tx, rx) = async_channel::unbounded();
    super::worker::Worker::new(id.into(), r, tx, Arc::new(Control::default()), tuning).run();
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[test]
fn f03_replace_publish_failure_puts_the_existing_item_back() {
    test_env();
    for (mode, force_copy) in [(TransferMode::Copy, false), (TransferMode::Move, true), (TransferMode::Move, false)] {
        let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let src = s.path().join("a");
        fs::write(&src, b"new").unwrap();
        fs::write(d.path().join("a"), b"old").unwrap();
        let tuning = Tuning { fail_publish: true, force_copy, ..Tuning::default() };
        let ev = drain(&start_with(req(mode, &[&src], d.path()), tuning), replace_all);
        assert_eq!(final_state(&ev), TransferState::Failed, "{mode:?} {ev:?}");
        assert_eq!(fs::read(d.path().join("a")).unwrap(), b"old", "{mode:?}: the existing item is back");
        assert_eq!(fs::read(&src).unwrap(), b"new", "{mode:?}: the source is kept");
        assert_eq!(names(d.path()), vec!["a"], "{mode:?}: no temp left");
    }
}

#[test]
fn f03_move_falling_back_to_copy_after_exdev_asks_once() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("a");
    fs::write(&src, b"new").unwrap();
    fs::write(d.path().join("a"), b"old").unwrap();
    let tuning = Tuning { exdev_once: true.into(), ..Tuning::default() };
    let mut asked = 0;
    let ev = drain(&start_with(req(TransferMode::Move, &[&src], d.path()), tuning), |e, h| {
        if matches!(e, TransferEvent::Conflict { .. }) {
            asked += 1;
            h.resolve(ConflictResolution { action: ConflictAction::Replace, apply_to_remaining: false });
        }
    });
    assert_eq!(final_state(&ev), TransferState::Completed, "{ev:?}");
    assert_eq!(asked, 1, "{ev:?}");
    assert_eq!(fs::read(d.path().join("a")).unwrap(), b"new");
    assert!(!src.exists());
}

#[test]
fn f03_replace_refuses_target_that_contains_the_source_via_a_link() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("a/a");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    // The destination is the source's grandparent, reached through a link: the lexical check misses it.
    let dest = d.path().join("link");
    symlink(s.path(), &dest).unwrap();
    let ev = drain(&start_transfer(req(TransferMode::Move, &[&src], &dest)), replace_all);
    assert_eq!(final_state(&ev), TransferState::Failed, "{ev:?}");
    assert_eq!(fs::read(src.join("f")).unwrap(), b"data");
    let refused = ItemOutcome::Failed("cannot replace a folder that contains the item".into());
    assert!(items(&ev).iter().any(|r| r.path == src && r.outcome == refused), "{ev:?}");
}

#[test]
fn f03_top_level_already_exists_from_copy_is_reported() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("a");
    fs::write(&src, b"x").unwrap();
    fs::create_dir(d.path().join(format!("{}fixed-0", crate::ops::journal::TEMP_PREFIX))).unwrap();
    let ev = run_worker("fixed", req(TransferMode::Copy, &[&src], d.path()), Tuning::default());
    assert_eq!(final_state(&ev), TransferState::Failed, "{ev:?}");
    assert!(items(&ev).iter().any(|r| r.path == src && matches!(r.outcome, ItemOutcome::Failed(_))));
}

#[test]
fn f03_locked_and_read_only_copies_are_cleaned_up_after_failure() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ro = s.path().join("dir/ro");
    fs::create_dir_all(&ro).unwrap();
    let f = ro.join("f");
    fs::write(&f, b"x").unwrap();
    set_flags(&f, libc::UF_IMMUTABLE);
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let tuning = Tuning { fail_publish: true, ..Tuning::default() };
    let ev = drain(&start_with(req(TransferMode::Copy, &[&s.path().join("dir")], d.path()), tuning), |_, _| {});
    assert_eq!(final_state(&ev), TransferState::Failed);
    assert!(names(d.path()).is_empty(), "temp removed: {:?}", names(d.path()));
    set_flags(&f, 0);
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn f03_locked_item_is_published_and_stays_locked() {
    test_env();
    let (s, d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = s.path().join("locked");
    fs::write(&src, b"x").unwrap();
    set_flags(&src, libc::UF_IMMUTABLE);
    let ev = drain(&start_transfer(req(TransferMode::Copy, &[&src], d.path())), |_, _| {});
    let target = d.path().join("locked");
    let target_flags = fs::symlink_metadata(&target).map(|_| flags(&target));
    set_flags(&src, 0);
    if target_flags.is_ok() {
        set_flags(&target, 0);
    }
    assert_eq!(final_state(&ev), TransferState::Completed, "{ev:?}");
    assert_ne!(target_flags.unwrap() & libc::UF_IMMUTABLE, 0);
}
