//! Demuxing and decoding.
//!
//! One thread owns the ffmpeg Input and both decoders. Sharing libav contexts
//! across threads is awkward and buys nothing here: libdav1d and the h264
//! decoder are internally threaded, so the expensive work already parallelises.

use anyhow::{anyhow, Result};
use ffmpeg_next as ff;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct VideoFrame {
    pub pts: f64,
    pub w: u32,
    pub h: u32,
    pub gen: u64,
    pub data: Vec<u8>,
}

pub enum Cmd {
    Seek(f64),
    Resize(u32, u32),
    Quit,
}

#[derive(Clone)]
pub struct Info {
    pub duration: f64,
    pub container: String,
    pub has_video: bool,
    pub has_audio: bool,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub acodec: String,
}

pub fn probe(path: &str) -> Result<Info> {
    let ictx = ff::format::input(&path)?;
    let mut info = Info {
        duration: ictx.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE),
        container: ictx.format().name().to_string(),
        has_video: false,
        has_audio: false,
        width: 0,
        height: 0,
        fps: 25.0,
        vcodec: String::new(),
        acodec: String::new(),
    };
    if let Some(s) = ictx.streams().best(ff::media::Type::Video) {
        let ctx = ff::codec::context::Context::from_parameters(s.parameters())?;
        let id = ctx.id();
        let v = ctx.decoder().video()?;
        info.has_video = true;
        info.width = v.width();
        info.height = v.height();
        info.vcodec = format!("{id:?}");
        let r = s.avg_frame_rate();
        if r.denominator() != 0 {
            let f = r.numerator() as f64 / r.denominator() as f64;
            if f > 1.0 && f < 1000.0 {
                info.fps = f;
            }
        }
    }
    if let Some(s) = ictx.streams().best(ff::media::Type::Audio) {
        let ctx = ff::codec::context::Context::from_parameters(s.parameters())?;
        let id = ctx.id();
        info.has_audio = true;
        info.acodec = format!("{id:?}");
    }
    if info.duration <= 0.0 {
        info.duration = 0.0;
    }
    Ok(info)
}

/// Copy a scaled RGB frame out of ffmpeg's padded buffer into a tight Vec.
fn pack_rgb(f: &ff::frame::Video) -> Vec<u8> {
    let w = f.width() as usize;
    let h = f.height() as usize;
    let stride = f.stride(0);
    let src = f.data(0);
    let row = w * 3;
    let mut out = Vec::with_capacity(row * h);
    for y in 0..h {
        let s = y * stride;
        if s + row <= src.len() {
            out.extend_from_slice(&src[s..s + row]);
        } else {
            out.resize(row * (y + 1), 0);
        }
    }
    out
}

pub struct Shared {
    pub quit: Arc<AtomicBool>,
    pub eof: Arc<AtomicBool>,
    pub generation: Arc<AtomicU64>,
    /// Set after a seek. A seek lands on the nearest keyframe at or before the
    /// requested time, so the requested position is a guess; the first decoded
    /// packet afterwards carries the truth and re-bases the clock to it.
    /// Without this every frame looks late and gets dropped for good.
    pub rebase: Arc<AtomicBool>,
    /// Microseconds spent in decode + swscale for the last video frame, so the
    /// media thread's CPU cost can be told apart from the render loop's.
    pub decode_us: Arc<AtomicU64>,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    path: String,
    mut target: (u32, u32),
    start: f64,
    vtx: SyncSender<VideoFrame>,
    audio_q: Option<Arc<Mutex<VecDeque<f32>>>>,
    out_rate: u32,
    out_ch: u16,
    cmds: Receiver<Cmd>,
    shared: Shared,
    clock: Arc<dyn crate::audio::ClockSource>,
) -> std::thread::JoinHandle<Result<()>> {
    std::thread::spawn(move || -> Result<()> {
        let mut ictx = ff::format::input(&path)?;

        let vstream = ictx.streams().best(ff::media::Type::Video).map(|s| s.index());
        let astream = ictx.streams().best(ff::media::Type::Audio).map(|s| s.index());
        let vtb = vstream
            .map(|i| {
                let tb = ictx.stream(i).unwrap().time_base();
                tb.numerator() as f64 / tb.denominator() as f64
            })
            .unwrap_or(0.0);
        let atb = astream
            .map(|i| {
                let tb = ictx.stream(i).unwrap().time_base();
                tb.numerator() as f64 / tb.denominator() as f64
            })
            .unwrap_or(0.0);

        let mut vdec = match vstream {
            Some(i) => Some(
                ff::codec::context::Context::from_parameters(ictx.stream(i).unwrap().parameters())?
                    .decoder()
                    .video()?,
            ),
            None => None,
        };
        let mut adec = match astream {
            Some(i) => Some(
                ff::codec::context::Context::from_parameters(ictx.stream(i).unwrap().parameters())?
                    .decoder()
                    .audio()?,
            ),
            None => None,
        };

        let mut scaler: Option<ff::software::scaling::Context> = None;
        let mut scaler_key = (0u32, 0u32, 0u32, 0u32);

        let mut resampler = match (&adec, &audio_q) {
            (Some(a), Some(_)) => Some(ff::software::resampling::Context::get(
                a.format(),
                a.channel_layout(),
                a.rate(),
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                ff::ChannelLayout::default(out_ch as i32),
                out_rate,
            )?),
            _ => None,
        };

        if start > 0.0 {
            let ts = (start * 1e6) as i64;
            let _ = ictx.seek(ts, ..ts);
        }

        // roughly one second of audio; also the backpressure valve for the whole
        // pipeline, since a paused device stops draining it
        let cap = (out_rate as usize) * (out_ch as usize);

        // std's Receiver cannot be peeked, and the inner retry loops must not
        // swallow commands, so drained commands land here until the top of the
        // outer loop can act on them.
        let mut cmdq: Vec<Cmd> = Vec::new();

        'outer: loop {
            // --- commands -------------------------------------------------
            while let Ok(cmd) = cmds.try_recv() {
                cmdq.push(cmd);
            }
            for cmd in std::mem::take(&mut cmdq) {
                match cmd {
                    Cmd::Quit => break 'outer,
                    Cmd::Resize(w, h) => target = (w, h),
                    Cmd::Seek(to) => {
                        let ts = (to.max(0.0) * 1e6) as i64;
                        let _ = ictx.seek(ts, ..ts);
                        // Without flushing, the decoders emit frames from the
                        // old position after the seek.
                        if let Some(d) = vdec.as_mut() {
                            d.flush();
                        }
                        if let Some(d) = adec.as_mut() {
                            d.flush();
                        }
                        if let Some(q) = &audio_q {
                            q.lock().unwrap().clear();
                        }
                        shared.eof.store(false, Ordering::Relaxed);
                        shared.rebase.store(true, Ordering::Relaxed);
                    }
                }
            }
            if shared.quit.load(Ordering::Relaxed) {
                break;
            }

            // --- one packet ------------------------------------------------
            let item = {
                let mut it = ictx.packets();
                it.next().map(|(s, p)| (s.index(), p))
            };
            let (idx, packet) = match item {
                Some(v) => v,
                None => {
                    shared.eof.store(true, Ordering::Relaxed);
                    // stay alive so seeking backwards still works
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                }
            };

            // --- video ------------------------------------------------------
            if Some(idx) == vstream {
                if let Some(dec) = vdec.as_mut() {
                    if dec.send_packet(&packet).is_err() {
                        continue;
                    }
                    let mut decoded = ff::frame::Video::empty();
                    while dec.receive_frame(&mut decoded).is_ok() {
                        let t_dec = std::time::Instant::now();
                        let key = (decoded.width(), decoded.height(), target.0, target.1);
                        if scaler.is_none() || scaler_key != key {
                            scaler = Some(ff::software::scaling::Context::get(
                                decoded.format(),
                                decoded.width(),
                                decoded.height(),
                                ff::format::Pixel::RGB24,
                                target.0.max(1),
                                target.1.max(1),
                                ff::software::scaling::Flags::BILINEAR,
                            )?);
                            scaler_key = key;
                        }
                        let mut rgb = ff::frame::Video::empty();
                        if scaler.as_mut().unwrap().run(&decoded, &mut rgb).is_err() {
                            continue;
                        }
                        let pts = decoded.pts().unwrap_or(0) as f64 * vtb;
                        if astream.is_none() && shared.rebase.swap(false, Ordering::Relaxed) {
                            clock.reset(pts);
                        }
                        shared
                            .decode_us
                            .store(t_dec.elapsed().as_micros() as u64, Ordering::Relaxed);
                        let mut frame = VideoFrame {
                            pts,
                            w: rgb.width(),
                            h: rgb.height(),
                            gen: shared.generation.load(Ordering::Relaxed),
                            data: pack_rgb(&rgb),
                        };
                        // bounded send, but stay responsive to quit/seek
                        loop {
                            match vtx.try_send(frame) {
                                Ok(()) => break,
                                Err(TrySendError::Full(f)) => {
                                    frame = f;
                                    if shared.quit.load(Ordering::Relaxed) {
                                        break 'outer;
                                    }
                                    while let Ok(c) = cmds.try_recv() {
                                        cmdq.push(c);
                                    }
                                    if !cmdq.is_empty() {
                                        break;
                                    }
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                                Err(TrySendError::Disconnected(_)) => break 'outer,
                            }
                        }
                    }
                }
            // --- audio ------------------------------------------------------
            } else if Some(idx) == astream {
                let (Some(dec), Some(rs), Some(q)) =
                    (adec.as_mut(), resampler.as_mut(), audio_q.as_ref())
                else {
                    continue;
                };
                if dec.send_packet(&packet).is_err() {
                    continue;
                }
                let mut decoded = ff::frame::Audio::empty();
                while dec.receive_frame(&mut decoded).is_ok() {
                    let apts = decoded.pts().unwrap_or(0) as f64 * atb;
                    if shared.rebase.swap(false, Ordering::Relaxed) {
                        clock.reset(apts);
                    }
                    let mut out = ff::frame::Audio::empty();
                    if rs.run(&decoded, &mut out).is_err() {
                        continue;
                    }
                    let n = out.samples() * out_ch as usize;
                    let bytes = out.data(0);
                    let n = n.min(bytes.len() / 4);
                    loop {
                        {
                            let mut g = q.lock().unwrap();
                            if g.len() < cap {
                                for c in bytes[..n * 4].chunks_exact(4) {
                                    g.push_back(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                                }
                                break;
                            }
                        }
                        if shared.quit.load(Ordering::Relaxed) {
                            break 'outer;
                        }
                        while let Ok(c) = cmds.try_recv() {
                            cmdq.push(c);
                        }
                        if !cmdq.is_empty() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
        Ok(())
    })
}

pub fn init() -> Result<()> {
    ff::init().map_err(|e| anyhow!("ffmpeg init failed: {e}"))
}
