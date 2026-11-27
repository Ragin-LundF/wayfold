//! Batch rename (F04): rules → before/after preview with problems → apply → undo.
//! Rules work on the name without its extension (folders: the whole name), in this order:
//! replace, case, prefix + suffix, sequence number appended after the suffix.

use super::fileops::rename_set;
use super::{FileIdentity, ItemResult, NameProblem, RenamedItem, UndoAction, identity, is_case_sensitive, validate_name};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseRule {
    Lower,
    Upper,
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub start: u64,
    pub step: u64,
    /// Minimum digits, zero-padded.
    pub padding: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenameRules {
    /// Text to find; empty disables replace.
    pub find: String,
    pub replace: String,
    /// Treat `find` as a regex (`replace` may use `$1`).
    pub regex: bool,
    pub prefix: String,
    pub suffix: String,
    pub sequence: Option<Sequence>,
    pub case: Option<CaseRule>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameProblem {
    Empty,
    /// `/`, NUL, `.`/`..`, too long, or a name that is not valid UTF-8.
    Invalid,
    /// Two rows end with the same name in one folder (case-insensitively where the volume is).
    DuplicateInBatch,
    /// A file outside the batch already has the name.
    Exists,
    /// The source is gone.
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameRow {
    pub before: PathBuf,
    pub after: PathBuf,
    pub problem: Option<RenameProblem>,
    identity: Option<FileIdentity>,
}

impl RenameRow {
    pub fn changes(&self) -> bool {
        self.before != self.after
    }
}

/// Preview a batch. Errors only for an invalid regex. Blocks on stat calls.
pub fn plan_batch_rename(paths: &[PathBuf], rules: &RenameRules) -> Result<Vec<RenameRow>, regex::Error> {
    let re = if rules.regex && !rules.find.is_empty() { Some(Regex::new(&rules.find)?) } else { None };
    let mut rows: Vec<RenameRow> = paths.iter().enumerate().map(|(i, p)| plan_row(p, rules, re.as_ref(), i as u64)).collect();
    mark_collisions(&mut rows);
    Ok(rows)
}

fn plan_row(path: &Path, rules: &RenameRules, re: Option<&Regex>, index: u64) -> RenameRow {
    let id = identity(path).ok();
    let mut row = RenameRow { before: path.to_path_buf(), after: path.to_path_buf(), problem: None, identity: id };
    if id.is_none() {
        row.problem = Some(RenameProblem::Missing);
        return row;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        row.problem = Some(RenameProblem::Invalid);
        return row;
    };
    let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let new_name = apply_rules(name, is_dir, rules, re, index);
    row.after = path.with_file_name(&new_name);
    row.problem = validate_name(&OsString::from(new_name)).err().map(|p| match p {
        NameProblem::Empty => RenameProblem::Empty,
        NameProblem::Invalid => RenameProblem::Invalid,
    });
    row
}

fn apply_rules(name: &str, is_dir: bool, rules: &RenameRules, re: Option<&Regex>, index: u64) -> String {
    let split = if is_dir { None } else { name.rfind('.').filter(|&i| i > 0) };
    let (stem, ext) = name.split_at(split.unwrap_or(name.len()));
    let mut s = match (re, rules.find.is_empty()) {
        (Some(re), _) => re.replace_all(stem, rules.replace.as_str()).into_owned(),
        (None, false) => stem.replace(&rules.find, &rules.replace),
        (None, true) => stem.to_owned(),
    };
    s = match rules.case {
        Some(CaseRule::Lower) => s.to_lowercase(),
        Some(CaseRule::Upper) => s.to_uppercase(),
        Some(CaseRule::Title) => title_case(&s),
        None => s,
    };
    s = format!("{}{s}{}", rules.prefix, rules.suffix);
    if let Some(seq) = rules.sequence {
        let n = seq.start.saturating_add(seq.step.saturating_mul(index));
        s.push_str(&format!("{n:0width$}", width = seq.padding));
    }
    s + ext
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if at_word_start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        at_word_start = c.is_whitespace() || c == '_' || c == '-';
    }
    out
}

/// Marks duplicate targets inside the batch and targets taken by files outside the batch.
fn mark_collisions(rows: &mut [RenameRow]) {
    let members: HashSet<FileIdentity> = rows.iter().filter_map(|r| r.identity).collect();
    let mut sensitivity: HashMap<PathBuf, bool> = HashMap::new();
    let mut key_of = |p: &Path| {
        let dir = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        let sensitive = *sensitivity.entry(dir.clone()).or_insert_with(|| is_case_sensitive(&dir));
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        // shortcut: lowercase folding ignores Unicode normalization (APFS also folds NFD/NFC).
        (dir, if sensitive { name.into_owned() } else { name.to_lowercase() })
    };
    let keys: Vec<_> = rows.iter().map(|r| key_of(&r.after)).collect();
    let mut count: HashMap<&(PathBuf, String), usize> = HashMap::new();
    for k in &keys {
        *count.entry(k).or_default() += 1;
    }
    for (row, key) in rows.iter_mut().zip(&keys) {
        if row.problem.is_some() || row.identity.is_none() {
            continue;
        }
        if count[key] > 1 {
            row.problem = Some(RenameProblem::DuplicateInBatch);
        } else if row.changes() && identity(&row.after).is_ok_and(|id| !members.contains(&id)) {
            row.problem = Some(RenameProblem::Exists);
        }
    }
}

/// Apply a planned batch. Refused (`Err`) while any row has a problem. Rows that changed on
/// disk since planning fail individually; the rest still apply.
pub fn apply_batch_rename(rows: &[RenameRow]) -> Result<(Vec<ItemResult>, Option<UndoAction>), RenameProblem> {
    if let Some(p) = rows.iter().find_map(|r| r.problem) {
        return Err(p);
    }
    let items: Vec<RenamedItem> = rows
        .iter()
        .filter(|r| r.changes())
        .filter_map(|r| Some(RenamedItem { from: r.before.clone(), to: r.after.clone(), identity: r.identity? }))
        .collect();
    let (results, renamed) = rename_set(&items);
    let undo = (!renamed.is_empty()).then_some(UndoAction::Renamed(renamed));
    Ok((results, undo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{ItemOutcome, test_env, undo};

    fn files(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| {
                let p = dir.join(n);
                fs::write(&p, n.as_bytes()).unwrap();
                p
            })
            .collect()
    }

    fn after_names(rows: &[RenameRow]) -> Vec<String> {
        rows.iter().map(|r| r.after.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn f04_rules_preview() {
        let t = tempfile::tempdir().unwrap();
        let p = files(t.path(), &["IMG_one.JPG", "IMG_two.JPG"]);
        let rules = RenameRules {
            find: "IMG_".into(),
            replace: "".into(),
            prefix: "trip ".into(),
            suffix: " #".into(),
            sequence: Some(Sequence { start: 1, step: 2, padding: 3 }),
            case: Some(CaseRule::Title),
            ..Default::default()
        };
        let rows = plan_batch_rename(&p, &rules).unwrap();
        assert_eq!(after_names(&rows), vec!["trip One #001.JPG", "trip Two #003.JPG"]);
        assert!(rows.iter().all(|r| r.problem.is_none()));

        let rules = RenameRules { find: r"IMG_(\w+)".into(), replace: "${1}_x".into(), regex: true, ..Default::default() };
        assert_eq!(after_names(&plan_batch_rename(&p, &rules).unwrap()), vec!["one_x.JPG", "two_x.JPG"]);
        let bad = RenameRules { find: "(".into(), regex: true, ..Default::default() };
        assert!(plan_batch_rename(&p, &bad).is_err());
    }

    #[test]
    fn f04_duplicate_targets_invalid_names_and_existing_files_block_apply() {
        let t = tempfile::tempdir().unwrap();
        let p = files(t.path(), &["a1.txt", "a2.txt", "keep.txt"]);
        files(t.path(), &["Z.txt"]);
        let dup = RenameRules { find: "1".into(), replace: "2".into(), ..Default::default() };
        let rows = plan_batch_rename(&p[..2], &dup).unwrap();
        assert_eq!(rows[0].problem, Some(RenameProblem::DuplicateInBatch));
        assert_eq!(rows[1].problem, Some(RenameProblem::DuplicateInBatch), "unchanged row keeps its name");
        assert_eq!(apply_batch_rename(&rows).unwrap_err(), RenameProblem::DuplicateInBatch);
        assert!(p[0].exists());

        let slash = RenameRules { find: "a".into(), replace: "x/".into(), ..Default::default() };
        assert_eq!(plan_batch_rename(&p[..1], &slash).unwrap()[0].problem, Some(RenameProblem::Invalid));
        let empty = RenameRules { find: "keep".into(), ..Default::default() };
        let rows = plan_batch_rename(&p[2..], &empty).unwrap();
        assert_eq!(after_names(&rows), vec![".txt"], "extension-only name is allowed (hidden file)");

        let exists = RenameRules { find: "keep".into(), replace: "z".into(), ..Default::default() };
        let rows = plan_batch_rename(&p[2..], &exists).unwrap();
        let expected = if is_case_sensitive(t.path()) { None } else { Some(RenameProblem::Exists) };
        assert_eq!(rows[0].problem, expected, "case-insensitive collision with Z.txt");
    }

    #[test]
    fn f04_rename_cycle_swap_and_undo() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let p = files(t.path(), &["a", "b"]);
        let rows = vec![
            RenameRow { before: p[0].clone(), after: p[1].clone(), problem: None, identity: identity(&p[0]).ok() },
            RenameRow { before: p[1].clone(), after: p[0].clone(), problem: None, identity: identity(&p[1]).ok() },
        ];
        // The planner accepts the swap: both targets belong to batch members.
        let planned: Vec<_> = rows.iter().map(|r| r.before.clone()).collect();
        assert!(plan_batch_rename(&planned, &RenameRules::default()).unwrap().iter().all(|r| r.problem.is_none()));

        let (results, u) = apply_batch_rename(&rows).unwrap();
        assert!(results.iter().all(|r| matches!(r.outcome, ItemOutcome::Done(_))));
        assert_eq!(fs::read(&p[0]).unwrap(), b"b");
        assert_eq!(fs::read(&p[1]).unwrap(), b"a");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 2, "no hop files left");

        assert!(undo(&u.unwrap()).iter().all(|r| matches!(r.outcome, ItemOutcome::Done(_))));
        assert_eq!(fs::read(&p[0]).unwrap(), b"a");
    }

    #[test]
    fn f04_existing_target_race_fails_that_item_only() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let p = files(t.path(), &["a", "b"]);
        let rules = RenameRules { suffix: "-new".into(), ..Default::default() };
        let rows = plan_batch_rename(&p, &rules).unwrap();
        fs::write(t.path().join("a-new"), b"intruder").unwrap(); // created after planning
        let (results, u) = apply_batch_rename(&rows).unwrap();
        assert!(matches!(results[0].outcome, ItemOutcome::Failed(_)));
        assert!(matches!(results[1].outcome, ItemOutcome::Done(_)));
        assert_eq!(fs::read(t.path().join("a-new")).unwrap(), b"intruder");
        assert_eq!(fs::read(&p[0]).unwrap(), b"a");
        assert!(matches!(u, Some(UndoAction::Renamed(ref v)) if v.len() == 1));
    }

    #[test]
    fn all_rows_failing_records_no_undo() {
        test_env();
        let t = tempfile::tempdir().unwrap();
        let p = files(t.path(), &["a", "b"]);
        let rules = RenameRules { suffix: "-new".into(), ..Default::default() };
        let rows = plan_batch_rename(&p, &rules).unwrap();
        fs::write(t.path().join("a-new"), b"intruder").unwrap();
        fs::write(t.path().join("b-new"), b"intruder").unwrap();
        let (results, u) = apply_batch_rename(&rows).unwrap();
        assert!(results.iter().all(|r| matches!(r.outcome, ItemOutcome::Failed(_))));
        assert!(u.is_none(), "an empty batch must not record an undo step");
    }
}
