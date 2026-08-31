//! termcast -- a terminal media player for mp4 and webm.

mod audio;
mod media;
mod render;
mod term;

use anyhow::Result;
use clap::Parser;
use cpal::traits::StreamTrait;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use render::Renderer;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(about = "Play video in the terminal", version)]
struct Args {
    /// Media file (mp4, webm, mkv, mov, mp3, ...)
    file: String,
    /// Use the ASCII ramp instead of half-blocks
    #[arg(long)]
    ascii: bool,
    /// Render without colour (implies --ascii)
    #[arg(long)]
    mono: bool,
    /// With --ascii, also paint a per-cell background. Doubles escape traffic;
    /// worth it on terminals that cannot render U+2580 for half-blocks.
    #[arg(long)]
    ascii_bg: bool,
    /// Do not open an audio device
    #[arg(long)]
    no_audio: bool,
    /// Show a sync/perf overlay
    #[arg(long)]
    stats: bool,
    /// Append per-frame timings to FILE as CSV and print a summary on exit.
    /// Independent of --stats, so the overlay's own output does not perturb
    /// the measurement.
    #[arg(long, value_name = "FILE")]
    stats_log: Option<PathBuf>,
    /// Start this many seconds in
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    /// Stop after this many seconds
    #[arg(long)]
    duration: Option<f64>,
    /// Cap the render width in characters
    #[arg(long)]
    cols: Option<u16>,
    /// Keep the terminal's own background instead of painting the render area
    /// black. Use this to preserve a transparent or themed background.
    #[arg(long)]
    no_bg: bool,
    /// Panic after N rendered frames. Exists so the panic-safety of the
    /// terminal teardown can actually be exercised; there is no other way to
    /// provoke a panic while in raw mode.
    #[arg(long, hide = true)]
    panic_after: Option<u64>,
}

/// Fit the source into the character grid, preserving aspect ratio. Terminal
/// cells are about twice as tall as they are wide, which is why half-block
/// (two pixels per cell) yields square pixels and the ASCII ramp does not.
fn fit(vw: u32, vh: u32, cols: u16, rows: u16, halfblock: bool) -> (u32, u32) {
    let (vw, vh) = (vw.max(1) as f64, vh.max(1) as f64);
    let s = (cols as f64 / vw).min((rows as f64 * 2.0) / vh);
    let w = (vw * s).round().max(2.0) as u32;
    let ph = (vh * s).round().max(2.0) as u32;
    if halfblock {
        (w, (ph + 1) & !1) // even: two pixel rows per cell
    } else {
        (w, (ph / 2).max(1))
    }
}

fn main() {
    if let Err(e) = run() {
        term::restore();
        eprintln!("termcast: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    media::init()?;
    let info = media::probe(&args.file)?;

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let truecolor = term::truecolor();
    let use_ascii = args.ascii || args.mono || !is_tty;

    // ---- audio ---------------------------------------------------------
    let mut audio_out = None;
    if info.has_audio && !args.no_audio {
        match audio::open() {
            Ok(a) => audio_out = Some(a),
            // Non-fatal: fall back to the wall clock and play silent.
            Err(e) => eprintln!("audio unavailable ({e}); playing silent"),
        }
    }
    let clock: Arc<dyn audio::ClockSource> = match &audio_out {
        Some(a) => a.clock.clone(),
        None => Arc::new(audio::WallClock::new()),
    };
    clock.reset(args.start);
    let (arate, ach) = audio_out.as_ref().map(|a| (a.rate, a.channels)).unwrap_or((48000, 2));
    let audio_q: Option<Arc<Mutex<VecDeque<f32>>>> = audio_out.as_ref().map(|a| a.queue.clone());

    // ---- terminal ------------------------------------------------------
    // Raw mode and the alternate screen only make sense on a real terminal;
    // with a pipe or a file we just stream whole frames.
    let _guard = if is_tty { Some(term::TerminalGuard::new()?) } else { None };
    let (mut cols, mut rows) = term::size();
    if let Some(c) = args.cols {
        cols = cols.min(c);
    }

    let mut renderer: Box<dyn Renderer> = if use_ascii {
        Box::new(render::ascii::Ascii::new(truecolor, !args.mono, args.ascii_bg))
    } else {
        Box::new(render::halfblock::HalfBlock::new(truecolor))
    };
    renderer.set_sequential(!is_tty);
    // Black, not grey: a space is the ramp's darkest glyph, so this colour is
    // the picture's black point, and it also fills the letterbox bars.
    let canvas_bg = if args.no_bg || !is_tty { None } else { Some([0u8, 0, 0]) };
    renderer.set_canvas_bg(canvas_bg);
    let clear: Vec<u8> = match canvas_bg {
        Some(c) => format!("\x1b[48;2;{};{};{}m\x1b[2J", c[0], c[1], c[2]).into_bytes(),
        None => b"\x1b[2J".to_vec(),
    };

    let mut target = if info.has_video {
        fit(info.width, info.height, cols, rows, !use_ascii)
    } else {
        (2, 2)
    };

    // ---- pipeline ------------------------------------------------------
    let (vtx, vrx) = sync_channel::<media::VideoFrame>(6);
    let (ctx_tx, ctx_rx) = std::sync::mpsc::channel::<media::Cmd>();
    let shared = media::Shared {
        quit: Arc::new(AtomicBool::new(false)),
        eof: Arc::new(AtomicBool::new(false)),
        generation: Arc::new(AtomicU64::new(0)),
        // also true for the initial --start seek
        rebase: Arc::new(AtomicBool::new(true)),
        decode_us: Arc::new(AtomicU64::new(0)),
    };
    let decode_us = shared.decode_us.clone();
    let (quit, eof, generation, rebase) = (
        shared.quit.clone(),
        shared.eof.clone(),
        shared.generation.clone(),
        shared.rebase.clone(),
    );

    let handle = media::spawn(
        args.file.clone(),
        target,
        args.start,
        vtx,
        audio_q.clone(),
        arate,
        ach,
        ctx_rx,
        shared,
        clock.clone(),
    );

    if let Some(a) = &audio_out {
        a.stream.play()?;
    }

    // ---- loop ----------------------------------------------------------
    let mut out: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut stdout = std::io::stdout();
    let frame_interval = 1.0 / info.fps.max(1.0);
    let mut pending: Option<media::VideoFrame> = None;
    let mut paused = false;
    let mut show_stats = args.stats;
    let mut dropped = 0u64;
    let mut shown = 0u64;
    let mut write_ms = 0.0f64;
    let mut log = match &args.stats_log {
        Some(path) => {
            let mut f = BufWriter::new(File::create(path)?);
            writeln!(f, "wall_s,pts_s,drift_ms,draw_ms,write_ms,decode_ms,bytes,shown,dropped,px_w,px_h")?;
            Some(f)
        }
        None => None,
    };
    let mut samples: Vec<(f64, f64, f64, usize)> = Vec::new();
    let started = Instant::now();
    let mut origin = (0u16, 0u16);
    let deadline = args.duration.map(|d| args.start + d);

    if is_tty {
        out.extend_from_slice(&clear);
    }
    stdout.write_all(&out)?;
    out.clear();
    stdout.flush()?;

    loop {
        // ---- input ------------------------------------------------------
        while is_tty && event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(k) => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            quit.store(true, Ordering::Relaxed);
                        }
                        KeyCode::Char('c') if ctrl => {
                            quit.store(true, Ordering::Relaxed);
                        }
                        KeyCode::Char(' ') => {
                            paused = !paused;
                            clock.set_paused(paused);
                            if let Some(a) = &audio_out {
                                let _ = if paused { a.stream.pause() } else { a.stream.play() };
                            }
                        }
                        KeyCode::Char('s') => {
                            show_stats = !show_stats;
                            renderer.invalidate();
                        }
                        KeyCode::Char('m') => {
                            if let Some(a) = &audio_out {
                                let v = a.muted.load(Ordering::Relaxed);
                                a.muted.store(!v, Ordering::Relaxed);
                            }
                        }
                        KeyCode::Up | KeyCode::Down => {
                            if let Some(a) = &audio_out {
                                let cur = f32::from_bits(a.volume.load(Ordering::Relaxed));
                                let d = if k.code == KeyCode::Up { 0.1 } else { -0.1 };
                                a.volume.store((cur + d).clamp(0.0, 2.0).to_bits(),
                                               Ordering::Relaxed);
                            }
                        }
                        KeyCode::Left | KeyCode::Right | KeyCode::PageUp | KeyCode::PageDown => {
                            let d = match k.code {
                                KeyCode::Left => -5.0,
                                KeyCode::Right => 5.0,
                                KeyCode::PageDown => -60.0,
                                _ => 60.0,
                            };
                            let dur = if info.duration > 0.0 { info.duration } else { f64::MAX };
                            let to = (clock.now() + d).clamp(0.0, (dur - 0.5).max(0.0));
                            generation.fetch_add(1, Ordering::Relaxed);
                            if let Some(q) = &audio_q {
                                q.lock().unwrap().clear();
                            }
                            // provisional, so the overlay is sane in the gap;
                            // the media thread re-bases to the true landing PTS
                            clock.reset(to);
                            rebase.store(true, Ordering::Relaxed);
                            pending = None;
                            let _ = ctx_tx.send(media::Cmd::Seek(to));
                            renderer.invalidate();
                        }
                        _ => {}
                    }
                }
                Event::Resize(w, h) => {
                    cols = args.cols.map(|c| w.min(c)).unwrap_or(w);
                    rows = h;
                    if info.has_video {
                        target = fit(info.width, info.height, cols, rows, !use_ascii);
                        let _ = ctx_tx.send(media::Cmd::Resize(target.0, target.1));
                    }
                    renderer.invalidate();
                    out.extend_from_slice(&clear);
                }
                _ => {}
            }
        }

        if quit.load(Ordering::Relaxed) {
            break;
        }
        if let Some(d) = deadline {
            if clock.now() >= d {
                break;
            }
        }
        if paused {
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }

        // ---- present ----------------------------------------------------
        if !info.has_video {
            let empty = audio_q.as_ref().map(|q| q.lock().unwrap().is_empty()).unwrap_or(true);
            if eof.load(Ordering::Relaxed) && empty {
                break;
            }
            draw_status(&mut out, &info, clock.now(), paused, &audio_out, 1, 1);
            stdout.write_all(&out)?;
            out.clear();
            stdout.flush()?;
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        if pending.is_none() {
            pending = vrx.try_recv().ok();
        }
        let Some(frame) = pending.take() else {
            if eof.load(Ordering::Relaxed) {
                let empty = audio_q.as_ref().map(|q| q.lock().unwrap().is_empty()).unwrap_or(true);
                if empty {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };

        // frames produced before the last seek are stale
        if frame.gen != generation.load(Ordering::Relaxed) {
            continue;
        }

        let drift = frame.pts - clock.now();
        let dt = drift;
        if dt > 0.002 {
            pending = Some(frame);
            std::thread::sleep(Duration::from_secs_f64(dt.min(0.005)));
            continue;
        }
        if dt < -frame_interval {
            dropped += 1;
            continue;
        }

        let t_draw = Instant::now();
        origin = (
            1 + (cols.saturating_sub(frame.w as u16)) / 2,
            1 + (rows.saturating_sub(if use_ascii { frame.h as u16 } else { (frame.h / 2) as u16 }))
                / 2,
        );
        renderer.set_origin(origin.0, origin.1);
        renderer.draw(&frame.data, frame.w, frame.h, &mut out);
        let draw_ms = t_draw.elapsed().as_secs_f64() * 1000.0;
        shown += 1;
        if args.panic_after == Some(shown) {
            panic!("deliberate panic after {shown} frames (--panic-after)");
        }

        if show_stats {
            draw_status(&mut out, &info, clock.now(), paused, &audio_out, 1, 1);
            let stat_line = frame.pts - clock.now();
            let _ = write!(
                out,
                "\x1b[2;1H\x1b[0m drift {:+6.1}ms drop {} shown {} draw {:.2}ms write {:.2}ms dec {:.2}ms {}x{} ",
                stat_line * 1000.0,
                dropped,
                shown,
                draw_ms,
                write_ms,
                decode_us.load(Ordering::Relaxed) as f64 / 1000.0,
                frame.w,
                frame.h
            );
            renderer.invalidate_row(1);
            renderer.invalidate_row(2);
        }
        out.extend_from_slice(b"\x1b[0m");
        let nbytes = out.len();
        let t_write = Instant::now();
        stdout.write_all(&out)?;
        out.clear();
        stdout.flush()?;
        write_ms = t_write.elapsed().as_secs_f64() * 1000.0;

        let decode_ms = decode_us.load(Ordering::Relaxed) as f64 / 1000.0;
        if let Some(f) = log.as_mut() {
            let _ = writeln!(
                f,
                "{:.3},{:.3},{:+.2},{:.3},{:.3},{:.3},{},{},{},{},{}",
                started.elapsed().as_secs_f64(),
                frame.pts,
                drift * 1000.0,
                draw_ms,
                write_ms,
                decode_ms,
                nbytes,
                shown,
                dropped,
                frame.w,
                frame.h
            );
            samples.push((draw_ms, write_ms, decode_ms, nbytes));
        }
    }

    quit.store(true, Ordering::Relaxed);
    let _ = ctx_tx.send(media::Cmd::Quit);
    if let Some(a) = &audio_out {
        let _ = a.stream.pause();
    }
    drop(vrx);
    let _ = handle.join();
    let _ = origin;

    if let Some(f) = log.as_mut() {
        let _ = f.flush();
    }
    if !samples.is_empty() {
        // Leave the alternate screen first so the summary lands in the
        // scrollback the user can actually read.
        drop(_guard);
        let elapsed = started.elapsed().as_secs_f64();
        let pct = |mut v: Vec<f64>, q: f64| -> f64 {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[((v.len() - 1) as f64 * q) as usize]
        };
        let col = |i: usize| -> Vec<f64> {
            samples
                .iter()
                .map(|s| match i {
                    0 => s.0,
                    1 => s.1,
                    _ => s.2,
                })
                .collect()
        };
        let total: usize = samples.iter().map(|s| s.3).sum();
        eprintln!(
            "\ntermcast: {} frames shown, {} dropped, {:.1}s wall",
            shown, dropped, elapsed
        );
        for (name, i) in [("draw  ", 0), ("write ", 1), ("decode", 2)] {
            eprintln!(
                "  {}  p50 {:6.2}ms  p95 {:6.2}ms  max {:6.2}ms",
                name,
                pct(col(i), 0.50),
                pct(col(i), 0.95),
                pct(col(i), 1.0)
            );
        }
        eprintln!(
            "  output  {:.1} MB total, {:.1} MB/s",
            total as f64 / 1e6,
            total as f64 / 1e6 / elapsed.max(0.001)
        );
    }
    Ok(())
}

fn draw_status(
    out: &mut Vec<u8>,
    info: &media::Info,
    now: f64,
    paused: bool,
    audio: &Option<audio::AudioOut>,
    row: u16,
    col: u16,
) {
    let vol = audio
        .as_ref()
        .map(|a| {
            if a.muted.load(Ordering::Relaxed) {
                "muted".to_string()
            } else {
                format!("{:.0}%", f32::from_bits(a.volume.load(Ordering::Relaxed)) * 100.0)
            }
        })
        .unwrap_or_else(|| "no audio".into());
    let _ = write!(
        out,
        "\x1b[{row};{col}H\x1b[0m {} {:>8} / {:<8} {} {} ",
        if paused { "||" } else { " >" },
        fmt_time(now),
        fmt_time(info.duration),
        vol,
        info.container
    );
}

fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "--:--".into();
    }
    let s = t as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}
