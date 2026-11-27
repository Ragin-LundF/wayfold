//! On-demand disk usage (P04). Builds an in-memory size tree of one folder with a bounded
//! number of worker threads; nothing is persisted and the caller drops the tree when done.
//!
//! Policy:
//! - Symlinks are counted as the link itself and never followed.
//! - The scan stays on the root's volume (like `du -x`); other mounts are skipped.
//! - Hardlinks: a file with more than one link is counted once per scan (first path seen).
//! - Logical bytes = file length; allocated bytes = `st_blocks * 512`. Folders themselves add
//!   no bytes; only their contents do.
//! - Unreadable folders/files are counted as errors with their path; the scan continues.

use crate::pool::{self, CancelToken};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const WORKERS: usize = 4;
/// Files kept per folder; smaller ones are folded into one summary node to bound memory.
const MAX_FILES_PER_DIR: usize = 500;
/// Error paths kept for display; all errors are counted.
const MAX_ERROR_PATHS: usize = 200;
const PROGRESS_EVERY: u64 = 2048;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageNode {
    pub name: OsString,
    pub is_dir: bool,
    pub logical: u64,
    pub allocated: u64,
    /// Files (and links) below this node, including itself when it is one.
    pub items: u64,
    /// Sorted by allocated bytes, largest first.
    pub children: Vec<UsageNode>,
    /// Summary node standing for `items` smaller files of the folder.
    pub folded: bool,
}

#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    pub root: UsageNode,
    pub error_count: u64,
    pub errors: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

#[derive(Clone, Debug)]
pub enum UsageEvent {
    Progress { items: u64, allocated: u64 },
    Done(Box<UsageReport>),
}

pub struct UsageHandle {
    pub events: async_channel::Receiver<UsageEvent>,
    pub cancel: CancelToken,
}

pub fn start_scan(root: PathBuf) -> UsageHandle {
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    pool::for_path(&root).execute(move || {
        let progress_tx = tx.clone();
        let report = scan(&root, &token, &move |items, allocated| {
            let _ = progress_tx.try_send(UsageEvent::Progress { items, allocated });
        });
        let _ = tx.send_blocking(UsageEvent::Done(Box::new(report)));
    });
    UsageHandle { events: rx, cancel }
}

struct Shared<'a> {
    dev: u64,
    cancel: &'a CancelToken,
    progress: &'a (dyn Fn(u64, u64) + Sync),
    seen_inodes: Mutex<HashSet<(u64, u64)>>,
    errors: Mutex<(u64, Vec<(PathBuf, String)>)>,
    items: AtomicU64,
    allocated: AtomicU64,
}

impl Shared<'_> {
    fn error(&self, path: &Path, e: impl ToString) {
        let mut g = self.errors.lock().unwrap();
        g.0 += 1;
        if g.1.len() < MAX_ERROR_PATHS {
            g.1.push((path.to_path_buf(), e.to_string()));
        }
    }

    fn count(&self, allocated: u64) {
        let n = self.items.fetch_add(1, Ordering::Relaxed) + 1;
        let a = self.allocated.fetch_add(allocated, Ordering::Relaxed) + allocated;
        if n.is_multiple_of(PROGRESS_EVERY) {
            (self.progress)(n, a);
        }
    }
}

/// Blocking scan of `root`. `progress(items, allocated)` is called from worker threads.
pub fn scan(root: &Path, cancel: &CancelToken, progress: &(dyn Fn(u64, u64) + Sync)) -> UsageReport {
    let name = root.file_name().map(OsString::from).unwrap_or_else(|| root.as_os_str().into());
    let md = match fs::symlink_metadata(root) {
        Ok(md) => md,
        Err(e) => {
            return UsageReport {
                root: UsageNode { name, ..Default::default() },
                error_count: 1,
                errors: vec![(root.into(), e.to_string())],
                cancelled: false,
            };
        }
    };
    let shared = Shared {
        dev: md.dev(),
        cancel,
        progress,
        seen_inodes: Mutex::new(HashSet::new()),
        errors: Mutex::new((0, Vec::new())),
        items: AtomicU64::new(0),
        allocated: AtomicU64::new(0),
    };
    let root_node = if md.is_dir() { scan_root_parallel(root, name, &shared) } else { file_node(name, &md, &shared) };
    let (error_count, errors) = shared.errors.into_inner().unwrap();
    progress(shared.items.into_inner(), shared.allocated.into_inner());
    UsageReport { root: root_node, error_count, errors, cancelled: cancel.is_cancelled() }
}

/// The root's direct child folders are spread over `WORKERS` threads; each scans its
/// subtrees depth-first.
fn scan_root_parallel(root: &Path, name: OsString, shared: &Shared) -> UsageNode {
    let (files, dirs) = read_children(root, shared);
    let queue = Mutex::new(dirs.into_iter());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap().next();
                    let Some((path, child_name)) = next else { break };
                    let node = scan_dir(&path, child_name, shared);
                    results.lock().unwrap().push(node);
                }
            });
        }
    });
    let mut children = results.into_inner().unwrap();
    children.extend(files);
    finish_dir(name, children)
}

fn scan_dir(dir: &Path, name: OsString, shared: &Shared) -> UsageNode {
    let (mut children, dirs) = read_children(dir, shared);
    for (path, child_name) in dirs {
        children.push(scan_dir(&path, child_name, shared));
    }
    finish_dir(name, children)
}

/// File nodes of `dir` plus the subfolders still to scan (same volume only).
fn read_children(dir: &Path, shared: &Shared) -> (Vec<UsageNode>, Vec<(PathBuf, OsString)>) {
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    if shared.cancel.is_cancelled() {
        return (files, dirs);
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            shared.error(dir, e);
            return (files, dirs);
        }
    };
    for entry in rd {
        if shared.cancel.is_cancelled() {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                shared.error(dir, e);
                continue;
            }
        };
        let path = entry.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(md) => md,
            Err(e) => {
                shared.error(&path, e);
                continue;
            }
        };
        if md.is_dir() {
            if md.dev() == shared.dev {
                dirs.push((path, entry.file_name()));
            }
        } else {
            files.push(file_node(entry.file_name(), &md, shared));
        }
    }
    (files, dirs)
}

fn file_node(name: OsString, md: &fs::Metadata, shared: &Shared) -> UsageNode {
    let counted = md.nlink() <= 1 || shared.seen_inodes.lock().unwrap().insert((md.dev(), md.ino()));
    let (logical, allocated) = if counted { (md.len(), md.blocks() * 512) } else { (0, 0) };
    shared.count(allocated);
    UsageNode { name, is_dir: false, logical, allocated, items: 1, children: Vec::new(), folded: false }
}

fn finish_dir(name: OsString, mut children: Vec<UsageNode>) -> UsageNode {
    children.sort_by(|a, b| b.allocated.cmp(&a.allocated).then(b.logical.cmp(&a.logical)));
    let file_count = children.iter().filter(|c| !c.is_dir).count();
    if file_count > MAX_FILES_PER_DIR {
        let mut kept_files = 0;
        let mut rest = UsageNode { folded: true, ..Default::default() };
        children.retain(|c| {
            if c.is_dir || kept_files < MAX_FILES_PER_DIR {
                kept_files += usize::from(!c.is_dir);
                return true;
            }
            rest.logical += c.logical;
            rest.allocated += c.allocated;
            rest.items += 1;
            false
        });
        rest.name = format!("{} smaller files", rest.items).into();
        children.push(rest);
    }
    UsageNode {
        logical: children.iter().map(|c| c.logical).sum(),
        allocated: children.iter().map(|c| c.allocated).sum(),
        items: children.iter().map(|c| c.items).sum(),
        name,
        is_dir: true,
        children,
        folded: false,
    }
}

/// Squarified treemap layout (Bruls et al.) of `sizes` (sorted descending) into a `w`×`h`
/// rectangle. Returns `(x, y, w, h)` per input index; zero sizes get empty rectangles.
pub fn squarify(sizes: &[u64], w: f32, h: f32) -> Vec<(f32, f32, f32, f32)> {
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    let mut out = vec![(0.0, 0.0, 0.0, 0.0); sizes.len()];
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let scale = (w as f64 * h as f64) / total;
    let areas: Vec<f64> = sizes.iter().map(|&s| s as f64 * scale).collect();
    let (mut x, mut y, mut rw, mut rh) = (0.0f64, 0.0f64, w as f64, h as f64);
    let mut i = 0;
    while i < areas.len() && areas[i] > 0.0 {
        let side = rw.min(rh);
        let mut end = i + 1;
        while end < areas.len() && areas[end] > 0.0 && worst(&areas[i..=end], side) <= worst(&areas[i..end], side) {
            end += 1;
        }
        let row_area: f64 = areas[i..end].iter().sum();
        let thickness = row_area / side;
        let mut offset = 0.0;
        for (k, a) in areas[i..end].iter().enumerate() {
            let len = a / thickness;
            out[i + k] = if rw >= rh {
                (x as f32, (y + offset) as f32, thickness as f32, len as f32)
            } else {
                ((x + offset) as f32, y as f32, len as f32, thickness as f32)
            };
            offset += len;
        }
        if rw >= rh {
            x += thickness;
            rw -= thickness;
        } else {
            y += thickness;
            rh -= thickness;
        }
        i = end;
    }
    out
}

/// Worst aspect ratio of a row laid along `side`.
fn worst(row: &[f64], side: f64) -> f64 {
    let sum: f64 = row.iter().sum();
    let (max, min) = row.iter().fold((0.0f64, f64::MAX), |(mx, mn), &a| (mx.max(a), mn.min(a)));
    let s2 = side * side;
    (s2 * max / (sum * sum)).max(sum * sum / (s2 * min))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn no_progress() -> impl Fn(u64, u64) + Sync {
        |_, _| {}
    }

    fn child<'a>(n: &'a UsageNode, name: &str) -> &'a UsageNode {
        n.children.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no child {name}"))
    }

    #[test]
    fn p04_totals_hardlinks_once_and_symlinks_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/big.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(root.join("small.txt"), b"hello").unwrap();
        fs::hard_link(root.join("a/b/big.bin"), root.join("hard.bin")).unwrap();
        fs::create_dir(t.path().join("outside")).unwrap();
        fs::write(t.path().join("outside/huge.bin"), vec![0u8; 500_000]).unwrap();
        symlink(t.path().join("outside"), root.join("link")).unwrap();

        let r = scan(&root, &CancelToken::new(), &no_progress());
        assert_eq!(r.error_count, 0);
        assert!(!r.cancelled);
        let link_len = fs::symlink_metadata(root.join("link")).unwrap().len();
        // big.bin counted once (hardlink), small.txt, the link itself; outside/ not traversed.
        assert_eq!(r.root.logical, 100_000 + 5 + link_len);
        assert_eq!(r.root.items, 4);
        assert!(r.root.allocated >= 100_000);
        assert_eq!(child(child(&r.root, "a"), "b").children[0].name, "big.bin");
        let a = child(&r.root, "a").logical + child(&r.root, "hard.bin").logical;
        assert_eq!(a, 100_000, "the hardlinked inode is counted exactly once");
        assert!(r.root.children.windows(2).all(|w| w[0].allocated >= w[1].allocated), "children sorted largest first");
    }

    #[test]
    fn p04_partial_errors_are_counted_with_paths() {
        let t = tempfile::tempdir().unwrap();
        let locked = t.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("f"), b"x").unwrap();
        fs::write(t.path().join("ok.txt"), b"abc").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let r = scan(t.path(), &CancelToken::new(), &no_progress());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.error_count, 1);
        assert_eq!(r.errors[0].0, locked);
        assert_eq!(r.root.logical, 3);
    }

    #[test]
    fn p04_cancel_marks_report() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("d/f"), b"x").unwrap();
        let c = CancelToken::new();
        c.cancel();
        let r = scan(t.path(), &c, &no_progress());
        assert!(r.cancelled);
        assert_eq!(r.root.items, 0);
    }

    #[test]
    fn p04_many_files_fold_into_summary_node() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..(MAX_FILES_PER_DIR + 10) {
            fs::write(t.path().join(format!("f{i}")), vec![0u8; i + 1]).unwrap();
        }
        let r = scan(t.path(), &CancelToken::new(), &no_progress());
        assert_eq!(r.root.children.len(), MAX_FILES_PER_DIR + 1);
        let folded = r.root.children.last().unwrap();
        assert!(folded.folded);
        assert_eq!(folded.items, 10);
        assert_eq!(r.root.items, (MAX_FILES_PER_DIR + 10) as u64);
        let expected: u64 = (1..=(MAX_FILES_PER_DIR as u64 + 10)).sum();
        assert_eq!(r.root.logical, expected);
    }

    #[test]
    fn p04_squarify_fills_area_without_overlap() {
        let sizes = [600, 300, 60, 30, 10, 0];
        let rects = squarify(&sizes, 400.0, 300.0);
        let area: f32 = rects.iter().map(|r| r.2 * r.3).sum();
        assert!((area - 120_000.0).abs() < 1.0, "{area}");
        assert_eq!(rects[5], (0.0, 0.0, 0.0, 0.0));
        for (i, a) in rects.iter().enumerate() {
            assert!(a.0 >= -0.01 && a.1 >= -0.01 && a.0 + a.2 <= 400.01 && a.1 + a.3 <= 300.01);
            for b in &rects[i + 1..] {
                let overlap_w = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
                let overlap_h = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
                assert!(overlap_w <= 0.01 || overlap_h <= 0.01, "{a:?} overlaps {b:?}");
            }
        }
    }
}
