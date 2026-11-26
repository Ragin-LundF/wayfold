//! A cheap, owned copy of the visible terminal screen for the renderer.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// `fg`/`bg` are `0xRRGGBB`; `None` means the theme's default color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCell {
    pub ch: char,
    pub style: CellStyle,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalSnapshot {
    pub rows: Vec<Vec<SnapshotCell>>,
    /// `(row, column)` within `rows`, when the cursor is visible.
    pub cursor: Option<(usize, usize)>,
    pub exited: bool,
}

impl TerminalSnapshot {
    /// Visible text, one string per row with trailing blanks trimmed.
    pub fn text_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }
}

pub(super) fn capture<T: EventListener>(term: &Term<T>, exited: bool) -> TerminalSnapshot {
    let grid = term.grid();
    let offset = grid.display_offset();
    let colors = term.colors();
    let rows = (0..grid.screen_lines())
        .map(|row| {
            let line = &grid[Line(row as i32 - offset as i32)];
            (0..grid.columns())
                .filter_map(|column| {
                    let cell = &line[Column(column)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        return None;
                    }
                    let style = CellStyle {
                        fg: resolve(cell.fg, colors),
                        bg: resolve(cell.bg, colors),
                        bold: cell.flags.contains(Flags::BOLD),
                        italic: cell.flags.contains(Flags::ITALIC),
                        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                        inverse: cell.flags.contains(Flags::INVERSE),
                    };
                    let ch = if cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        cell.c
                    };
                    Some(SnapshotCell { ch, style })
                })
                .collect()
        })
        .collect();
    let point = grid.cursor.point;
    let cursor_row = point.line.0 + offset as i32;
    let visible = term.mode().contains(TermMode::SHOW_CURSOR)
        && (0..grid.screen_lines() as i32).contains(&cursor_row);
    let cursor = visible.then_some((cursor_row as usize, point.column.0));
    TerminalSnapshot {
        rows,
        cursor,
        exited,
    }
}

fn resolve(color: Color, colors: &Colors) -> Option<u32> {
    let index = match color {
        Color::Spec(rgb) => return Some(pack(rgb)),
        Color::Indexed(index) => usize::from(index),
        Color::Named(named) => match named as usize {
            index @ 0..=15 => index,
            dim @ 259..=266 => dim - 259, // DimBlack..DimWhite
            _ => return None,
        },
    };
    Some(colors[index].map_or_else(|| xterm_color(index), pack))
}

fn pack(rgb: Rgb) -> u32 {
    u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
}

/// The standard xterm 256-color palette (0-15 tuned for the dark terminal background).
fn xterm_color(index: usize) -> u32 {
    const ANSI: [u32; 16] = [
        0x2e3440, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xc8ccd4, 0x5c6370,
        0xff7a85, 0xb5e890, 0xffd68a, 0x82c4ff, 0xde9bf0, 0x7fd4de, 0xffffff,
    ];
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let level = |value: usize| {
                if value == 0 {
                    0
                } else {
                    55 + value as u32 * 40
                }
            };
            let cube = index - 16;
            level(cube / 36) << 16 | level(cube / 6 % 6) << 8 | level(cube % 6)
        }
        _ => {
            let gray = 8 + (index.min(255) - 232) as u32 * 10;
            gray << 16 | gray << 8 | gray
        }
    }
}

const _: () = assert!(NamedColor::DimBlack as usize == 259 && NamedColor::DimWhite as usize == 266);
