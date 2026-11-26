//! Workspace state: panes, tabs, history, selection, view preferences. Pure data so the
//! reducer logic is testable without a window. Entry identity is its path.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use waypoint_core::fs::{Entry, Visibility};
use waypoint_core::storage::path_bytes;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ViewMode {
    #[default]
    List,
    Icons,
    Columns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Kind,
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
    pub folders_first: bool,
    /// Group rows by kind with section headers (N02).
    #[serde(default)]
    pub group_kind: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { key: SortKey::Name, ascending: true, folders_first: true, group_kind: false }
    }
}

/// Visibility filters shared by the window (N04).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Filters {
    pub show_hidden: bool,
    pub show_system: bool,
}

impl Filters {
    pub fn allows(&self, e: &Entry) -> bool {
        match e.visibility {
            Visibility::Normal => true,
            Visibility::Hidden => self.show_hidden,
            Visibility::System => self.show_system,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub location: PathBuf,
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
    pub view: ViewMode,
    pub sort: Sort,
    /// Current-folder name filter (S01 default scope).
    pub filter: String,
    pub selection: HashSet<PathBuf>,
    /// Shift-range anchor and keyboard cursor.
    pub anchor: Option<PathBuf>,
    pub cursor: Option<PathBuf>,
    /// Bumped on every navigation; worker results carrying an older value are discarded.
    pub generation: u64,
}

impl Tab {
    pub fn new(location: PathBuf) -> Self {
        Self {
            id: TabId(next_id()),
            location,
            back: vec![],
            forward: vec![],
            view: ViewMode::default(),
            sort: Sort::default(),
            filter: String::new(),
            selection: HashSet::new(),
            anchor: None,
            cursor: None,
            generation: next_id(),
        }
    }

    pub fn title(&self) -> String {
        display_name(&self.location)
    }

    /// Navigate to `path`, pushing history. Returns false when already there.
    pub fn navigate(&mut self, path: PathBuf) -> bool {
        if path == self.location {
            return false;
        }
        let prev = std::mem::replace(&mut self.location, path);
        // Coming back up selects the folder we came from, like Finder.
        let came_from_child = prev.parent() == Some(self.location.as_path());
        self.back.push(prev.clone());
        self.forward.clear();
        self.reset_for_location(came_from_child.then_some(prev));
        true
    }

    pub fn go_back(&mut self) -> bool {
        let Some(p) = self.back.pop() else { return false };
        let prev = std::mem::replace(&mut self.location, p);
        self.forward.push(prev.clone());
        self.reset_for_location(Some(prev));
        true
    }

    pub fn go_forward(&mut self) -> bool {
        let Some(p) = self.forward.pop() else { return false };
        let prev = std::mem::replace(&mut self.location, p);
        self.back.push(prev);
        self.reset_for_location(None);
        true
    }

    fn reset_for_location(&mut self, select: Option<PathBuf>) {
        self.selection.clear();
        self.filter.clear();
        self.anchor = select.clone();
        self.cursor = select.clone();
        if let Some(s) = select {
            self.selection.insert(s);
        }
        self.generation = next_id();
    }

    /// Plain click / keyboard move: select exactly one.
    pub fn select_only(&mut self, path: &Path) {
        self.selection.clear();
        self.selection.insert(path.to_path_buf());
        self.anchor = Some(path.to_path_buf());
        self.cursor = Some(path.to_path_buf());
    }

    /// Cmd-click.
    pub fn toggle(&mut self, path: &Path) {
        if !self.selection.remove(path) {
            self.selection.insert(path.to_path_buf());
        }
        self.anchor = Some(path.to_path_buf());
        self.cursor = Some(path.to_path_buf());
    }

    /// Shift-click / shift-arrow over the visible order.
    pub fn select_range(&mut self, rows: &[&Entry], to: &Path) {
        let anchor = self.anchor.clone().unwrap_or_else(|| to.to_path_buf());
        let a = rows.iter().position(|e| e.path == anchor);
        let b = rows.iter().position(|e| e.path == to);
        let (Some(a), Some(b)) = (a, b) else {
            return self.select_only(to);
        };
        self.selection = rows[a.min(b)..=a.max(b)].iter().map(|e| e.path.clone()).collect();
        self.anchor = Some(anchor);
        self.cursor = Some(to.to_path_buf());
    }

    pub fn select_all(&mut self, rows: &[&Entry]) {
        self.selection = rows.iter().map(|e| e.path.clone()).collect();
    }

    /// Arrow-key movement by `delta` rows; `extend` keeps the anchor (shift).
    pub fn move_cursor(&mut self, rows: &[&Entry], delta: isize, extend: bool) {
        if rows.is_empty() {
            return;
        }
        let cur = self.cursor.as_ref().and_then(|c| rows.iter().position(|e| &e.path == c));
        let next = match cur {
            None if delta >= 0 => 0,
            None => rows.len() - 1,
            Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        let path = rows[next].path.clone();
        if extend {
            self.select_range(rows, &path);
        } else {
            self.select_only(&path);
        }
    }

    /// Type-to-select (N05): first row whose name starts with `prefix`, case-insensitive.
    pub fn type_select(&mut self, rows: &[&Entry], prefix: &str) -> bool {
        let p = prefix.to_lowercase();
        if let Some(e) = rows.iter().find(|e| e.display_name().to_lowercase().starts_with(&p)) {
            let path = e.path.clone();
            self.select_only(&path);
            return true;
        }
        false
    }

    /// Selected entries in visible order.
    pub fn selected<'a>(&self, rows: &[&'a Entry]) -> Vec<&'a Entry> {
        rows.iter().copied().filter(|e| self.selection.contains(&e.path)).collect()
    }

    /// Drop selected paths that no longer exist in the listing.
    pub fn reconcile(&mut self, entries: &[Entry]) {
        let present: HashSet<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        self.selection.retain(|p| present.contains(p.as_path()));
        if self.cursor.as_ref().is_some_and(|c| !present.contains(c.as_path())) {
            self.cursor = None;
        }
    }
}

/// Filter and sort `entries` for display. Sorting is stable and selection is by path, so a
/// re-sort never loses the selection (N02/N05 acceptance).
pub fn visible_rows<'a>(entries: &'a [Entry], filters: Filters, filter: &str, sort: Sort) -> Vec<&'a Entry> {
    visible_indices(entries, filters, filter, sort).into_iter().map(|i| &entries[i]).collect()
}

/// Indices into `entries` in display order (the cacheable form of `visible_rows`).
pub fn visible_indices(entries: &[Entry], filters: Filters, filter: &str, sort: Sort) -> Vec<usize> {
    let needle = filter.to_lowercase();
    // Names computed once: building them per comparison dominated sorting 100k rows.
    let names: Vec<std::borrow::Cow<str>> = entries.iter().map(|e| e.name.to_string_lossy()).collect();
    let kinds: Vec<&'static str> = entries.iter().map(Entry::kind_label).collect();
    let mut rows: Vec<usize> = (0..entries.len())
        .filter(|&i| filters.allows(&entries[i]))
        .filter(|&i| needle.is_empty() || names[i].to_lowercase().contains(&needle))
        .collect();
    rows.sort_by(|&x, &y| {
        let (a, b) = (&entries[x], &entries[y]);
        if sort.folders_first && a.is_dir() != b.is_dir() {
            return b.is_dir().cmp(&a.is_dir());
        }
        if sort.group_kind && kinds[x] != kinds[y] {
            return kinds[x].cmp(kinds[y]);
        }
        let ord = match sort.key {
            SortKey::Name => natural_cmp(&names[x], &names[y]),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Kind => kinds[x].cmp(kinds[y]),
            SortKey::Modified => a.modified.cmp(&b.modified),
        }
        .then_with(|| natural_cmp(&names[x], &names[y]));
        if sort.ascending { ord } else { ord.reverse() }
    });
    rows
}

/// Case-insensitive natural order ("file2" < "file10"), like Finder. Allocation-free.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (na, nb) = (trim_zeros(&a[si..i]), trim_zeros(&b[sj..j]));
            let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if o != Equal {
                return o;
            }
        } else if a[i].is_ascii() && b[j].is_ascii() {
            let o = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
            if o != Equal {
                return o;
            }
            i += 1;
            j += 1;
        } else {
            // Non-ASCII tail: fall back to a Unicode case-insensitive comparison.
            let (ra, rb) = (String::from_utf8_lossy(&a[i..]), String::from_utf8_lossy(&b[j..]));
            return ra.to_lowercase().cmp(&rb.to_lowercase());
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let k = d.iter().position(|&c| c != b'0').unwrap_or(d.len());
    &d[k..]
}

#[derive(Clone, Debug)]
pub struct Pane {
    pub id: PaneId,
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl Pane {
    pub fn new(location: PathBuf) -> Self {
        Self { id: PaneId(next_id()), tabs: vec![Tab::new(location)], active: 0 }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    pub fn new_tab(&mut self) {
        let mut t = Tab::new(self.tab().location.clone());
        t.view = self.tab().view;
        t.sort = self.tab().sort;
        self.tabs.insert(self.active + 1, t);
        self.active += 1;
    }

    /// Close a tab, keeping the active tab's identity when an inactive one closes
    /// (v0.3 acceptance). The last tab cannot be closed.
    pub fn close_tab(&mut self, id: TabId) -> bool {
        if self.tabs.len() == 1 {
            return false;
        }
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else { return false };
        let active_id = self.tab().id;
        self.tabs.remove(ix);
        self.active = self
            .tabs
            .iter()
            .position(|t| t.id == active_id)
            .unwrap_or(ix.min(self.tabs.len() - 1));
        true
    }
}

pub fn display_name(p: &Path) -> String {
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => "Macintosh HD".into(),
    }
}

/// Resolve user path input: `~`, relative to `base`, `.`/`..` lexically.
pub fn resolve_input(input: &str, base: &Path, home: &Path) -> PathBuf {
    let input = input.trim();
    let raw = if input == "~" {
        home.to_path_buf()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.join(rest)
    } else if input.starts_with('/') {
        PathBuf::from(input)
    } else {
        base.join(input)
    };
    let mut out = PathBuf::from("/");
    for c in raw.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

/// Persisted workspace (N03 restore).
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Session {
    pub panes: Vec<SessionPane>,
    pub active_pane: usize,
    #[serde(default)]
    pub filters: Filters,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionPane {
    pub tabs: Vec<SessionTab>,
    pub active: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionTab {
    #[serde(with = "path_bytes")]
    pub location: PathBuf,
    pub view: ViewMode,
    pub sort: Sort,
}

#[cfg(test)]
mod tests {
    use super::*;
    use waypoint_core::fs::Kind;

    fn e(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            path: PathBuf::from("/t").join(name),
            name: name.into(),
            kind: if dir { Kind::Dir } else { Kind::File },
            size: (!dir).then_some(size),
            modified: None,
            visibility: if name.starts_with('.') { Visibility::Hidden } else { Visibility::Normal },
            error: None,
            tags: vec![],
        }
    }

    #[test]
    fn history_back_forward() {
        let mut t = Tab::new("/a".into());
        t.navigate("/a/b".into());
        t.navigate("/a/b/c".into());
        assert!(t.go_back());
        assert_eq!(t.location, Path::new("/a/b"));
        // Going back selects the child we came from.
        assert!(t.selection.contains(Path::new("/a/b/c")));
        assert!(t.go_forward());
        assert_eq!(t.location, Path::new("/a/b/c"));
        t.go_back();
        t.navigate("/x".into());
        assert!(t.forward.is_empty());
    }

    #[test]
    fn n05_selection_survives_sorting_and_range() {
        let es = vec![e("b", false, 3), e("a", false, 1), e("c", false, 2), e(".h", false, 0)];
        let mut t = Tab::new("/t".into());
        let rows = visible_rows(&es, Filters::default(), "", t.sort);
        assert_eq!(rows.len(), 3, "hidden filtered");
        t.select_only(&rows[0].path);
        t.select_range(&rows, &rows[2].path);
        assert_eq!(t.selection.len(), 3);
        t.toggle(Path::new("/t/b"));
        assert_eq!(t.selection.len(), 2);
        let sorted = visible_rows(&es, Filters::default(), "", Sort { key: SortKey::Size, ..Sort::default() });
        assert_eq!(sorted.iter().map(|e| e.display_name()).collect::<Vec<_>>(), ["a", "c", "b"]);
        assert_eq!(t.selected(&sorted).len(), 2);
    }

    #[test]
    fn n04_hidden_and_system_independent() {
        let mut sys = e("Library", true, 0);
        sys.visibility = Visibility::System;
        let es = vec![e(".git", true, 0), sys, e("src", true, 0)];
        let only = |f: Filters| visible_rows(&es, f, "", Sort::default()).len();
        assert_eq!(only(Filters { show_hidden: false, show_system: false }), 1);
        assert_eq!(only(Filters { show_hidden: true, show_system: false }), 2);
        assert_eq!(only(Filters { show_hidden: false, show_system: true }), 2);
        assert_eq!(only(Filters { show_hidden: true, show_system: true }), 3);
    }

    #[test]
    fn folders_first_and_natural_names() {
        let es = vec![e("file10", false, 0), e("file2", false, 0), e("zdir", true, 0)];
        let names: Vec<_> = visible_rows(&es, Filters::default(), "", Sort::default())
            .iter()
            .map(|e| e.display_name())
            .collect();
        assert_eq!(names, ["zdir", "file2", "file10"]);
        assert_eq!(natural_cmp("a007", "a7"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("B", "a"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("Äpfel", "äpfel"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn keyboard_and_type_select() {
        let es = vec![e("alpha", false, 0), e("beta", false, 0), e("gamma", false, 0)];
        let rows = visible_rows(&es, Filters::default(), "", Sort::default());
        let mut t = Tab::new("/t".into());
        t.move_cursor(&rows, 1, false);
        assert_eq!(t.cursor.as_deref(), Some(Path::new("/t/alpha")));
        t.move_cursor(&rows, 1, true);
        assert_eq!(t.selection.len(), 2);
        assert!(t.type_select(&rows, "GA"));
        assert_eq!(t.cursor.as_deref(), Some(Path::new("/t/gamma")));
    }

    #[test]
    fn closing_inactive_tab_keeps_active_identity() {
        let mut p = Pane::new("/a".into());
        p.new_tab();
        p.new_tab();
        let active = p.tab().id;
        let first = p.tabs[0].id;
        assert!(p.close_tab(first));
        assert_eq!(p.tab().id, active);
        let only = Pane::new("/x".into());
        let mut only2 = only.clone();
        assert!(!only2.close_tab(only.tabs[0].id));
    }

    #[test]
    fn resolves_user_paths() {
        let home = Path::new("/Users/me");
        assert_eq!(resolve_input("~/Docs", Path::new("/x"), home), Path::new("/Users/me/Docs"));
        assert_eq!(resolve_input("../y", Path::new("/x/z"), home), Path::new("/x/y"));
        assert_eq!(resolve_input("/a/./b/..", Path::new("/"), home), Path::new("/a"));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;
    use waypoint_core::fs::list_dir;
    use waypoint_core::pool::CancelToken;

    /// Q01 benchmark (run: cargo test --release -p waypoint bench_100k -- --ignored --nocapture).
    #[test]
    #[ignore]
    fn bench_100k_listing_and_sort() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..100_000 {
            std::fs::File::create(t.path().join(format!("file-{i:06}.txt"))).unwrap();
        }
        let start = Instant::now();
        let mut first = None;
        let mut all = Vec::new();
        list_dir(t.path(), &CancelToken::new(), |b| {
            first.get_or_insert_with(|| start.elapsed());
            all.extend(b);
            true
        })
        .unwrap();
        let total = start.elapsed();
        let s = Instant::now();
        let rows = visible_indices(&all, Filters::default(), "", Sort::default());
        let sort_name = s.elapsed();
        let s = Instant::now();
        let _ = visible_indices(&all, Filters::default(), "", Sort { key: SortKey::Size, ..Sort::default() });
        let sort_size = s.elapsed();
        assert_eq!(rows.len(), 100_000);
        println!("100k: first batch {first:?}, full listing {total:?}, sort by name {sort_name:?}, by size {sort_size:?}");
    }
}
