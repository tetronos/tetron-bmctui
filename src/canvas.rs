//! Off-screen text buffer. The UI draws into it and it is then written to the
//! terminal in one pass. This gives three things: no full screen clear (less
//! flicker), dialogs and overlays that simply paint over what is below, and a
//! single place where text from the BMC is made safe.

use crate::view::Kind;
use crossterm::{
    cursor, queue,
    style::{Attribute, Color, Print, SetAttribute, SetForegroundColor},
};
use std::io::{self, Write};

/// A rectangle in terminal cells. Used for panes and for clickable regions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn new(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    /// True when the cell (x, y) lies inside the rectangle. Mouse events are
    /// matched against rectangles with this.
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Replace everything that is not printable ASCII. Sensor names, FRU fields and
/// error messages come from the BMC and must never reach the terminal raw: an
/// escape sequence in them could rewrite the screen or worse. Limiting to ASCII
/// also keeps one character equal to one cell.
pub fn safe_char(c: char) -> char {
    match c {
        '\t' => ' ',
        ' '..='~' => c,
        _ => '?',
    }
}

/// Like `safe_char`, for cells of the console emulator. Line drawing and Latin-1
/// characters pass too, since boot menus use them and each takes one cell.
/// Control characters never do.
pub fn safe_term_char(c: char) -> char {
    match c {
        ' '..='~' | '\u{a1}'..='\u{ac}' | '\u{ae}'..='\u{ff}' | '\u{2500}'..='\u{259f}' => c,
        '\u{a0}' => ' ',
        _ => '?',
    }
}

/// Look of a console cell. Only these parsed values come from the host. The
/// escape sequence for them is built here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TermStyle {
    pub fg: vt100::Color,
    pub bg: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl TermStyle {
    pub fn of(c: &vt100::Cell) -> TermStyle {
        TermStyle {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            dim: c.dim(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
        }
    }
}

/// The SGR parameter for a color. `base` is 30 for text and 40 for background.
/// The first 16 colors use the classic codes: terminals can treat them
/// differently from the 256 color form, for example by showing bold text in
/// the bright color, and BIOS screens rely on that.
fn color_code(c: vt100::Color, base: u8) -> Option<String> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i @ 0..=7) => Some((base + i).to_string()),
        vt100::Color::Idx(i @ 8..=15) => Some((base + 60 + i - 8).to_string()),
        vt100::Color::Idx(i) => Some(format!("{};5;{i}", base + 8)),
        vt100::Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

/// The whole escape sequence for a style, starting from a reset.
fn sgr(s: TermStyle) -> String {
    let mut codes = vec!["0".to_string()];
    for (on, code) in [(s.bold, "1"), (s.dim, "2"), (s.italic, "3"), (s.underline, "4"), (s.inverse, "7")] {
        if on {
            codes.push(code.into());
        }
    }
    codes.extend(color_code(s.fg, 30));
    codes.extend(color_code(s.bg, 40));
    format!("\x1b[{}m", codes.join(";"))
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    cells: Vec<(char, Kind)>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, cells: vec![(' ', Kind::Normal); w * h] }
    }

    /// Write `text` at (x, y), cut off after `max` cells or at the right edge.
    pub fn text(&mut self, x: usize, y: usize, max: usize, text: &str, kind: Kind) {
        if y >= self.h {
            return;
        }
        for (i, c) in text.chars().take(max).enumerate() {
            if x + i >= self.w {
                break;
            }
            self.cells[y * self.w + x + i] = (safe_char(c), kind);
        }
    }

    /// Put one console cell at (x, y).
    pub fn term_cell(&mut self, x: usize, y: usize, c: char, kind: Kind) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = (safe_term_char(c), kind);
        }
    }

    /// Like `text`, but pads with spaces to exactly `width` cells. Used for
    /// selected rows and bars, so the highlight covers the whole row.
    pub fn line(&mut self, x: usize, y: usize, width: usize, text: &str, kind: Kind) {
        let padded = format!("{text:<width$}");
        self.text(x, y, width, &padded, kind);
    }

    /// Blank a rectangle. Overlays call this before drawing on top.
    pub fn clear(&mut self, r: Rect) {
        for y in r.y..r.y + r.h {
            self.line(r.x, y, r.w, "", Kind::Normal);
        }
    }

    /// ASCII box around `r` (the border cells are inside `r`).
    pub fn frame(&mut self, r: Rect) {
        if r.w < 2 || r.h < 2 {
            return;
        }
        let edge = format!("+{}+", "-".repeat(r.w - 2));
        self.text(r.x, r.y, r.w, &edge, Kind::Dim);
        self.text(r.x, r.y + r.h - 1, r.w, &edge, Kind::Dim);
        for y in r.y + 1..r.y + r.h - 1 {
            self.text(r.x, y, 1, "|", Kind::Dim);
            self.text(r.x + r.w - 1, y, 1, "|", Kind::Dim);
        }
    }

    /// The style of one cell, for tests of what is highlighted.
    #[cfg(test)]
    pub fn kind(&self, x: usize, y: usize) -> Kind {
        self.cells[y * self.w + x].1
    }

    /// One row as plain text. Tests use this to check what would be shown.
    #[cfg(test)]
    pub fn row(&self, y: usize) -> String {
        self.cells[y * self.w..(y + 1) * self.w].iter().map(|c| c.0).collect()
    }

    /// Send the buffer to the terminal. Neighbouring cells with the same style
    /// are written as one run to keep the output small.
    pub fn flush(&self, out: &mut impl Write) -> io::Result<()> {
        for y in 0..self.h {
            queue!(out, cursor::MoveTo(0, y as u16))?;
            let row = &self.cells[y * self.w..(y + 1) * self.w];
            for run in row.chunk_by(|a, b| a.1 == b.1) {
                let s: String = run.iter().map(|c| c.0).collect();
                match run[0].1 {
                    Kind::Normal => {}
                    Kind::Head => queue!(out, SetAttribute(Attribute::Bold))?,
                    Kind::Good => queue!(out, SetForegroundColor(Color::Green))?,
                    Kind::Warn => queue!(out, SetForegroundColor(Color::Yellow))?,
                    Kind::Bad => queue!(out, SetForegroundColor(Color::Red))?,
                    Kind::Dim => queue!(out, SetAttribute(Attribute::Dim))?,
                    Kind::Sel => queue!(out, SetAttribute(Attribute::Reverse))?,
                    Kind::Term(s) => queue!(out, Print(sgr(s)))?,
                }
                // Attribute::Reset also restores the default colors.
                queue!(out, Print(s), SetAttribute(Attribute::Reset))?;
            }
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_never_reach_the_buffer() {
        let mut c = Canvas::new(20, 1);
        c.text(0, 0, 20, "a\x1b[2Jb\u{9b}c\r\n", Kind::Normal);
        assert_eq!(c.row(0).trim_end(), "a?[2Jb?c??");
    }

    #[test]
    fn console_colors_use_classic_codes() {
        use vt100::Color::{Idx, Rgb};
        assert_eq!(sgr(TermStyle::default()), "\x1b[0m");
        let s = TermStyle { fg: Idx(3), bg: Idx(4), bold: true, ..Default::default() };
        assert_eq!(sgr(s), "\x1b[0;1;33;44m");
        let s = TermStyle { fg: Idx(15), bg: Idx(200), inverse: true, ..Default::default() };
        assert_eq!(sgr(s), "\x1b[0;7;97;48;5;200m");
        assert_eq!(sgr(TermStyle { fg: Rgb(1, 2, 3), ..Default::default() }), "\x1b[0;38;2;1;2;3m");
    }

    /// Colors survive the whole way: host bytes, emulator, buffer, terminal.
    #[test]
    fn host_colors_reach_the_output() {
        let mut p = vt100::Parser::new(vt100::Parser::DEFAULT_ROWS, vt100::Parser::DEFAULT_COLS, 0);
        p.process(b"\x1b[1;37;44mMain\x1b[0m");
        let cell = p.screen().cell(0, 0).unwrap();
        let mut c = Canvas::new(1, 1);
        c.term_cell(0, 0, 'M', Kind::Term(TermStyle::of(cell)));
        let mut out = vec![];
        c.flush(&mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("\x1b[0;1;37;44mM"));
    }

    #[test]
    fn text_is_clipped_to_the_buffer() {
        let mut c = Canvas::new(4, 1);
        c.text(2, 0, 10, "abcdef", Kind::Normal);
        c.text(0, 5, 10, "ignored", Kind::Normal);
        assert_eq!(c.row(0), "  ab");
    }
}
