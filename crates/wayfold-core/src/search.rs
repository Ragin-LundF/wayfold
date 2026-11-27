//! On-demand search without a persistent index (S01/S02): bounded recursive filename scan and
//! content/regex search. No symlink following, explicit exclusions, unreadable paths reported.

use crate::pool::CancelToken;
use regex::bytes::{Regex, RegexBuilder};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub include_hidden: bool,
    pub max_results: usize,
    /// Directory names never descended into.
    pub exclude_dirs: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            max_results: 5_000,
            exclude_dirs: [".git", "node_modules", "target", ".Trash"].map(String::from).to_vec(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    /// Unreadable paths with the reason (S02 acceptance: reported, not hidden).
    pub skipped: Vec<(PathBuf, String)>,
    pub truncated: bool,
    pub cancelled: bool,
}

/// Depth-first walk calling `visit` for every non-excluded entry. Returns false from `visit` to stop.
fn walk(
    root: &Path,
    opts: &ScanOptions,
    cancel: &CancelToken,
    report: &mut ScanReport,
    mut visit: impl FnMut(&Path, &fs::Metadata) -> bool,
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            report.cancelled = true;
            return;
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                report.skipped.push((dir, e.to_string()));
                continue;
            }
        };
        for item in rd.flatten() {
            let name = item.file_name();
            let hidden = name.as_encoded_bytes().first() == Some(&b'.');
            if hidden && !opts.include_hidden {
                continue;
            }
            let Ok(md) = item.metadata() else { continue };
            let path = item.path();
            if !visit(&path, &md) {
                return;
            }
            if md.is_dir() && !opts.exclude_dirs.iter().any(|x| name == x.as_str()) {
                stack.push(path);
            }
        }
    }
}

/// Recursive filename search: case-insensitive substring of the name.
pub fn find_names(
    root: &Path,
    needle: &str,
    opts: &ScanOptions,
    cancel: &CancelToken,
    mut on_batch: impl FnMut(Vec<PathBuf>),
) -> ScanReport {
    let needle = needle.to_lowercase();
    let mut report = ScanReport::default();
    let mut batch = Vec::new();
    let mut n = 0;
    let mut truncated = false;
    walk(root, opts, cancel, &mut report, |p, _| {
        let name = p.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if name.contains(&needle) {
            batch.push(p.to_path_buf());
            n += 1;
            if batch.len() >= 100 {
                on_batch(std::mem::take(&mut batch));
            }
            if n >= opts.max_results {
                truncated = true;
                return false;
            }
        }
        true
    });
    if !batch.is_empty() {
        on_batch(batch);
    }
    report.truncated = truncated;
    report
}

#[derive(Clone, Debug)]
pub struct ContentQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_insensitive: bool,
    /// Lowercase extensions without dot; empty = all text files.
    pub extensions: Vec<String>,
    /// Larger files are skipped (reported).
    pub max_file_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentHit {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

/// Compile with bounded resources so a pathological pattern cannot exhaust memory.
pub fn compile(q: &ContentQuery) -> Result<Regex, regex::Error> {
    let pat = if q.regex { q.pattern.clone() } else { regex::escape(&q.pattern) };
    RegexBuilder::new(&pat).case_insensitive(q.case_insensitive).size_limit(1 << 20).dfa_size_limit(4 << 20).build()
}

/// Content search; first matching line per file is reported. Binary files (NUL in the first
/// 8 KiB) are excluded.
pub fn find_content(
    root: &Path,
    q: &ContentQuery,
    opts: &ScanOptions,
    cancel: &CancelToken,
    mut on_batch: impl FnMut(Vec<ContentHit>),
) -> Result<ScanReport, regex::Error> {
    let re = compile(q)?;
    let mut report = ScanReport::default();
    let mut skipped = Vec::new();
    let mut batch = Vec::new();
    let mut n = 0;
    let mut truncated = false;
    let mut buf = Vec::new();
    walk(root, opts, cancel, &mut report, |p, md| {
        if !md.is_file() {
            return true;
        }
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !q.extensions.is_empty() && !q.extensions.contains(&ext) {
            return true;
        }
        if md.len() > q.max_file_bytes {
            skipped.push((p.to_path_buf(), format!("larger than {} bytes", q.max_file_bytes)));
            return true;
        }
        buf.clear();
        match fs::File::open(p).and_then(|f| f.take(q.max_file_bytes).read_to_end(&mut buf)) {
            Ok(_) => {}
            Err(e) => {
                skipped.push((p.to_path_buf(), e.to_string()));
                return true;
            }
        }
        if buf[..buf.len().min(8192)].contains(&0) {
            return true;
        }
        if let Some(m) = re.find(&buf) {
            let line = buf[..m.start()].iter().filter(|&&b| b == b'\n').count() + 1;
            let start = buf[..m.start()].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            let end = buf[m.end()..].iter().position(|&b| b == b'\n').map_or(buf.len(), |i| m.end() + i);
            let text: String = String::from_utf8_lossy(&buf[start..end]).chars().take(200).collect();
            batch.push(ContentHit { path: p.to_path_buf(), line, text });
            n += 1;
            if batch.len() >= 50 {
                on_batch(std::mem::take(&mut batch));
            }
            if n >= opts.max_results {
                truncated = true;
                return false;
            }
        }
        !cancel.is_cancelled()
    });
    if !batch.is_empty() {
        on_batch(batch);
    }
    report.skipped.extend(skipped);
    report.truncated = truncated;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("a/b")).unwrap();
        fs::create_dir_all(t.path().join(".git")).unwrap();
        fs::write(t.path().join("a/b/Report.md"), "hello\nfind ME here\n").unwrap();
        fs::write(t.path().join("a/report.bin"), b"find ME\0binary").unwrap();
        fs::write(t.path().join(".git/report"), "find ME").unwrap();
        std::os::unix::fs::symlink(t.path(), t.path().join("a/loop")).unwrap();
        t
    }

    #[test]
    fn s01_recursive_names_skip_hidden_and_links() {
        let t = fixture();
        let mut found = vec![];
        let r = find_names(t.path(), "report", &ScanOptions::default(), &CancelToken::new(), |b| found.extend(b));
        found.sort();
        assert_eq!(found, [t.path().join("a/b/Report.md"), t.path().join("a/report.bin")]);
        assert!(!r.truncated);
    }

    #[test]
    fn s02_content_regex_excludes_binary_and_reports_unreadable() {
        let t = fixture();
        fs::create_dir(t.path().join("locked")).unwrap();
        fs::set_permissions(t.path().join("locked"), std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let q =
            ContentQuery { pattern: "find\\s+me".into(), regex: true, case_insensitive: true, extensions: vec![], max_file_bytes: 1 << 20 };
        let mut hits = vec![];
        let r = find_content(t.path(), &q, &ScanOptions::default(), &CancelToken::new(), |b| hits.extend(b)).unwrap();
        fs::set_permissions(t.path().join("locked"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert!(r.skipped.iter().any(|(p, _)| p.ends_with("locked")));
    }

    #[test]
    fn s02_invalid_regex_errors_and_limit_truncates() {
        let q = ContentQuery { pattern: "(".into(), regex: true, case_insensitive: false, extensions: vec![], max_file_bytes: 10 };
        assert!(compile(&q).is_err());
        let t = fixture();
        let opts = ScanOptions { max_results: 1, ..Default::default() };
        let r = find_names(t.path(), "", &opts, &CancelToken::new(), |_| {});
        assert!(r.truncated);
    }

    #[test]
    fn cancel_stops_scan() {
        let t = fixture();
        let c = CancelToken::new();
        c.cancel();
        let r = find_names(t.path(), "x", &ScanOptions::default(), &c, |_| {});
        assert!(r.cancelled);
    }
}
