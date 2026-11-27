//! The window's root view: owns workspace state, schedules worker jobs and handles actions.

use crate::app::actions::*;
use crate::app::listing::ListingCache;
use crate::app::state::*;
use crate::terminal_view::{TerminalPanel, TerminalPanelEvent};
use crate::ui::theme;
use gpui_kit::Focusable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wayfold_core::fs::{self as wfs, Entry, Mount};
use wayfold_core::pool::{self, CancelToken};
use wayfold_core::storage::{self, paths_bytes};

/// Cancels the worker when the owning foreground task is dropped (navigation away).
pub struct CancelGuard(pub CancelToken);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Serialize, Deserialize)]
pub struct Prefs {
    #[serde(with = "paths_bytes", default)]
    pub favorites: Vec<PathBuf>,
    #[serde(with = "paths_bytes", default)]
    pub recents: Vec<PathBuf>,
    #[serde(default)]
    pub light_theme: bool,
    /// T02 opt-in: shell integration scripts enable terminal follow modes.
    #[serde(default)]
    pub shell_integration: bool,
    #[serde(default)]
    pub folder_views: Vec<FolderPref>,
    /// N04: show file name extensions (default on) and which optional columns are hidden.
    #[serde(default = "yes")]
    pub show_extensions: bool,
    #[serde(default)]
    pub hidden_columns: Vec<String>,
    /// Global: folders sort before files in every folder; overrides `Sort::folders_first` of tabs and folder prefs.
    #[serde(default = "yes")]
    pub folders_first: bool,
}

fn yes() -> bool {
    true
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            favorites: vec![],
            recents: vec![],
            light_theme: false,
            shell_integration: false,
            folder_views: vec![],
            show_extensions: true,
            hidden_columns: vec![],
            folders_first: true,
        }
    }
}

/// Per-folder view preference (N02).
#[derive(Serialize, Deserialize, Clone)]
pub struct FolderPref {
    #[serde(with = "wayfold_core::storage::path_bytes")]
    pub path: PathBuf,
    pub view: ViewMode,
    pub sort: Sort,
}

// Listing versions come from the `ListingCache` clock, so they are unique across folders.
type RowKey = (u64, u64, String, Sort, Filters);
/// Per-tab sorted/filtered row order, tagged with the key it was computed for.
type RowCache = HashMap<TabId, (RowKey, std::rc::Rc<Vec<usize>>)>;

const MAX_RECENTS: usize = 12;
const MAX_FOLDER_PREFS: usize = 500;
/// Listing cache bound (entries across all cached folders).
const CACHE_ENTRIES: usize = 300_000;

pub struct Workspace {
    pub panes: Vec<Pane>,
    pub active_pane: usize,
    pub filters: Filters,
    pub cache: ListingCache,
    loads: HashMap<PathBuf, Task<()>>,
    pub pane_focus: Vec<FocusHandle>,
    pub scroll: HashMap<TabId, UniformListScrollHandle>,
    pub path_input: Entity<InputState>,
    pub path_list: Entity<gpui_kit::component::list::ListState<crate::ui::path_complete::PathCompletion>>,
    pub path_editing: bool,
    pub prefs: Prefs,
    pub volumes: Vec<Mount>,
    pub tag_filter: Option<String>,
    /// Standard tags as (English key, dot color, Finder's localized tag), resolved once at start (D-3).
    pub standard_tags: Vec<(&'static str, u32, wfs::tags::Tag)>,
    pub preview_open: bool,
    pub terminal_open: bool,
    pub queue_open: bool,
    pub terminal: Option<Entity<TerminalPanel>>,
    type_buffer: (String, Instant),
    pub status: Option<SharedString>,
    watcher: Option<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    pub home: PathBuf,
    /// List view column widths (Name, Size, Kind, Modified); adjustable by dragging (N02).
    pub columns: [f32; 4],
    pub clipboard: Option<crate::ui::clipboard::FileClipboard>,
    pub transfers: Vec<crate::ui::transfers::TransferView>,
    pub transfer_tasks: Vec<Task<()>>,
    pub undo_stack: Vec<wayfold_core::ops::UndoAction>,
    pub folder_sizes: HashMap<PathBuf, crate::ui::fileops::SizeState>,
    pub size_tasks: Vec<Task<()>>,
    pub preview: crate::ui::preview::PreviewState,
    pub search: crate::ui::search::SearchState,
    /// Sorted/filtered row order per tab; recomputed only when its key changes (100k rows).
    row_cache: std::cell::RefCell<RowCache>,
    /// Tiles per row from the last icons layout; all panes share one width.
    pub(crate) icons_per_row: std::cell::Cell<usize>,
    _tasks: Vec<Task<()>>,
    pub(crate) _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
        let prefs: Prefs = storage::load("prefs");
        let session: Session = storage::load("session");
        let mut panes: Vec<Pane> = session
            .panes
            .iter()
            .map(|sp| {
                let mut p = Pane::new(home.clone());
                p.tabs = sp
                    .tabs
                    .iter()
                    .map(|st| {
                        let loc = if st.location.is_dir() { st.location.clone() } else { home.clone() };
                        let mut t = Tab::new(loc);
                        t.view = st.view;
                        t.sort = st.sort;
                        t
                    })
                    .collect();
                if p.tabs.is_empty() {
                    p.tabs.push(Tab::new(home.clone()));
                }
                p.active = sp.active.min(p.tabs.len() - 1);
                p
            })
            .take(2)
            .collect();
        if panes.is_empty() {
            panes.push(Pane::new(home.clone()));
        }
        let active_pane = session.active_pane.min(panes.len() - 1);
        let favorites = if prefs.favorites.is_empty() {
            ["", "Desktop", "Documents", "Downloads"].iter().map(|s| home.join(s)).filter(|p| p.is_dir()).collect()
        } else {
            prefs.favorites.clone()
        };

        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder("Go to folder (~, /, relative)"));
        let path_list = {
            let d = crate::ui::path_complete::PathCompletion::new(path_input.clone(), home.clone());
            cx.new(|cx| gpui_kit::component::list::ListState::new(d, window, cx))
        };
        let mut subs = vec![cx.subscribe_in(&path_input, window, Self::on_path_input)];

        let watch_task = Self::start_watcher(cx);
        let mut this = Self {
            pane_focus: panes.iter().map(|_| cx.focus_handle()).collect(),
            panes,
            active_pane,
            filters: session.filters,
            cache: ListingCache::new(CACHE_ENTRIES),
            loads: HashMap::new(),
            scroll: HashMap::new(),
            path_input,
            path_list,
            path_editing: false,
            prefs: Prefs { favorites, ..prefs },
            volumes: wfs::mounts(),
            tag_filter: None,
            standard_tags: theme::TAGS.iter().filter_map(|&(name, rgb)| Some((name, rgb, wfs::tags::standard(name)?))).collect(),
            preview_open: false,
            terminal_open: false,
            queue_open: false,
            terminal: None,
            type_buffer: (String::new(), Instant::now()),
            status: None,
            watcher: None,
            watched: HashSet::new(),
            home,
            columns: [360., 90., 120., 160.],
            clipboard: None,
            transfers: vec![],
            transfer_tasks: vec![],
            undo_stack: vec![],
            folder_sizes: HashMap::new(),
            size_tasks: vec![],
            preview: Default::default(),
            search: Default::default(),
            row_cache: Default::default(),
            icons_per_row: std::cell::Cell::new(1),
            _tasks: vec![],
            _subscriptions: vec![],
        };
        if let Some((w, t)) = watch_task {
            this.watcher = Some(w);
            this._tasks.push(t);
        }
        this._tasks.push(Self::poll_volumes(cx));
        subs.push(cx.observe_window_activation(window, |_, window, cx| {
            // External clipboard and mounted volumes may change while the app is inactive.
            if window.is_window_active() {
                cx.notify();
            }
        }));
        this._subscriptions = subs;
        this.refresh_visible(cx);
        let focus = this.pane_focus[this.active_pane].clone();
        window.focus(&focus, cx);
        this
    }

    // ----- accessors -------------------------------------------------------------------

    pub fn pane(&self) -> &Pane {
        &self.panes[self.active_pane]
    }

    pub fn tab(&self) -> &Tab {
        self.pane().tab()
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        self.panes[self.active_pane].tab_mut()
    }

    /// Visible rows of a tab, applying hidden/system/filter/tag/sort.
    pub fn rows_for<'a>(&'a self, tab: &Tab) -> Vec<&'a Entry> {
        if self.search.active_for(tab.id) {
            return self.search.results.iter().collect();
        }
        let Some(l) = self.cache.get(&tab.location) else { return vec![] };
        // The effective sort is part of the key, so toggling the global pref invalidates cached rows.
        let sort = self.effective_sort(tab.sort);
        let key = (l.version, l.generation, tab.filter.clone(), sort, self.filters);
        let mut cache = self.row_cache.borrow_mut();
        let hit = cache.get(&tab.id).filter(|(k, _)| *k == key).map(|(_, v)| v.clone());
        let ix = hit.unwrap_or_else(|| {
            let v = std::rc::Rc::new(visible_indices(&l.entries, self.filters, &tab.filter, sort));
            cache.insert(tab.id, (key, v.clone()));
            v
        });
        ix.iter().filter_map(|&i| l.entries.get(i)).collect()
    }

    /// A tab's sort with the global "Folders First" pref applied.
    pub fn effective_sort(&self, sort: Sort) -> Sort {
        Sort { folders_first: self.prefs.folders_first, ..sort }
    }

    pub fn rows(&self) -> Vec<&Entry> {
        self.rows_for(self.tab())
    }

    pub fn selected_entries(&self) -> Vec<Entry> {
        let rows = self.rows();
        self.tab().selected(&rows).into_iter().cloned().collect()
    }

    /// The single focused entry for preview and info.
    pub fn focused_entry(&self) -> Option<Entry> {
        let rows = self.rows();
        let t = self.tab();
        t.cursor
            .as_ref()
            .and_then(|c| rows.iter().find(|e| &e.path == c && t.selection.contains(c)))
            .or_else(|| rows.iter().find(|e| t.selection.contains(&e.path)))
            .map(|e| (*e).clone())
    }

    // ----- loading ---------------------------------------------------------------------

    /// Folders currently on screen: active tab of each pane plus Miller column ancestors.
    fn visible_dirs(&self) -> HashSet<PathBuf> {
        let mut s = HashSet::new();
        for p in &self.panes {
            let t = p.tab();
            s.insert(t.location.clone());
            if t.view == ViewMode::Columns {
                s.extend(crate::ui::browser::column_dirs(&t.location));
                if let Some(c) = &t.cursor {
                    s.insert(c.clone());
                }
            }
        }
        s
    }

    /// Load any visible folder not cached; refresh watches; evict cache.
    pub fn refresh_visible(&mut self, cx: &mut Context<Self>) {
        let dirs = self.visible_dirs();
        for d in &dirs {
            if self.cache.get(d).is_none() {
                self.load(d.clone(), cx);
            } else {
                self.cache.touch(d);
            }
        }
        self.cache.evict(&dirs);
        self.update_watches(&dirs);
        self.loads.retain(|p, _| dirs.contains(p));
    }

    /// Start a progressive listing on the worker pool. Replacing an older load for the same
    /// folder drops its task, which cancels the worker.
    pub fn load(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.is_absolute() {
            return;
        }
        let generation = self.cache.begin(&dir);
        let (tx, rx) = async_channel::bounded::<Vec<Entry>>(16);
        let (done_tx, done_rx) = async_channel::bounded::<Option<String>>(1);
        let cancel = CancelToken::new();
        let (worker_dir, worker_cancel) = (dir.clone(), cancel.clone());
        pool::for_path(&dir).execute(move || {
            let r = wfs::list_dir(&worker_dir, &worker_cancel, |b| tx.send_blocking(b).is_ok());
            let _ = done_tx.send_blocking(r.err().map(|e| e.to_string()));
        });
        let path = dir.clone();
        let task = cx.spawn(async move |this, cx| {
            let _guard = CancelGuard(cancel);
            while let Ok(mut batch) = rx.recv().await {
                while let Ok(more) = rx.try_recv() {
                    batch.extend(more);
                }
                let alive = this.update(cx, |w, cx| {
                    w.cache.push_batch(&path, generation, batch);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            let error = done_rx.recv().await.ok().flatten();
            this.update(cx, |w, cx| {
                if w.cache.finish(&path, generation, error)
                    && let Some(l) = w.cache.get(&path)
                {
                    let entries = l.entries.clone();
                    for p in &mut w.panes {
                        for t in &mut p.tabs {
                            if t.location == path {
                                t.reconcile(&entries);
                            }
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.loads.insert(dir, task);
    }

    fn update_watches(&mut self, dirs: &HashSet<PathBuf>) {
        use notify::Watcher as _;
        let Some(w) = self.watcher.as_mut() else { return };
        for gone in self.watched.difference(dirs).cloned().collect::<Vec<_>>() {
            let _ = w.unwatch(&gone);
            self.watched.remove(&gone);
        }
        for d in dirs {
            if !self.watched.contains(d) && w.watch(d, notify::RecursiveMode::NonRecursive).is_ok() {
                self.watched.insert(d.clone());
            }
        }
    }

    /// FSEvents watcher; events are coalesced for 150 ms and reload affected visible folders.
    fn start_watcher(cx: &mut Context<Self>) -> Option<(notify::RecommendedWatcher, Task<()>)> {
        let (tx, rx) = async_channel::unbounded::<notify::Result<notify::Event>>();
        let watcher = notify::recommended_watcher(move |ev| {
            let _ = tx.send_blocking(ev);
        })
        .ok()?;
        let task = cx.spawn(async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let mut events = vec![first];
                while let Ok(e) = rx.try_recv() {
                    events.push(e);
                }
                let mut dirs = HashSet::new();
                let mut rescan = false;
                for e in events {
                    match e {
                        Ok(e) => {
                            rescan |= e.need_rescan();
                            for p in e.paths {
                                if let Some(parent) = p.parent() {
                                    dirs.insert(parent.to_path_buf());
                                }
                                dirs.insert(p);
                            }
                        }
                        Err(_) => rescan = true,
                    }
                }
                let r = this.update(cx, |w, cx| {
                    let visible = w.visible_dirs();
                    for d in visible {
                        if rescan || dirs.contains(&d) {
                            w.load(d, cx);
                        }
                    }
                });
                if r.is_err() {
                    return;
                }
            }
        });
        Some((watcher, task))
    }

    /// Mount table changes (new volume, unmounted share) refresh the sidebar.
    fn poll_volumes(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let Some(v) = pool::local().run(wfs::refresh_mounts).await else { return };
                let r = this.update(cx, |w, cx| {
                    if w.volumes != v {
                        w.volumes = v;
                        cx.notify();
                    }
                });
                if r.is_err() {
                    return;
                }
            }
        })
    }

    // ----- navigation ------------------------------------------------------------------

    pub fn navigate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.tag_filter = None;
        if self.search.active_for(self.tab().id) {
            self.search.clear();
        }
        let pref = self.prefs.folder_views.iter().find(|f| f.path == path).cloned();
        let t = self.tab_mut();
        if t.navigate(path.clone()) {
            if let Some(pref) = pref {
                t.view = pref.view;
                t.sort = pref.sort;
            }
            self.push_recent(path);
        }
        self.after_location_change(cx);
    }

    fn push_recent(&mut self, path: PathBuf) {
        self.prefs.recents.retain(|p| p != &path);
        self.prefs.recents.insert(0, path);
        self.prefs.recents.truncate(MAX_RECENTS);
    }

    pub fn after_location_change(&mut self, cx: &mut Context<Self>) {
        self.path_editing = false;
        self.refresh_visible(cx);
        let loc = self.tab().location.clone();
        if let Some(term) = &self.terminal {
            term.update(cx, |t, cx| t.browser_location_changed(loc, cx));
        }
        self.save_session();
        cx.notify();
    }

    /// Open a folder entry or a file with the default application (F05).
    pub fn open_entry(&mut self, e: &Entry, cx: &mut Context<Self>) {
        if e.is_dir() {
            self.navigate(e.path.clone(), cx);
        } else {
            let path = e.path.clone();
            pool::local().execute(move || {
                if let Err(err) = wayfold_core::platform::open_default(&path) {
                    tracing::warn!("open failed: {err}");
                }
            });
        }
    }

    pub fn activate_pane(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.panes.len() {
            self.active_pane = ix;
            let f = self.pane_focus[ix].clone();
            window.focus(&f, cx);
            let loc = self.tab().location.clone();
            if let Some(term) = &self.terminal {
                term.update(cx, |t, cx| {
                    t.set_binding_label(format!("Pane {}", ix + 1).into(), cx);
                    t.browser_location_changed(loc, cx);
                });
            }
            cx.notify();
        }
    }

    pub fn focus_browser(&self, window: &mut Window, cx: &mut App) {
        let f = self.pane_focus[self.active_pane].clone();
        window.focus(&f, cx);
    }

    fn on_path_input(&mut self, input: &Entity<InputState>, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            InputEvent::PressEnter { .. } => {
                let text = input.read(cx).value().to_string();
                let target = resolve_input(&text, &self.tab().location, &self.home);
                if target.is_dir() {
                    self.navigate(target, cx);
                    self.focus_browser(window, cx);
                } else if target.exists() {
                    // A file path reveals it in its folder.
                    if let Some(parent) = target.parent() {
                        self.navigate(parent.to_path_buf(), cx);
                        self.tab_mut().select_only(&target);
                    }
                    self.focus_browser(window, cx);
                } else {
                    self.status = Some(format!("No such folder: {}", target.display()).into());
                    cx.notify();
                }
            }
            InputEvent::Blur => {
                self.path_editing = false;
                cx.notify();
            }
            _ => {}
        }
    }

    pub fn start_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loc = self.tab().location.display().to_string();
        let (base, hidden) = (self.tab().location.clone(), self.filters.show_hidden);
        self.path_list.update(cx, |l, _| {
            let d = l.delegate_mut();
            d.base = base;
            d.show_hidden = hidden;
        });
        self.path_editing = true;
        self.path_input.update(cx, |s, cx| {
            s.set_value(loc, window, cx);
            s.focus(window, cx);
            // Typing replaces the current path (like Finder's Go to Folder).
            s.select_all(window, cx);
        });
        cx.notify();
    }

    /// Tab in the path field: complete to the longest common prefix of matching folders.
    pub fn complete_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::ui::path_complete::{common_prefix, matches, split_query};
        let text = self.path_input.read(cx).value().to_string();
        let (dir, prefix) = split_query(&text, &self.tab().location, &self.home);
        let hidden = self.filters.show_hidden;
        let input = self.path_input.clone();
        let fut = pool::for_path(&dir).run({
            let dir = dir.clone();
            move || matches(&dir, &prefix, hidden)
        });
        cx.spawn_in(window, async move |_, cx| {
            let m = fut.await.unwrap_or_default();
            let Some(common) = common_prefix(&m) else { return };
            let mut out = dir.join(common).display().to_string();
            if m.len() == 1 {
                out.push('/');
            }
            let _ = input.update_in(cx, |i, window, cx| i.set_value(out, window, cx));
        })
        .detach();
    }

    // ----- persistence -----------------------------------------------------------------

    pub fn save_session(&self) {
        let session = Session {
            panes: self
                .panes
                .iter()
                .map(|p| SessionPane {
                    tabs: p.tabs.iter().map(|t| SessionTab { location: t.location.clone(), view: t.view, sort: t.sort }).collect(),
                    active: p.active,
                })
                .collect(),
            active_pane: self.active_pane,
            filters: self.filters,
        };
        // Tiny files; written off the UI thread. The dir is resolved now, not when the job runs.
        let prefs = serde_json::to_value(&self.prefs).ok();
        let dir = storage::data_dir();
        pool::local().execute(move || {
            if let Err(e) = storage::save_in(&dir, "session", &session) {
                tracing::warn!("saving session failed: {e}");
            }
            if let Some(p) = prefs
                && let Err(e) = storage::save_in(&dir, "prefs", &p)
            {
                tracing::warn!("saving prefs failed: {e}");
            }
        });
    }

    fn remember_folder_view(&mut self) {
        let t = self.tab();
        let pref = FolderPref { path: t.location.clone(), view: t.view, sort: t.sort };
        self.prefs.folder_views.retain(|f| f.path != pref.path);
        self.prefs.folder_views.insert(0, pref);
        self.prefs.folder_views.truncate(MAX_FOLDER_PREFS);
    }

    pub fn notify_status(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(msg.into());
        cx.notify();
    }

    // ----- action handlers -------------------------------------------------------------

    pub fn register_actions<E: InteractiveElement>(&self, el: E, cx: &mut Context<Self>) -> E {
        el.on_action(cx.listener(|w, _: &GoBack, _, cx| {
            if w.tab_mut().go_back() {
                w.after_location_change(cx);
            }
        }))
        .on_action(cx.listener(|w, _: &GoForward, _, cx| {
            if w.tab_mut().go_forward() {
                w.after_location_change(cx);
            }
        }))
        .on_action(cx.listener(|w, _: &GoUp, _, cx| {
            if let Some(p) = w.tab().location.parent().map(Path::to_path_buf) {
                w.navigate(p, cx);
            }
        }))
        .on_action(cx.listener(|w, _: &GoHome, _, cx| w.navigate(w.home.clone(), cx)))
        .on_action(cx.listener(|w, _: &GoToPath, window, cx| w.start_path_edit(window, cx)))
        .on_action(cx.listener(|w, _: &FocusPath, window, cx| w.start_path_edit(window, cx)))
        .on_action(cx.listener(|w, _: &Refresh, _, cx| {
            let loc = w.tab().location.clone();
            w.load(loc, cx);
        }))
        .on_action(cx.listener(|w, _: &NewTab, _, cx| {
            w.panes[w.active_pane].new_tab();
            w.after_location_change(cx);
        }))
        .on_action(cx.listener(|w, _: &CloseTab, _, cx| {
            let id = w.tab().id;
            if w.panes[w.active_pane].close_tab(id) {
                w.after_location_change(cx);
            } else if w.panes.len() > 1 {
                // Closing the last tab of the second pane closes the split.
                w.close_pane(w.active_pane, cx);
            }
        }))
        .on_action(cx.listener(|w, _: &NextTab, _, cx| {
            let p = &mut w.panes[w.active_pane];
            p.active = (p.active + 1) % p.tabs.len();
            w.after_location_change(cx);
        }))
        .on_action(cx.listener(|w, _: &PrevTab, _, cx| {
            let p = &mut w.panes[w.active_pane];
            p.active = (p.active + p.tabs.len() - 1) % p.tabs.len();
            w.after_location_change(cx);
        }))
        .on_action(cx.listener(|w, _: &ToggleSplit, window, cx| w.toggle_split(window, cx)))
        .on_action(cx.listener(|w, _: &FocusOtherPane, window, cx| {
            if w.panes.len() > 1 {
                w.activate_pane(1 - w.active_pane, window, cx);
            }
        }))
        .on_action(cx.listener(|w, _: &ViewIcons, _, cx| w.set_view(ViewMode::Icons, cx)))
        .on_action(cx.listener(|w, _: &ViewList, _, cx| w.set_view(ViewMode::List, cx)))
        .on_action(cx.listener(|w, _: &ViewColumns, _, cx| w.set_view(ViewMode::Columns, cx)))
        .on_action(cx.listener(|w, _: &SortByName, _, cx| w.set_sort(SortKey::Name, cx)))
        .on_action(cx.listener(|w, _: &SortBySize, _, cx| w.set_sort(SortKey::Size, cx)))
        .on_action(cx.listener(|w, _: &SortByKind, _, cx| w.set_sort(SortKey::Kind, cx)))
        .on_action(cx.listener(|w, _: &SortByModified, _, cx| w.set_sort(SortKey::Modified, cx)))
        .on_action(cx.listener(|w, _: &ToggleHidden, _, cx| {
            w.filters.show_hidden = !w.filters.show_hidden;
            w.save_session();
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &ToggleSystem, _, cx| {
            w.filters.show_system = !w.filters.show_system;
            w.save_session();
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &TogglePreview, _, cx| {
            w.preview_open = !w.preview_open;
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &ClosePanels, _, cx| {
            if w.preview_open {
                w.preview_open = false;
            } else if !w.tab().filter.is_empty() || w.tag_filter.is_some() {
                w.tab_mut().filter.clear();
                w.tag_filter = None;
            }
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &ToggleTerminal, window, cx| w.toggle_terminal(window, cx)))
        .on_action(cx.listener(|w, _: &ToggleQueue, _, cx| {
            w.queue_open = !w.queue_open;
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &SelectAll, _, cx| {
            let t = w.tab().clone();
            let rows = w.rows_for(&t);
            let mut t2 = t.clone();
            t2.select_all(&rows);
            *w.tab_mut() = t2;
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &SelectNext, _, cx| w.move_cursor(1, false, cx)))
        .on_action(cx.listener(|w, _: &SelectPrev, _, cx| w.move_cursor(-1, false, cx)))
        .on_action(cx.listener(|w, _: &ExtendNext, _, cx| w.move_cursor(1, true, cx)))
        .on_action(cx.listener(|w, _: &ExtendPrev, _, cx| w.move_cursor(-1, true, cx)))
        .on_action(cx.listener(|w, _: &SelectFirst, _, cx| w.move_to_edge(false, false, cx)))
        .on_action(cx.listener(|w, _: &SelectLast, _, cx| w.move_to_edge(true, false, cx)))
        .on_action(cx.listener(|w, _: &ExtendFirst, _, cx| w.move_to_edge(false, true, cx)))
        .on_action(cx.listener(|w, _: &ExtendLast, _, cx| w.move_to_edge(true, true, cx)))
        .on_action(cx.listener(|w, _: &SelectPageDown, _, cx| w.move_page(true, false, cx)))
        .on_action(cx.listener(|w, _: &SelectPageUp, _, cx| w.move_page(false, false, cx)))
        .on_action(cx.listener(|w, _: &ExtendPageDown, _, cx| w.move_page(true, true, cx)))
        .on_action(cx.listener(|w, _: &ExtendPageUp, _, cx| w.move_page(false, true, cx)))
        .on_action(cx.listener(|w, _: &SelectLeft, _, cx| w.horizontal_move(-1, cx)))
        .on_action(cx.listener(|w, _: &SelectRight, _, cx| w.horizontal_move(1, cx)))
        .on_action(cx.listener(|w, _: &OpenItems, _, cx| {
            for e in w.selected_entries() {
                w.open_entry(&e, cx);
            }
        }))
        .on_action(cx.listener(|w, _: &OpenCommandPalette, window, cx| w.open_palette(window, cx)))
        .on_action(cx.listener(|w, _: &AddFavorite, _, cx| w.add_favorite(cx)))
        .on_action(cx.listener(|w, _: &CopyPath, _, cx| {
            let text = w.selected_entries().iter().map(|e| e.path.display().to_string()).collect::<Vec<_>>().join("\n");
            let text = if text.is_empty() { w.tab().location.display().to_string() } else { text };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            w.notify_status("Path copied", cx);
        }))
        .on_action(cx.listener(|w, _: &ToggleTheme, _, cx| {
            w.prefs.light_theme = !w.prefs.light_theme;
            theme::apply(!w.prefs.light_theme, cx);
            w.save_session();
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &Search, window, cx| w.open_search(window, cx)))
        .on_action(cx.listener(|w, _: &ToggleGroupByKind, _, cx| {
            let s = &mut w.tab_mut().sort;
            s.group_kind = !s.group_kind;
            w.remember_folder_view();
            w.save_session();
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &ToggleExtensions, _, cx| {
            w.prefs.show_extensions = !w.prefs.show_extensions;
            w.save_session();
            cx.notify();
        }))
        .on_action(cx.listener(|w, _: &ToggleFoldersFirst, _, cx| {
            w.prefs.folders_first = !w.prefs.folders_first;
            w.save_session();
            w.scroll_cursor_into_view(cx);
        }))
        .on_action(cx.listener(|w, _: &ToggleSizeColumn, _, cx| w.toggle_column("Size", cx)))
        .on_action(cx.listener(|w, _: &ToggleKindColumn, _, cx| w.toggle_column("Kind", cx)))
        .on_action(cx.listener(|w, _: &ToggleModifiedColumn, _, cx| w.toggle_column("Modified", cx)))
        .on_action(cx.listener(|w, _: &ToggleShellIntegration, window, cx| {
            w.prefs.shell_integration = !w.prefs.shell_integration;
            let on = w.prefs.shell_integration;
            if let Some(t) = &w.terminal {
                t.update(cx, |t, cx| t.set_shell_integration(on, cx));
            }
            w.save_session();
            let msg = if on { "Shell integration on: new terminal sessions can follow the browser" } else { "Shell integration off" };
            window.push_notification(msg, cx);
        }))
    }

    fn close_pane(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.panes.remove(ix);
        self.pane_focus.remove(ix);
        self.active_pane = 0;
        self.after_location_change(cx);
    }

    pub fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() == 1 {
            let loc = self.tab().location.clone();
            let mut p = Pane::new(loc);
            p.tab_mut().view = self.tab().view;
            self.panes.push(p);
            self.pane_focus.push(cx.focus_handle());
            self.after_location_change(cx);
            self.activate_pane(1, window, cx);
        } else {
            self.close_pane(1, cx);
            self.focus_browser(window, cx);
        }
    }

    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_open = !self.terminal_open;
        if self.terminal_open && self.terminal.is_none() {
            let loc = self.tab().location.clone();
            let panel = cx.new(|cx| TerminalPanel::new(loc, window, cx));
            self._subscriptions.push(cx.subscribe_in(&panel, window, |w, _, ev: &TerminalPanelEvent, window, cx| match ev {
                TerminalPanelEvent::Hide => {
                    w.terminal_open = false;
                    w.focus_browser(window, cx);
                    cx.notify();
                }
                TerminalPanelEvent::CwdChanged(p) => {
                    // No is_dir here: it can block on a stalled mount; the async load reports errors.
                    if *p != w.tab().location {
                        // Two-way mode: update the bound pane; the panel suppresses the echo.
                        let p = p.clone();
                        w.tag_filter = None;
                        if w.tab_mut().navigate(p.clone()) {
                            w.push_recent(p);
                        }
                        w.path_editing = false;
                        w.refresh_visible(cx);
                        w.save_session();
                        cx.notify();
                    }
                }
            }));
            let integration = self.prefs.shell_integration;
            panel.update(cx, |t, cx| {
                t.set_shell_integration(integration, cx);
                t.set_binding_label(format!("Pane {}", self.active_pane + 1).into(), cx);
            });
            self.terminal = Some(panel);
        }
        if !self.terminal_open {
            self.focus_browser(window, cx);
        } else if let Some(t) = &self.terminal {
            let f = t.focus_handle(cx);
            window.focus(&f, cx);
        }
        cx.notify();
    }

    pub fn set_view(&mut self, v: ViewMode, cx: &mut Context<Self>) {
        self.tab_mut().view = v;
        self.remember_folder_view();
        self.refresh_visible(cx);
        self.save_session();
        cx.notify();
    }

    pub fn column_visible(&self, name: &str) -> bool {
        !self.prefs.hidden_columns.iter().any(|c| c == name)
    }

    fn toggle_column(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.column_visible(name) {
            self.prefs.hidden_columns.push(name.to_string());
        } else {
            self.prefs.hidden_columns.retain(|c| c != name);
        }
        self.save_session();
        cx.notify();
    }

    pub fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let s = &mut self.tab_mut().sort;
        if s.key == key {
            s.ascending = !s.ascending;
        } else {
            s.key = key;
            s.ascending = key == SortKey::Name || key == SortKey::Kind;
        }
        self.remember_folder_view();
        self.save_session();
        self.scroll_cursor_into_view(cx);
    }

    /// After a re-sort the cursor keeps its entry, but its row may now be off screen.
    fn scroll_cursor_into_view(&mut self, cx: &mut Context<Self>) {
        self.move_cursor_with(cx, |_, _| {});
    }

    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.move_cursor_with(cx, |t, rows| t.move_cursor(rows, delta, extend));
    }

    fn move_to_edge(&mut self, last: bool, extend: bool, cx: &mut Context<Self>) {
        self.move_cursor_with(cx, |t, rows| t.move_to_edge(rows, last, extend));
    }

    fn move_page(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let page = self.visible_page_rows();
        self.move_cursor_with(cx, |t, rows| t.move_page(rows, page, down, extend));
    }

    /// Entries that fit the active tab's viewport, taken from its last layout: list rows, or whole
    /// tile rows in icons view so the cursor keeps its column.
    fn visible_page_rows(&self) -> usize {
        let viewport = self.scroll.get(&self.tab().id).and_then(|h| h.0.borrow().last_item_size);
        // shortcut: columns reuse the last list/icon viewport (or 10 before any layout), which may be
        // stale, and column lists don't scroll the cursor into view; group-by-kind headers count as
        // rows, so a grouped page is a little short. Track column sizes if that ever matters.
        let Some(height) = viewport.map(|s| s.item.height.to_f64() as f32) else { return 10 };
        if self.tab().view == ViewMode::Icons {
            let per_row = self.icons_per_row.get();
            return ((height / crate::ui::browser::ICON_ROW_H) as usize).max(1) * per_row;
        }
        (height / theme::ROW_H) as usize
    }

    /// Applies a cursor move to the active tab, then keeps the cursor row visible.
    fn move_cursor_with(&mut self, cx: &mut Context<Self>, mv: impl FnOnce(&mut Tab, &[&Entry])) {
        let mut t = self.tab().clone();
        let rows = self.rows_for(&t);
        mv(&mut t, &rows);
        let ix = t.cursor.as_ref().and_then(|c| rows.iter().position(|e| &e.path == c));
        let id = t.id;
        let view = t.view;
        *self.tab_mut() = t;
        if let (Some(ix), Some(h)) = (ix, self.scroll.get(&id)) {
            let item = if view == ViewMode::Icons {
                ix / self.icons_per_row.get()
            } else {
                let tab = self.tab();
                crate::ui::browser::display_index(&self.rows_for(tab), tab.sort.group_kind, ix)
            };
            h.scroll_to_item(item, ScrollStrategy::Nearest);
        }
        if view == ViewMode::Columns {
            self.refresh_visible(cx);
        }
        cx.notify();
    }

    /// Left/right: in columns view move between columns; in icons view move by one tile.
    fn horizontal_move(&mut self, dir: isize, cx: &mut Context<Self>) {
        match self.tab().view {
            ViewMode::Icons => self.move_cursor(dir, false, cx),
            ViewMode::Columns => {
                if dir < 0 {
                    if let Some(p) = self.tab().location.parent().map(Path::to_path_buf) {
                        self.navigate(p, cx);
                    }
                } else if let Some(e) = self.focused_entry().filter(|e| e.is_dir()) {
                    self.navigate(e.path.clone(), cx);
                    let rows = self.rows();
                    if let Some(first) = rows.first().map(|e| e.path.clone()) {
                        self.tab_mut().select_only(&first);
                    }
                }
            }
            ViewMode::List => {}
        }
    }

    /// Type-to-select (N05): letters typed within 1 s accumulate.
    pub fn type_to_select(&mut self, ch: &str, cx: &mut Context<Self>) {
        let now = Instant::now();
        if now.duration_since(self.type_buffer.1) > Duration::from_secs(1) {
            self.type_buffer.0.clear();
        }
        self.type_buffer.0.push_str(ch);
        self.type_buffer.1 = now;
        let prefix = self.type_buffer.0.clone();
        let mut t = self.tab().clone();
        let rows = self.rows_for(&t);
        if t.type_select(&rows, &prefix) {
            let ix = t.cursor.as_ref().and_then(|c| rows.iter().position(|e| &e.path == c));
            if let (Some(ix), Some(h)) = (ix, self.scroll.get(&t.id)) {
                h.scroll_to_item(ix, ScrollStrategy::Center);
            }
            *self.tab_mut() = t;
            cx.notify();
        }
    }

    pub fn add_favorite(&mut self, cx: &mut Context<Self>) {
        let target =
            self.selected_entries().into_iter().find(|e| e.is_dir()).map(|e| e.path).unwrap_or_else(|| self.tab().location.clone());
        if !self.prefs.favorites.contains(&target) {
            self.prefs.favorites.push(target);
            self.save_session();
        }
        cx.notify();
    }

    /// Finder's standard tag for an English key ("Blue"), as read at startup (D-3).
    pub fn standard_tag(&self, english: &str) -> Option<&wfs::tags::Tag> {
        self.standard_tags.iter().find(|(key, ..)| *key == english).map(|(.., tag)| tag)
    }

    /// Shown name of a tag filter key: Finder's localized name for a standard tag, else the key.
    pub fn tag_label(&self, key: &str) -> String {
        self.standard_tag(key).map_or_else(|| key.to_owned(), |t| t.name.clone())
    }

    /// Sidebar tag filter (v0.3): Spotlight query scoped to home, scan fallback.
    pub fn toggle_tag_filter(&mut self, tag: &str, cx: &mut Context<Self>) {
        if self.tag_filter.as_deref() == Some(tag) {
            self.tag_filter = None;
            self.search.clear();
        } else {
            self.tag_filter = Some(tag.to_string());
            crate::ui::search::start_tag_search(self, tag.to_string(), cx);
        }
        cx.notify();
    }

    pub fn remove_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        // Removes the sidebar reference only; the folder is untouched.
        self.prefs.favorites.retain(|p| p != path);
        self.save_session();
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::command::{Command, CommandItem, CommandState};
        let state = cx.new(|cx| CommandState::new(window, cx));
        let commands = palette_commands();
        let labels: Vec<&'static str> = commands.iter().map(|(l, _)| *l).collect();
        let actions: std::rc::Rc<Vec<Box<dyn Action>>> = std::rc::Rc::new(commands.into_iter().map(|(_, a)| a).collect());
        let focus = self.pane_focus[self.active_pane].clone();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // Without focus in the search field, typing goes nowhere and Return only closes the dialog.
            if first.replace(false) {
                state.focus_handle(cx).focus(window, cx);
            }
            let state = state.clone();
            let labels = labels.clone();
            let actions = actions.clone();
            let focus = focus.clone();
            dialog.close_button(false).p_0().w(px(520.)).content(move |content, _, _| {
                let actions = actions.clone();
                let focus = focus.clone();
                content.child(
                    Command::new(&state)
                        .bordered(false)
                        .placeholder("Type a command…")
                        .items(labels.iter().map(|l| CommandItem::new().label(*l)))
                        .on_confirm(move |ix, window, cx| {
                            let action = actions[ix.row].boxed_clone();
                            window.close_dialog(cx);
                            // Dispatch from the browser so workspace handlers receive it.
                            window.focus(&focus, cx);
                            window.dispatch_action(action, cx);
                        }),
                )
            })
        });
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::ui::search::open_search_dialog(self, window, cx);
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::chrome::render_workspace(self, window, cx)
    }
}
