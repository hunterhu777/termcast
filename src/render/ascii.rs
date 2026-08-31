//! ASCII ramp renderer: one glyph per cell chosen by luminance. Lower fidelity
//! than half-block, but it survives terminals without truecolor, SSH sessions,
//! and redirection to a file.

use super::{Cell, Grid, Renderer};

const RAMP: &[u8] = b" .`:,;!~+=*#%@";

pub struct Ascii {
    grid: Grid,
    colour: bool,
    /// Paint a per-cell background as well as a foreground.
    cell_bg: bool,
}

impl Ascii {
    pub fn new(truecolor: bool, colour: bool, cell_bg: bool) -> Self {
        Self { grid: Grid::new(truecolor), colour, cell_bg }
    }
}

/// How much of the cell the glyph inks, approximated by its position in the
/// ramp -- which is the assumption the ramp encodes in the first place.
fn coverage(idx: usize) -> f32 {
    idx as f32 / (RAMP.len() - 1) as f32
}

fn scale(c: [u8; 3], m: f32) -> [u8; 3] {
    let f = |v: u8| (v as f32 * m).clamp(0.0, 255.0) as u8;
    [f(c[0]), f(c[1]), f(c[2])]
}

/// Split a pixel into a foreground/background pair that the glyph's coverage
/// blends back to the original colour: with coverage c and spread k,
///     c*(1 + k(1-c)) + (1-c)*(1 - k*c) == 1
/// so the cell integrates to the true pixel value at any k, and k only controls
/// how pronounced the glyph texture is.
const SPREAD: f32 = 0.75;

fn split(base: [u8; 3], cov: f32) -> ([u8; 3], [u8; 3]) {
    (
        scale(base, 1.0 + SPREAD * (1.0 - cov)),
        scale(base, 1.0 - SPREAD * cov),
    )
}

/// 2nd/98th percentile of luminance. Real footage bunches into the mid-tones and
/// looks like grey mush without this stretch.
fn luma_window(rgb: &[u8], n: usize) -> (u32, u32) {
    // Percentiles do not need every pixel. Sampling ~50k of them keeps the
    // estimate statistically indistinguishable while making this pass O(1) in
    // resolution instead of O(n) -- it was a second full sweep of the frame.
    let step = (n / 50_000).max(1);
    let mut hist = [0u32; 256];
    let mut seen_n = 0u32;
    let mut i = 0;
    while i < n {
        let p = i * 3;
        let l = (rgb[p] as u32 * 299 + rgb[p + 1] as u32 * 587 + rgb[p + 2] as u32 * 114) / 1000;
        hist[l as usize] += 1;
        seen_n += 1;
        i += step;
    }
    let cut = seen_n / 50;
    let (mut lo, mut hi, mut seen) = (0u32, 255u32, 0u32);
    let _ = n;
    for (v, c) in hist.iter().enumerate() {
        seen += c;
        if seen >= cut { lo = v as u32; break; }
    }
    seen = 0;
    for (v, c) in hist.iter().enumerate().rev() {
        seen += c;
        if seen >= cut { hi = v as u32; break; }
    }
    if hi.saturating_sub(lo) < 24 { (0, 255) } else { (lo, hi) }
}

impl Renderer for Ascii {
    fn draw(&mut self, rgb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) {
        self.grid.resize(w as u16, h as u16);
        let (lo, hi) = luma_window(rgb, (w * h) as usize);
        let span = (hi - lo).max(1);
        let (wu, hu) = (w as usize, h as usize);
        let top = RAMP.len() - 1;
        for y in 0..hu {
            let src = &rgb[y * wu * 3..(y + 1) * wu * 3];
            let colour = self.colour;
            let cell_bg = self.cell_bg;
            let dst = self.grid.row_mut(y);
            // pixels and cells walked in lockstep: no indexing, no bounds checks
            for (cell, p) in dst.iter_mut().zip(src.chunks_exact(3)) {
                let c = [p[0], p[1], p[2]];
                let l = (c[0] as u32 * 299 + c[1] as u32 * 587 + c[2] as u32 * 114) / 1000;
                let l = (l.saturating_sub(lo) * 255 / span).min(255);
                let idx = ((l as usize * RAMP.len()) >> 8).min(top);
                let ch = RAMP[idx] as char;
                let base = if colour {
                    c
                } else if cell_bg {
                    // split() requires base to carry the pixel's luminance. A
                    // constant here makes bg = base*(1 - k*cov) brightest
                    // exactly where the picture is darkest, which inverts the
                    // image.
                    let g = l as u8;
                    [g, g, g]
                } else {
                    // Against the black canvas the glyph's coverage alone
                    // carries brightness, so the ink is a constant.
                    [200, 200, 200]
                };
                *cell = if cell_bg {
                    let (fg, bg) = split(base, coverage(idx));
                    Cell { ch, fg, bg: Some(bg) }
                } else {
                    Cell { ch, fg: base, bg: None }
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
