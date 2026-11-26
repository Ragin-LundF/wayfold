//! Path field completion (N01/N06): folder suggestions under the editable path, Tab
//! completes to the longest common prefix. Directory reads run on the worker pool.

use crate::app::state::resolve_input;
use gpui_kit::component::input::InputState;
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::IndexPath;
use gpui_kit::*;
use std::path::{Path, PathBuf};
use std::time::Duration;
use waypoint_core::pool;

pub struct PathCompletion {
    pub input: Entity<InputState>,
    pub base: PathBuf,
    pub home: PathBuf,
    pub show_hidden: bool,
    pub items: Vec<PathBuf>,
    selected: Option<IndexPath>,
}

impl PathCompletion {
    pub fn new(input: Entity<InputState>, home: PathBuf) -> Self {
        Self { input, base: home.clone(), home, show_hidden: false, items: vec![], selected: None }
    }
}

/// Split typed text into the folder to list and the name prefix to match.
pub fn split_query(query: &str, base: &Path, home: &Path) -> (PathBuf, String) {
    if query.is_empty() || query.ends_with('/') {
        return (resolve_input(if query.is_empty() { "." } else { query }, base, home), String::new());
    }
    match query.rfind('/') {
        Some(i) => (resolve_input(if i == 0 { "/" } else { &query[..i] }, base, home), query[i + 1..].to_string()),
        None if query == "~" => (home.to_path_buf(), String::new()),
        None => (base.to_path_buf(), query.to_string()),
    }
}

/// Matching subfolders, at most 50, sorted. Blocking.
pub fn matches(dir: &Path, prefix: &str, show_hidden: bool) -> Vec<PathBuf> {
    let p = prefix.to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.starts_with(&p) && (show_hidden || !n.starts_with('.') || p.starts_with('.'))
        })
        .map(|e| e.path())
        .collect();
    v.sort();
    v.truncate(50);
    v
}

/// Longest common prefix of the names, case-insensitive like the volume.
pub fn common_prefix(paths: &[PathBuf]) -> Option<String> {
    let names: Vec<String> = paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
    let first = names.first()?;
    let mut len = first.chars().count();
    for n in &names[1..] {
        len = len.min(first.chars().zip(n.chars()).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count());
    }
    Some(first.chars().take(len).collect())
}

impl ListDelegate for PathCompletion {
    type Item = ListItem;

    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut Context<ListState<Self>>) -> Task<()> {
        let (dir, prefix) = split_query(query, &self.base, &self.home);
        let hidden = self.show_hidden;
        cx.spawn_in(window, async move |list, cx| {
            cx.background_executor().timer(Duration::from_millis(80)).await;
            let items = pool::for_path(&dir).run(move || matches(&dir, &prefix, hidden)).await.unwrap_or_default();
            let _ = list.update(cx, |list, cx| {
                list.delegate_mut().items = items;
                cx.notify();
            });
        })
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.items.len()
    }

    fn render_item(&mut self, ix: IndexPath, _: &mut Window, _: &mut Context<ListState<Self>>) -> Option<ListItem> {
        let p = self.items.get(ix.row)?;
        Some(ListItem::new(ix.row).child(SharedString::from(p.display().to_string())))
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, _: &mut Context<ListState<Self>>) {
        self.selected = ix;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        if let Some(p) = self.selected.and_then(|ix| self.items.get(ix.row)) {
            let text = format!("{}/", p.display());
            self.input.update(cx, |i, cx| i.set_value(text, window, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn n06_split_and_complete() {
        let home = Path::new("/Users/me");
        assert_eq!(split_query("~/Doc", Path::new("/x"), home), (PathBuf::from("/Users/me"), "Doc".into()));
        assert_eq!(split_query("/usr/", Path::new("/x"), home), (PathBuf::from("/usr"), String::new()));
        assert_eq!(split_query("sub", Path::new("/x"), home), (PathBuf::from("/x"), "sub".into()));
        let t = tempfile::tempdir().unwrap();
        for d in ["Projects", "Pictures", "Public", ".hidden"] {
            std::fs::create_dir(t.path().join(d)).unwrap();
        }
        std::fs::write(t.path().join("Pfile"), b"").unwrap();
        let m = matches(t.path(), "p", false);
        assert_eq!(m.len(), 3, "folders only, hidden excluded");
        assert_eq!(common_prefix(&m).as_deref(), Some("P"));
        assert_eq!(common_prefix(&matches(t.path(), "pr", false)).as_deref(), Some("Projects"));
    }
}
