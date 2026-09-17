//! Half-block renderer: one cell carries two vertically stacked pixels by
//! printing U+2580 with foreground = top pixel and background = bottom pixel.
//! This doubles vertical resolution over one-glyph-per-cell approaches, and
//! brightness comes entirely from colour rather than from glyph shape.

use super::{Cell, Grid, Renderer};

pub struct HalfBlock {
    grid: Grid,
    colour: bool,
}

impl HalfBlock {
    pub fn new(truecolor: bool, colour: bool) -> Self {
        Self { grid: Grid::new(truecolor), colour }
    }
}

#[inline]
fn grey(p: &[u8]) -> [u8; 3] {
    let l = ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8;
    [l, l, l]
}

impl Renderer for HalfBlock {
    fn draw(&mut self, rgb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) {
        let cols = w as u16;
        let rows = (h / 2) as u16;
        self.grid.resize(cols, rows);
        let (wu, hu) = (w as usize, h as usize);
        let stride = wu * 3;
        let colour = self.colour;
        for y in 0..rows as usize {
            let ty = 2 * y;
            let by = (2 * y + 1).min(hu - 1); // odd heights reuse the last row
            let (t, b) = (&rgb[ty * stride..ty * stride + stride],
                          &rgb[by * stride..by * stride + stride]);
            let dst = self.grid.row_mut(y);
            for ((cell, tp), bp) in dst
                .iter_mut()
                .zip(t.chunks_exact(3))
                .zip(b.chunks_exact(3))
            {
                *cell = if colour {
                    Cell {
                        ch: '\u{2580}',
                        fg: [tp[0], tp[1], tp[2]],
                        bg: Some([bp[0], bp[1], bp[2]]),
                    }
                } else {
                    Cell { ch: '\u{2580}', fg: grey(tp), bg: Some(grey(bp)) }
                };
            }
        }
        self.grid.flush(out);
    }

    fn invalidate(&mut self) {
        self.grid.invalidate();
    }

    fn invalidate_row(&mut self, y: u16) {
        self.grid.invalidate_screen_row(y);
    }

    fn set_origin(&mut self, x: u16, y: u16) {
        self.grid.set_origin(x, y);
    }

    fn set_sequential(&mut self, on: bool) {
        self.grid.sequential = on;
    }

    fn set_canvas_bg(&mut self, bg: Option<[u8; 3]>) {
        self.grid.set_canvas_bg(bg);
    }
}
