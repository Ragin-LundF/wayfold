//! Modeless preview panel (P01–P03). Name, kind, size, modified and path are always shown;
//! content decodes on a worker with generation-based cancellation.

use crate::ui::browser::{format_time, icon, size_text, tag_dots};
use crate::ui::theme::palette;
use crate::ui::workspace::Workspace;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::Sizable as _;
use std::path::PathBuf;
use waypoint_core::fs::{format_size, Entry};
use waypoint_core::preview::{self, Preview};
use waypoint_core::{platform, pool};

#[derive(Default)]
pub struct PreviewState {
    pub path: Option<PathBuf>,
    pub modified: Option<std::time::SystemTime>,
    pub content: Option<Preview>,
    task: Option<Task<()>>,
}

fn thumb_dir() -> PathBuf {
    std::env::temp_dir().join("waypoint-quicklook")
}

impl Workspace {
    /// Start decoding when the focused file changed. Dropping the old task discards its result.
    fn request_preview(&mut self, e: &Entry, cx: &mut Context<Self>) {
        if self.preview.path.as_ref() == Some(&e.path) && self.preview.modified == e.modified {
            return;
        }
        self.preview.path = Some(e.path.clone());
        self.preview.modified = e.modified;
        self.preview.content = None;
        if e.is_dir() {
            self.preview.task = None;
            if !self.folder_sizes.contains_key(&e.path) {
                self.compute_size(e.path.clone(), cx);
            }
            return;
        }
        let path = e.path.clone();
        let fut = pool::for_path(&path).run(move || preview::load(&path, &thumb_dir()));
        let expected = e.path.clone();
        self.preview.task = Some(cx.spawn(async move |this, cx| {
            let Some(content) = fut.await else { return };
            this.update(cx, |w, cx| {
                if w.preview.path.as_ref() == Some(&expected) {
                    w.preview.content = Some(content);
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

pub fn render_preview(w: &mut Workspace, _window: &mut Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let p = palette(cx);
    let entry = w.focused_entry();
    if let Some(e) = &entry {
        w.request_preview(e, cx);
    }
    let head = h_flex()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(p.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Preview"))
        .child(div().flex_1())
        .child(Button::new("preview-close").ghost().xsmall().icon(icon("x")).tooltip("Close preview (Space / Esc)").on_click(cx.listener(|w, _, window, cx| {
            w.preview_open = false;
            w.focus_browser(window, cx);
            cx.notify();
        })));

    let body: AnyElement = match entry {
        None => div().p_4().text_color(p.muted).child("Select a file to preview.").into_any_element(),
        Some(e) => {
            let size: SharedString = if e.is_dir() {
                size_text(w, &e)
            } else {
                e.size.map(|s| format!("{} · {} bytes", format_size(s), s)).unwrap_or_else(|| "—".into()).into()
            };
            let meta = v_flex()
                .gap_1()
                .child(h_flex().gap_2().child(div().text_lg().font_weight(FontWeight::SEMIBOLD).truncate().child(e.display_name())).child(tag_dots(&e)))
                .child(div().text_sm().text_color(p.muted).child(SharedString::from(format!("{} · {}", e.kind_label(), size))))
                .child(div().text_sm().text_color(p.muted).child(SharedString::from(format!("Modified {}", format_time(e.modified)))))
                .child(div().text_xs().text_color(p.muted).child(SharedString::from(e.path.display().to_string())))
                .when_some(e.error.clone(), |d, err| d.child(div().text_sm().text_color(p.danger).child(SharedString::from(err))));
            let content: AnyElement = if e.is_dir() {
                folder_body(w, &e, cx)
            } else {
                match &w.preview.content {
                    None => div().text_color(p.muted).child("Loading preview…").into_any_element(),
                    Some(Preview::Text { text, truncated }) => v_flex()
                        .gap_1()
                        .child(div().font_family("Menlo").text_xs().whitespace_normal().child(SharedString::from(text.clone())))
                        .when(*truncated, |d| d.child(div().text_xs().text_color(p.warning).child("Preview truncated")))
                        .into_any_element(),
                    Some(Preview::Markdown(md)) => TextView::markdown("md-preview", md.clone()).selectable(true).into_any_element(),
                    Some(Preview::Html(html)) => v_flex()
                        .gap_2()
                        .child(div().text_xs().text_color(p.muted).child("Static HTML preview (scripts and remote content disabled)"))
                        .child(TextView::html("html-preview", html.clone()).selectable(true))
                        .into_any_element(),
                    Some(Preview::Csv { rows, truncated }) => csv_table(rows, *truncated, cx),
                    Some(Preview::Image { width, height }) => v_flex()
                        .gap_2()
                        .child(img(e.path.clone()).w_full().max_h(px(420.)).object_fit(ObjectFit::Contain))
                        .child(div().text_xs().text_color(p.muted).child(SharedString::from(format!("{width} × {height} px"))))
                        .into_any_element(),
                    Some(Preview::Thumbnail(png)) => v_flex()
                        .gap_2()
                        .child(img(png.clone()).w_full().max_h(px(480.)).object_fit(ObjectFit::Contain))
                        .child(quick_look_button(&e.path))
                        .into_any_element(),
                    Some(Preview::Unsupported(why)) => v_flex()
                        .gap_2()
                        .child(div().text_color(p.muted).child(SharedString::from(format!("No preview: {why}"))))
                        .child(quick_look_button(&e.path))
                        .into_any_element(),
                }
            };
            v_flex().p_4().gap_3().child(meta).child(div().h(px(1.)).bg(p.line)).child(content).into_any_element()
        }
    };
    v_flex()
        .id("preview")
        .size_full()
        .bg(p.panel)
        .border_l_1()
        .border_color(p.line)
        .child(head)
        .child(div().id("preview-scroll").flex_1().overflow_y_scroll().child(body))
}

fn quick_look_button(path: &std::path::Path) -> impl IntoElement {
    let path = path.to_path_buf();
    Button::new("ql").small().label("Open in Quick Look").on_click(move |_, _, _| {
        let _ = platform::quick_look(&path);
    })
}

fn folder_body(w: &Workspace, e: &Entry, cx: &mut Context<Workspace>) -> AnyElement {
    let p = palette(cx);
    let state = w.folder_sizes.get(&e.path).cloned();
    let path = e.path.clone();
    let text = match &state {
        None => "Size not calculated".to_string(),
        Some(crate::ui::fileops::SizeState::Calculating(t)) => format!("Calculating… {} items · {}", t.items, format_size(t.bytes)),
        Some(crate::ui::fileops::SizeState::Done(t)) => {
            let mut s = format!("{} items · {} ({} bytes, logical)", t.items, format_size(t.bytes), t.bytes);
            if t.errors > 0 {
                s.push_str(&format!("\nPartial: {} unreadable folder(s) or item(s)", t.errors));
            }
            s
        }
    };
    v_flex()
        .gap_2()
        .child(div().text_sm().child(SharedString::from(text)))
        .child(Button::new("recalc").small().label("Recalculate").on_click(cx.listener(move |w, _, _, cx| {
            w.folder_sizes.remove(&path);
            w.compute_size(path.clone(), cx);
        })))
        .child(div().text_xs().text_color(p.muted).child("Symlinks are not followed; hard links count per entry."))
        .into_any_element()
}

fn csv_table(rows: &[Vec<String>], truncated: bool, cx: &mut Context<Workspace>) -> AnyElement {
    let p = palette(cx);
    v_flex()
        .id("csv")
        .overflow_x_scroll()
        .children(rows.iter().take(200).enumerate().map(|(i, r)| {
            h_flex()
                .when(i == 0, |d| d.bg(p.canvas).font_weight(FontWeight::SEMIBOLD))
                .border_b_1()
                .border_color(p.line.opacity(0.5))
                .children(r.iter().map(|c| div().w(px(110.)).flex_none().px_1().py_0p5().text_xs().truncate().child(SharedString::from(c.clone()))))
        }))
        .when(truncated || rows.len() > 200, |d| d.child(div().text_xs().text_color(p.warning).child(format!("Showing first {} rows", rows.len().min(200)))))
        .into_any_element()
}
