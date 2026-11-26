//! Asynchronous folder size (P03): logical bytes and item count, no symlink traversal.
//! Hardlink policy: every directory entry counts (a file with two links counts twice),
//! matching `ls`-style logical totals; disk usage analysis reports allocated bytes separately.

use crate::pool::CancelToken;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeTotals {
    pub bytes: u64,
    pub items: u64,
    /// Unreadable subfolders/entries; a non-zero value means the total is partial.
    pub errors: u64,
}

/// Walk `root` depth-first with an explicit stack. `progress` is called roughly every 2000 items.
pub fn folder_size(root: &Path, cancel: &CancelToken, mut progress: impl FnMut(SizeTotals)) -> SizeTotals {
    let mut t = SizeTotals::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(rd) = fs::read_dir(&dir) else {
            t.errors += 1;
            continue;
        };
        for item in rd {
            let Ok(item) = item else {
                t.errors += 1;
                continue;
            };
            match item.metadata() {
                // DirEntry::metadata does not follow symlinks.
                Ok(md) if md.is_dir() => stack.push(item.path()),
                Ok(md) => t.bytes += md.len(),
                Err(_) => t.errors += 1,
            }
            t.items += 1;
            if t.items % 2000 == 0 {
                progress(t);
                if cancel.is_cancelled() {
                    return t;
                }
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p03_counts_bytes_without_following_links() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("a")).unwrap();
        fs::write(t.path().join("a/x"), vec![0u8; 100]).unwrap();
        fs::write(t.path().join("y"), vec![0u8; 50]).unwrap();
        std::os::unix::fs::symlink(t.path(), t.path().join("a/loop")).unwrap();
        let r = folder_size(t.path(), &CancelToken::new(), |_| {});
        assert_eq!(r.items, 4);
        assert!(r.bytes >= 150 && r.bytes < 150 + 4096, "symlink counted as link size only");
        assert_eq!(r.errors, 0);
    }
}
