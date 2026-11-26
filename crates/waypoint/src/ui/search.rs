//! Search (S01/S02): Filter (default, current folder), scoped Spotlight, recursive filename
//! scan, content/regex. A new query drops the previous task, which cancels its worker.

use crate::app::state::TabId;
use crate::ui::browser::{entry_icon, format_time, size_text, tag_dots};
use crate::ui::fileops::{field, footer};
use crate::ui::theme::{palette, ROW_H};
use crate::ui::workspace::{CancelGuard, Workspace};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex, WindowExt as _};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::{Selectable as _, Sizable as _};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use waypoint_core::fs::{stat_entry, Entry};
use waypoint_core::platform::spotlight;
use waypoint_core::pool::{self, CancelToken};
use waypoint_core::search::{self, ContentQuery, ScanOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    Filter,
    Spotlight,
    Recursive,
    Content,
}

impl Scope {
    const ALL: [Scope; 4] = [Scope::Filter, Scope::Spotlight, Scope::Recursive, Scope::Content];
    fn label(self) -> &'static str {
        match self {
            Scope::Filter => "Filter current folder",
            Scope::Spotlight => "Spotlight",
            Scope::Recursive => "Recursive filename",
            Scope::Content => "Content / regex",
        }
    }
}

#[derive(Default)]
pub struct SearchState {
    /// Tab whose view shows the results.
    pub tab: Option<TabId>,
    pub title: String,
    pub results: Vec<Entry>,
    /// Content hits: path → (line, snippet).
    pub snippets: HashMap<PathBuf, (usize, String)>,
    pub running: bool,
    pub note: Option<String>,
    pub skipped: Vec<(PathBuf, String)>,
    pub scope: Scope,
    pub regex: bool,
    pub extensions: String,
    task: Option<Task<()>>,
}

impl SearchState {
    pub fn clear(&mut self) {
        *self = SearchState { scope: self.scope, regex: self.regex, extensions: std::mem::take(&mut self.extensions), ..Default::default() };
    }

    pub fn active_for(&self, tab: TabId) -> bool {
        self.tab == Some(tab)
    }
}

enum Hit {
    Paths(Vec<PathBuf>),
    Content(Vec<search::ContentHit>),
}

pub fn open_search_dialog(w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let initial = w.tab().filter.clone();
    let query = cx.new(|cx| InputState::new(window, cx).placeholder("Name, text or pattern").default_value(initial));
    let exts = {
        let e = w.search.extensions.clone();
        cx.new(|cx| InputState::new(window, cx).placeholder("e.g. rs, md, txt (empty = all)").default_value(e))
    };
    let scope = Rc::new(Cell::new(w.search.scope));
    let regex = Rc::new(Cell::new(w.search.regex));
    let this = cx.entity().downgrade();
    let location = w.tab().location.clone();
    let first = Cell::new(true);
    let submit = {
        let (query, exts, scope, regex, this) = (query.clone(), exts.clone(), scope.clone(), regex.clone(), this.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| {
            let q = query.read(cx).value().to_string();
            let e = exts.read(cx).value().to_string();
            window.close_dialog(cx);
            let _ = this.update(cx, |w, cx| {
                w.search.scope = scope.get();
                w.search.regex = regex.get();
                w.search.extensions = e;
                run_search(w, q, cx);
                w.focus_browser(window, cx);
            });
        })
    };
    let s2 = submit.clone();
    w._subscriptions.push(cx.subscribe_in(&query, window, move |_, _, ev: &InputEvent, window, cx| {
        if let InputEvent::PressEnter { .. } = ev {
            s2(window, cx);
        }
    }));
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            query.focus_handle(cx).focus(window, cx);
        }
        let p = palette(cx);
        let current = scope.get();
        let (submit, submit_clear, this2) = (submit.clone(), submit.clone(), this.clone());
        let query2 = query.clone();
        dialog
            .title("Search files")
            .w(px(620.))
            .child(field("Search query", Input::new(&query)))
            .child(field(
                "Search method",
                h_flex().flex_wrap().gap_2().children(Scope::ALL.iter().map(|s| {
                    let (s, scope) = (*s, scope.clone());
                    Button::new(SharedString::from(format!("scope-{s:?}"))).small().label(s.label()).selected(current == s).on_click(move |_, window, _| {
                        scope.set(s);
                        window.refresh();
                    })
                })),
            ))
            .when(current == Scope::Content, |d| {
                let regex = regex.clone();
                let on = regex.get();
                d.child(field("File extensions", Input::new(&exts))).child(
                    gpui_kit::component::checkbox::Checkbox::new("regex").label("Regular expression").checked(on).on_click(move |v, window, _| {
                        regex.set(*v);
                        window.refresh();
                    }),
                )
            })
            .child(div().text_sm().text_color(p.muted).child(SharedString::from(format!(
                "Location: {}\nNo persistent index. Spotlight respects macOS privacy settings and falls back to a scan.",
                location.display()
            ))))
            .footer(
                footer(cx)
                    .child(Button::new("clear").label("Clear").on_click(move |_, window, cx| {
                        query2.update(cx, |q, cx| q.set_value("", window, cx));
                        let _ = this2.update(cx, |w, cx| {
                            w.tab_mut().filter.clear();
                            w.search.clear();
                            cx.notify();
                        });
                        let _ = &submit_clear;
                        window.close_dialog(cx);
                    }))
                    .child(Button::new("go").primary().label("Search").on_click(move |_, window, cx| submit(window, cx))),
            )
    });
}

fn run_search(w: &mut Workspace, query: String, cx: &mut Context<Workspace>) {
    let scope = w.search.scope;
    w.tag_filter = None;
    if scope == Scope::Filter || query.is_empty() {
        w.search.clear();
        w.tab_mut().filter = query;
        cx.notify();
        return;
    }
    w.tab_mut().filter.clear();
    let root = w.tab().location.clone();
    let title = format!("{}: {query}", scope.label());
    let opts = ScanOptions { include_hidden: w.filters.show_hidden, ..Default::default() };
    let content = ContentQuery {
        pattern: query.clone(),
        regex: w.search.regex,
        case_insensitive: true,
        extensions: w.search.extensions.split([',', ' ']).map(|s| s.trim().trim_start_matches('.').to_lowercase()).filter(|s| !s.is_empty()).collect(),
        max_file_bytes: 20 << 20,
    };
    start(w, title, cx, move |cancel, tx| match scope {
        Scope::Spotlight => {
            let r = spotlight::run(&spotlight::name_query(&query), &root, cancel, 5_000, |b| tx.send_blocking(Hit::Paths(b)).is_ok());
            match r {
                Ok(()) => None,
                Err(e) => {
                    // Fallback when Spotlight is unavailable for this location.
                    let rep = search::find_names(&root, &query, &opts, cancel, |b| {
                        let _ = tx.send_blocking(Hit::Paths(b));
                    });
                    Some((format!("{e}; used a recursive scan instead"), rep))
                }
            }
        }
        Scope::Recursive => Some((String::new(), search::find_names(&root, &query, &opts, cancel, |b| {
            let _ = tx.send_blocking(Hit::Paths(b));
        }))),
        Scope::Content => match search::find_content(&root, &content, &opts, cancel, |b| {
            let _ = tx.send_blocking(Hit::Content(b));
        }) {
            Ok(rep) => Some((String::new(), rep)),
            Err(e) => Some((format!("Invalid regular expression: {e}"), Default::default())),
        },
        Scope::Filter => None,
    });
}

/// Tag sidebar filter: Spotlight metadata query in the home folder, scan fallback (v0.3).
pub fn start_tag_search(w: &mut Workspace, tag: String, cx: &mut Context<Workspace>) {
    let root = w.home.clone();
    start(w, format!("Tag: {tag}"), cx, move |cancel, tx| {
        match spotlight::run(&spotlight::tag_query(&tag), &root, cancel, 5_000, |b| tx.send_blocking(Hit::Paths(b)).is_ok()) {
            Ok(()) => None,
            Err(e) => {
                let mut report = waypoint_core::search::ScanReport::default();
                let opts = ScanOptions::default();
                // Scan fallback: names don't matter, check tags of every entry.
                let r = search::find_names(&root, "", &ScanOptions { max_results: usize::MAX, ..opts }, cancel, |b| {
                    let tagged: Vec<PathBuf> = b.into_iter().filter(|p| waypoint_core::fs::tags::read(p).contains(&tag)).collect();
                    if !tagged.is_empty() {
                        let _ = tx.send_blocking(Hit::Paths(tagged));
                    }
                });
                report.skipped = r.skipped;
                Some((format!("{e}; scanned instead"), report))
            }
        }
    });
}

type Job = Box<dyn FnOnce(&CancelToken, &async_channel::Sender<Hit>) -> Option<(String, search::ScanReport)> + Send>;

fn start(
    w: &mut Workspace,
    title: String,
    cx: &mut Context<Workspace>,
    job: impl FnOnce(&CancelToken, &async_channel::Sender<Hit>) -> Option<(String, search::ScanReport)> + Send + 'static,
) {
    let job: Job = Box::new(job);
    let tab = w.tab().id;
    let root = w.tab().location.clone();
    w.search.clear();
    w.search.tab = Some(tab);
    w.search.title = title;
    w.search.running = true;
    let (tx, rx) = async_channel::bounded::<Hit>(32);
    let (done_tx, done_rx) = async_channel::bounded(1);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    pool::for_path(&root).execute(move || {
        let r = job(&c2, &tx);
        drop(tx);
        let _ = done_tx.send_blocking(r);
    });
    w.search.task = Some(cx.spawn(async move |this, cx| {
        let _guard = CancelGuard(cancel);
        while let Ok(hit) = rx.recv().await {
            // Stat on the worker so the UI thread never touches the filesystem.
            let entries = pool::local()
                .run(move || match hit {
                    Hit::Paths(ps) => ps.iter().map(|p| (stat_entry(p), None)).collect::<Vec<_>>(),
                    Hit::Content(hs) => hs.into_iter().map(|h| (stat_entry(&h.path), Some((h.line, h.text)))).collect(),
                })
                .await
                .unwrap_or_default();
            if this
                .update(cx, |w, cx| {
                    for (e, snip) in entries {
                        if let Some(s) = snip {
                            w.search.snippets.insert(e.path.clone(), s);
                        }
                        w.search.results.push(e);
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        }
        let report = done_rx.recv().await.ok().flatten();
        this.update(cx, |w, cx| {
            w.search.running = false;
            if let Some((note, rep)) = report {
                let mut parts = vec![];
                if !note.is_empty() {
                    parts.push(note);
                }
                if rep.truncated {
                    parts.push("result limit reached".into());
                }
                if !rep.skipped.is_empty() {
                    parts.push(format!("{} unreadable path(s) skipped", rep.skipped.len()));
                }
                w.search.note = (!parts.is_empty()).then(|| parts.join(" · "));
                w.search.skipped = rep.skipped;
            }
            cx.notify();
        })
        .ok();
    }));
    cx.notify();
}

/// Results list for the tab that owns the search; shows each result's folder path.
pub fn render_results(w: &Workspace, ix: usize, cx: &mut Context<Workspace>) -> AnyElement {
    let p = palette(cx);
    let s = &w.search;
    let count = s.results.len();
    let header = h_flex()
        .h(px(30.))
        .px_3()
        .gap_2()
        .text_sm()
        .bg(p.panel)
        .border_b_1()
        .border_color(p.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(s.title.clone())))
        .child(div().text_color(p.muted).child(SharedString::from(format!("{count} result(s){}", if s.running { " · searching…" } else { "" }))))
        .when_some(s.note.clone(), |d, n| d.child(div().text_color(p.warning).truncate().child(SharedString::from(n))))
        .child(div().flex_1())
        .when(s.running, |d| {
            d.child(Button::new("search-stop").xsmall().label("Stop").on_click(cx.listener(|w, _, _, cx| {
                w.search.task = None;
                w.search.running = false;
                cx.notify();
            })))
        })
        .child(Button::new("search-close").xsmall().ghost().label("Close results").on_click(cx.listener(|w, _, _, cx| {
            w.search.clear();
            w.tag_filter = None;
            cx.notify();
        })));
    let list = uniform_list(
        ("search-results", ix),
        count,
        cx.processor(move |w: &mut Workspace, range: std::ops::Range<usize>, _, cx| {
            let p = palette(cx);
            let sel = w.panes[ix].tab().selection.clone();
            w.search.results[range.start.min(count)..range.end.min(w.search.results.len())]
                .iter()
                .map(|e| {
                    let path = e.path.clone();
                    let parent = e.path.parent().map(|d| d.display().to_string()).unwrap_or_default();
                    let snippet = w.search.snippets.get(&e.path).map(|(l, t)| format!("{l}: {t}"));
                    let is_sel = sel.contains(&e.path);
                    let entry = e.clone();
                    h_flex()
                        .id(SharedString::from(format!("sr-{}", e.path.display())))
                        .role(Role::Row)
                        .aria_label(SharedString::from(format!("{} in {}", e.display_name(), parent)))
                        .aria_selected(is_sel)
                        .h(px(ROW_H + 8.))
                        .px_3()
                        .gap_2()
                        .text_sm()
                        .when(is_sel, |d| d.bg(p.selection))
                        .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                        .child(entry_icon(e).small().text_color(if e.is_dir() { p.accent } else { p.muted }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(h_flex().gap_2().child(div().truncate().child(e.display_name())).child(tag_dots(e)))
                                .child(div().text_xs().text_color(p.muted).truncate().child(SharedString::from(snippet.unwrap_or(parent)))),
                        )
                        .child(div().w(px(90.)).text_color(p.muted).child(size_text(w, e)))
                        .child(div().w(px(130.)).text_color(p.muted).child(format_time(e.modified)))
                        .on_click(cx.listener(move |w, ev: &ClickEvent, _, cx| {
                            w.active_pane = ix;
                            if ev.click_count() == 2 {
                                // Open reveals the item in its folder, or opens the file.
                                if entry.is_dir() {
                                    w.search.clear();
                                    w.navigate(entry.path.clone(), cx);
                                } else {
                                    w.open_entry(&entry, cx);
                                }
                            } else {
                                w.tab_mut().select_only(&path);
                                cx.notify();
                            }
                        }))
                })
                .collect::<Vec<_>>()
        }),
    )
    .flex_1();
    v_flex().size_full().child(header).child(list).into_any_element()
}
