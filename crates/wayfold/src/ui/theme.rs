//! Semantic theme tokens from 02-design.md. Components read roles, never literal colors.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{rgb, App, Global, Hsla};

#[derive(Clone, Copy)]
pub struct Palette {
    pub canvas: Hsla,
    pub panel: Hsla,
    pub line: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub hover: Hsla,
    pub control: Hsla,
    pub terminal_bg: Hsla,
    pub dark: bool,
}

impl Global for Palette {}

impl Palette {
    pub fn dark() -> Self {
        Self {
            canvas: rgb(0x15181d).into(),
            panel: rgb(0x1d2128).into(),
            line: rgb(0x303641).into(),
            text: rgb(0xe5e9f0).into(),
            muted: rgb(0x98a3b4).into(),
            accent: rgb(0x78a9ff).into(),
            selection: rgb(0x283e60).into(),
            warning: rgb(0xe6b86b).into(),
            danger: rgb(0xef6464).into(),
            hover: rgb(0x263243).into(),
            control: rgb(0x242933).into(),
            terminal_bg: rgb(0x111419).into(),
            dark: true,
        }
    }

    /// Light equivalents of the same roles (contrast checked against WCAG AA for text/muted).
    pub fn light() -> Self {
        Self {
            canvas: rgb(0xffffff).into(),
            panel: rgb(0xf3f4f6).into(),
            line: rgb(0xd7dbe1).into(),
            text: rgb(0x1b1f24).into(),
            muted: rgb(0x5b6472).into(),
            accent: rgb(0x2563c9).into(),
            selection: rgb(0xcfe0fb).into(),
            warning: rgb(0x8a5a00).into(),
            danger: rgb(0xb42318).into(),
            hover: rgb(0xe8eef8).into(),
            control: rgb(0xffffff).into(),
            terminal_bg: rgb(0xfbfbfc).into(),
            dark: false,
        }
    }
}

pub fn palette(cx: &App) -> Palette {
    *cx.global::<Palette>()
}

/// Apply a palette to the app and to gpui-component's theme so inputs, dialogs and menus match.
pub fn apply(dark: bool, cx: &mut App) {
    let p = if dark { Palette::dark() } else { Palette::light() };
    cx.set_global(p);
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    Theme::update(cx, |t| {
        t.font_size = gpui_kit::px(13.);
        t.mono_font_size = gpui_kit::px(12.);
        t.background = p.canvas;
        t.foreground = p.text;
        t.border = p.line;
        t.input = p.line;
        t.muted = p.panel;
        t.muted_foreground = p.muted;
        t.primary = p.accent;
        t.primary_hover = p.accent.opacity(0.85);
        t.primary_active = p.accent.opacity(0.75);
        t.ring = p.accent;
        t.popover = p.panel;
        t.popover_foreground = p.text;
        t.accent = p.hover;
        t.accent_foreground = p.text;
        t.list_active = p.selection;
        t.list_hover = p.hover;
        t.selection = p.selection;
        t.secondary = p.control;
        t.secondary_hover = p.hover;
        t.secondary_foreground = p.text;
        t.warning = p.warning;
        t.danger = p.danger;
        t.progress_bar = p.accent;
        t.tab_bar = p.panel;
        t.sidebar = p.panel;
        t.title_bar = p.panel;
        t.status_bar = p.panel;
    });
}

/// The seven Finder tag colors (v0.3). Finder stores tags by name with a color index.
pub const TAGS: [(&str, u32); 7] = [
    ("Red", 0xef6464),
    ("Orange", 0xf49b50),
    ("Yellow", 0xeac54b),
    ("Green", 0x65c786),
    ("Blue", 0x68a5ef),
    ("Purple", 0xb28ae6),
    ("Gray", 0x9da5af),
];

pub fn tag_color(name: &str) -> Option<Hsla> {
    TAGS.iter().find(|(n, _)| *n == name).map(|(_, c)| rgb(*c).into())
}

// Layout tokens (02-design.md "Layout").
pub const SIDEBAR_W: f32 = 180.;
pub const TOOLBAR_H: f32 = 44.;
pub const TABSTRIP_H: f32 = 34.;
pub const HEADER_H: f32 = 28.;
pub const ROW_H: f32 = 30.;
pub const STATUS_H: f32 = 26.;
pub const PREVIEW_W: f32 = 320.;
pub const TERMINAL_H: f32 = 200.;
pub const QUEUE_H: f32 = 260.;
pub const NARROW_W: f32 = 900.;
