//! Cell grid, damage tracking, and the renderer interface.

pub mod ascii;
pub mod halfblock;

use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: Option<[u8; 3]>,
}

pub trait Renderer {
    fn draw(&mut self, rgb: &[u8], w: u32, h: u32, out: &mut Vec<u8>);
    /// Force a full repaint (after a resize, or when something else has written
    /// to the screen).
    fn invalidate(&mut self);
    fn invalidate_row(&mut self, y: u16);
    /// Top-left screen cell (1-based) where this grid is drawn.
    fn set_origin(&mut self, x: u16, y: u16);
    /// Emit whole frames as plain rows with no cursor addressing, for when
    /// stdout is a pipe or a file rather than a terminal.
    fn set_sequential(&mut self, on: bool);
    /// Colour painted where a cell has no background of its own. Some(black)
    /// makes the picture independent of the terminal's theme; None leaves the
    /// terminal's own background (and any transparency) showing through.
    fn set_canvas_bg(&mut self, bg: Option<[u8; 3]>);
}

pub fn write_fg(out: &mut Vec<u8>, c: [u8; 3], truecolor: bool) {
    if truecolor {
        let _ = write!(out, "\x1b[38;2;{};{};{}m", c[0], c[1], c[2]);
    } else {
        let _ = write!(out, "\x1b[38;5;{}m", cube256(c));
    }
}

pub fn write_bg(out: &mut Vec<u8>, c: Option<[u8; 3]>, truecolor: bool) {
    match c {
        None => out.extend_from_slice(b"\x1b[49m"),
        Some(c) if truecolor => {
            let _ = write!(out, "\x1b[48;2;{};{};{}m", c[0], c[1], c[2]);
        }
        Some(c) => {
            let _ = write!(out, "\x1b[48;5;{}m", cube256(c));
        }
    }
}

fn cube256(c: [u8; 3]) -> u8 {
    16 + 36 * (c[0] as u16 * 6 / 256) as u8
        + 6 * (c[1] as u16 * 6 / 256) as u8
        + (c[2] as u16 * 6 / 256) as u8
}

/// A double-buffered character grid. Only cells that actually changed get
/// written, and runs sharing colours are coalesced into one escape sequence --
/// without this a full repaint at 24fps saturates most terminals.
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub origin: (u16, u16),
    pub sequential: bool,
    pub canvas_bg: Option<[u8; 3]>,
    cur: Vec<Cell>,
    prev: Vec<Option<Cell>>,
    truecolor: bool,
}

const BLANK: Cell = Cell { ch: ' ', fg: [0, 0, 0], bg: None };

impl Grid {
    pub fn new(truecolor: bool) -> Self {
        Self {
            cols: 0,
            rows: 0,
            origin: (1, 1),
            sequential: false,
            canvas_bg: None,
            cur: Vec::new(),
            prev: Vec::new(),
            truecolor,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let n = cols as usize * rows as usize;
        self.cur = vec![BLANK; n];
        self.prev = vec![None; n];
    }

    /// Mutable slice of one row, so renderers can walk pixels and cells in
    /// lockstep instead of paying a bounds check per cell.
    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [Cell] {
        let w = self.cols as usize;
        &mut self.cur[y * w..(y + 1) * w]
    }

    pub fn invalidate(&mut self) {
        self.prev.iter_mut().for_each(|p| *p = None);
    }

    pub fn set_canvas_bg(&mut self, bg: Option<[u8; 3]>) {
        if self.canvas_bg != bg {
            self.canvas_bg = bg;
            self.invalidate();
        }
    }

    pub fn set_origin(&mut self, x: u16, y: u16) {
        if self.origin != (x, y) {
            self.origin = (x, y);
            self.invalidate();
        }
    }

    /// Mark the grid row covering the given 1-based screen row as dirty, so an
    /// overlay drawn on top of the picture gets repaired on the next frame.
    pub fn invalidate_screen_row(&mut self, screen_row: u16) {
        let gy = screen_row as i32 - self.origin.1 as i32;
        if gy >= 0 {
            self.invalidate_row(gy as u16);
        }
    }

    pub fn invalidate_row(&mut self, y: u16) {
        if y < self.rows {
            let w = self.cols as usize;
            let s = y as usize * w;
            self.prev[s..s + w].iter_mut().for_each(|p| *p = None);
        }
    }

    /// No cursor addressing and no damage tracking: every frame is emitted in
    /// full, so the result is readable when piped or paged.
    fn flush_sequential(&mut self, out: &mut Vec<u8>) {
        let w = self.cols as usize;
        let mut buf = [0u8; 4];
        for y in 0..self.rows as usize {
            let mut fg: Option<[u8; 3]> = None;
            // each row ends with ESC[0m, so the background is already default
            // here; saying so avoids a redundant ESC[49m on every row
            let mut bg: Option<Option<[u8; 3]>> = Some(None);
            for x in 0..w {
                let c = self.cur[y * w + x];
                if fg != Some(c.fg) {
                    write_fg(out, c.fg, self.truecolor);
                    fg = Some(c.fg);
                }
                let cbg = c.bg.or(self.canvas_bg);
                if bg != Some(cbg) {
                    write_bg(out, cbg, self.truecolor);
                    bg = Some(cbg);
                }
                out.extend_from_slice(c.ch.encode_utf8(&mut buf).as_bytes());
            }
            out.extend_from_slice(b"\x1b[0m\n");
        }
    }

    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if self.sequential {
            return self.flush_sequential(out);
        }
        let w = self.cols as usize;
        // SGR state persists across cursor moves, so track it for the whole frame
        let mut fg: Option<[u8; 3]> = None;
        let mut bg: Option<Option<[u8; 3]>> = None;
        let mut buf = [0u8; 4];

        for y in 0..self.rows {
            let row = y as usize * w;
            let mut x = 0usize;
            while x < w {
                if self.prev[row + x] == Some(self.cur[row + x]) {
                    x += 1;
                    continue;
                }
                let _ = write!(out, "\x1b[{};{}H",
                               self.origin.1 + y, self.origin.0 + x as u16);
                while x < w && self.prev[row + x] != Some(self.cur[row + x]) {
                    let c = self.cur[row + x];
                    if fg != Some(c.fg) {
                        write_fg(out, c.fg, self.truecolor);
                        fg = Some(c.fg);
                    }
                    let cbg = c.bg.or(self.canvas_bg);
                    if bg != Some(cbg) {
                        write_bg(out, cbg, self.truecolor);
                        bg = Some(cbg);
                    }
                    out.extend_from_slice(c.ch.encode_utf8(&mut buf).as_bytes());
                    self.prev[row + x] = Some(c);
                    x += 1;
                }
            }
        }
    }
}
