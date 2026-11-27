//! Scoped Spotlight queries through `mdfind -onlyin` (S01, tag sidebar). No index of our own.
//! The child process is killed when the query is cancelled.

use crate::pool::CancelToken;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Escape a value for a Spotlight query string literal.
fn quote(v: &str) -> String {
    let mut s = String::with_capacity(v.len() + 2);
    s.push('"');
    for c in v.chars() {
        if matches!(c, '"' | '\\' | '*') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// Filename contains `text` (case/diacritic-insensitive).
pub fn name_query(text: &str) -> String {
    let q = quote(text);
    format!("kMDItemFSName == \"*{}*\"cd", &q[1..q.len() - 1])
}

/// Items carrying any of `names` (a tag's localized and English names).
pub fn tag_query(names: &[&str]) -> String {
    names.iter().map(|n| format!("kMDItemUserTags == {}", quote(n))).collect::<Vec<_>>().join(" || ")
}

/// Run `query` scoped to `root`, streaming paths in batches. Returns Err when Spotlight is
/// unavailable (caller falls back to a scan).
pub fn run(
    query: &str,
    root: &Path,
    cancel: &CancelToken,
    limit: usize,
    mut on_batch: impl FnMut(Vec<PathBuf>) -> bool,
) -> Result<(), String> {
    let mut child = Command::new("/usr/bin/mdfind")
        .arg("-onlyin")
        .arg(root)
        .arg(query)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Spotlight unavailable: {e}"))?;
    let out = child.stdout.take().expect("piped stdout");
    let mut batch = Vec::new();
    let mut n = 0;
    for line in BufReader::new(out).split(b'\n') {
        if cancel.is_cancelled() || n >= limit {
            break;
        }
        let Ok(line) = line else { break };
        use std::os::unix::ffi::OsStringExt;
        batch.push(PathBuf::from(std::ffi::OsString::from_vec(line)));
        n += 1;
        if batch.len() == 200 && !on_batch(std::mem::take(&mut batch)) {
            break;
        }
    }
    let _ = child.kill();
    let status = child.wait().map_err(|e| e.to_string())?;
    if !batch.is_empty() {
        on_batch(batch);
    }
    // mdfind exits non-zero when indexing is disabled for the volume.
    if n == 0 && !status.success() && !cancel.is_cancelled() {
        return Err("Spotlight index unavailable for this location".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s01_query_escapes_quotes_and_wildcards() {
        assert_eq!(name_query("a\"b*"), r#"kMDItemFSName == "*a\"b\**"cd"#);
        assert_eq!(tag_query(&["Rot", "Red"]), r#"kMDItemUserTags == "Rot" || kMDItemUserTags == "Red""#);
    }
}
