//! Audio output and the master clock.
//!
//! Video is presented against the audio clock rather than a wall timer: audio
//! cannot be dropped without an audible artefact, so it sets the pace and video
//! is fitted around it.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub trait ClockSource: Send + Sync {
    fn now(&self) -> f64;
    fn reset(&self, to: f64);
    fn set_paused(&self, paused: bool);
}

/// Position derived from how many samples the device has actually consumed,
/// less the device's own output latency so the clock reflects what is audible
/// right now rather than what has merely been handed over.
pub struct AudioClock {
    pub base_us: AtomicU64,
    pub samples: AtomicU64,
    pub latency_us: AtomicU64,
    pub rate: u32,
}

impl ClockSource for AudioClock {
    fn now(&self) -> f64 {
        let base = self.base_us.load(Ordering::Relaxed) as f64 / 1e6;
        let played = self.samples.load(Ordering::Relaxed) as f64 / self.rate as f64;
        let lat = self.latency_us.load(Ordering::Relaxed) as f64 / 1e6;
        (base + played - lat).max(0.0)
    }
    fn reset(&self, to: f64) {
        self.samples.store(0, Ordering::Relaxed);
        self.base_us.store((to.max(0.0) * 1e6) as u64, Ordering::Relaxed);
    }
    // Pausing the cpal stream stops the callbacks, which stops the clock.
    fn set_paused(&self, _paused: bool) {}
}

/// Fallback for files with no audio track.
pub struct WallClock {
    inner: Mutex<(f64, Option<Instant>)>,
}

impl WallClock {
    pub fn new() -> Self {
        Self { inner: Mutex::new((0.0, Some(Instant::now()))) }
    }
}

impl ClockSource for WallClock {
    fn now(&self) -> f64 {
        let g = self.inner.lock().unwrap();
        g.0 + g.1.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
    }
    fn reset(&self, to: f64) {
        let mut g = self.inner.lock().unwrap();
        let running = g.1.is_some();
        *g = (to, if running { Some(Instant::now()) } else { None });
    }
    fn set_paused(&self, paused: bool) {
        let mut g = self.inner.lock().unwrap();
        if paused {
            if let Some(t) = g.1.take() {
                g.0 += t.elapsed().as_secs_f64();
            }
        } else if g.1.is_none() {
            g.1 = Some(Instant::now());
        }
    }
}

pub struct AudioOut {
    pub stream: cpal::Stream,
    pub queue: Arc<Mutex<VecDeque<f32>>>,
    pub clock: Arc<AudioClock>,
    pub volume: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub rate: u32,
    pub channels: u16,
}

pub fn open() -> Result<AudioOut> {
    let host = cpal::default_host();
    let dev = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))?;
    let supported = dev.default_output_config()?;
    if supported.sample_format() != cpal::SampleFormat::F32 {
        return Err(anyhow!(
            "device sample format {:?} is not supported (only f32)",
            supported.sample_format()
        ));
    }
    let rate = supported.sample_rate();
    let channels = supported.channels();
    let cfg: cpal::StreamConfig = supported.into();

    let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let clock = Arc::new(AudioClock {
        base_us: AtomicU64::new(0),
        samples: AtomicU64::new(0),
        latency_us: AtomicU64::new(0),
        rate,
    });
    let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let muted = Arc::new(AtomicBool::new(false));

    let (q, c, v, m) = (queue.clone(), clock.clone(), volume.clone(), muted.clone());
    let ch = channels as usize;
    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let gain = if m.load(Ordering::Relaxed) {
                0.0
            } else {
                f32::from_bits(v.load(Ordering::Relaxed))
            };
            let mut popped = 0usize;
            {
                let mut q = q.lock().unwrap();
                for s in data.iter_mut() {
                    match q.pop_front() {
                        Some(x) => {
                            *s = x * gain;
                            popped += 1;
                        }
                        // Underrun: emit silence but do NOT advance the clock,
                        // or video would race ahead of the audio it is tracking.
                        None => *s = 0.0,
                    }
                }
            }
            if popped > 0 {
                c.samples.fetch_add((popped / ch) as u64, Ordering::Relaxed);
            }
            // How far ahead of the callback the samples will actually be
            // heard; subtracting it keeps the clock aligned with what the
            // listener perceives rather than what we just handed over.
            let (playback, callback) = (info.timestamp().playback, info.timestamp().callback);
            let d = playback.duration_since(callback);
            c.latency_us.store(d.as_micros() as u64, Ordering::Relaxed);
        },
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?;

    Ok(AudioOut { stream, queue, clock, volume, muted, rate, channels })
}
