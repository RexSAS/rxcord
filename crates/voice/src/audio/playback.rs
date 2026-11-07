//! Decode, mix and play everyone's voice through one cpal output stream.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::AudioError;
use super::jitter::{Jitter, Popped};
use super::mixer;
use super::tone::Ring;

const RATE: u32 = 48_000;
/// 10 ms of 48 kHz stereo: what one `render` produces.
const CHUNK: usize = 960;
/// Largest Opus frame (120 ms) in stereo samples.
const MAX_FRAME: usize = 5760 * 2;
/// One 20 ms stereo frame: the PLC length.
const FRAME: usize = 1920;
/// Consecutive missing frames concealed with Opus PLC; silence after.
const PLC_FRAMES: u8 = 2;
/// A user unseen this long loses its decoder and jitter buffer.
const IDLE: Duration = Duration::from_secs(5);
/// "Speaking" = a decoded frame above `SPEAKING_RMS` within this window.
const SPEAKING_HOLD: Duration = Duration::from_millis(200);
const SPEAKING_RMS: f32 = 0.01;

/// Output for everyone in the call; dropping it closes the device.
pub struct Playback {
    engine: Engine,
    /// `None` when the device runs at 48 kHz.
    resampler: Option<Resampler<2>>,
    channels: usize,
    ring: rtrb::Producer<f32>,
    /// Device samples kept queued ahead of the callback, at least (40 ms).
    min_target: usize,
    /// Samples the device asked for in its last callback.
    period: Arc<AtomicUsize>,
    resampled: Vec<f32>,
    failed: Arc<AtomicBool>,
    _stream: cpal::Stream,
}

impl Playback {
    /// Open `device` (by name from `devices::outputs`), or the default output if it's unset or gone.
    pub fn start(device: Option<String>) -> Result<Playback, AudioError> {
        let (device, config) = output(device)?;
        let channels = config.channels as usize;
        let (ring, mut consumer) = rtrb::RingBuffer::new(config.sample_rate as usize * channels * 2 / 5); // 400 ms
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let period = Arc::new(AtomicUsize::new(0));
        let last = period.clone();
        let stream = device.build_output_stream(
            config,
            // real-time: no locks, no allocation; underrun plays silence
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                last.store(data.len(), Ordering::Relaxed);
                for s in data {
                    *s = consumer.pop().unwrap_or(0.0);
                }
            },
            move |_| flag.store(true, Ordering::Relaxed),
            None,
        )?;
        stream.play()?;
        tracing::debug!(rate = config.sample_rate, channels, "audio output open");
        Ok(Playback {
            engine: Engine::default(),
            resampler: (config.sample_rate != RATE).then(|| Resampler::new(RATE, config.sample_rate)),
            channels,
            ring,
            min_target: config.sample_rate as usize * channels / 25, // 40 ms
            period,
            resampled: Vec::new(),
            failed,
            _stream: stream,
        })
    }

    pub fn push(&mut self, user_id: u64, seq: u16, opus: Vec<u8>) {
        self.engine.push(user_id, seq, opus, Instant::now());
    }

    /// Per-user volume, `0.0..=2.0` (0 = muted for me).
    pub fn set_gain(&mut self, user_id: u64, gain: f32) {
        self.engine.set_gain(user_id, gain);
    }

    pub fn set_deafened(&mut self, deafened: bool) {
        self.engine.set_deafened(deafened);
    }

    /// Mix the incoming-call ring into the output.
    pub fn set_ringing(&mut self, on: bool) {
        self.engine.set_ringing(on);
    }

    /// Who is speaking right now, for the UI.
    pub fn levels(&self) -> Vec<(u64, bool)> {
        self.engine.levels(Instant::now())
    }

    /// The device reported an error (e.g. unplugged): drop this and start again.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Called every 10 ms: top the device queue up to `target` (the device clock paces decoding).
    pub fn tick(&mut self) {
        let now = Instant::now();
        let mut chunk = [0.0; CHUNK];
        let capacity = self.ring.buffer().capacity();
        let target = fill_target(self.min_target, self.period.load(Ordering::Relaxed), capacity);
        while capacity - self.ring.slots() < target {
            self.engine.render(now, &mut chunk);
            let stereo: &[f32] = match &mut self.resampler {
                Some(r) => {
                    self.resampled.clear();
                    r.process(&chunk, &mut self.resampled);
                    &self.resampled
                }
                None => &chunk,
            };
            for f in stereo.chunks_exact(2) {
                for c in 0..self.channels {
                    let s = match (self.channels, c) {
                        (1, _) => (f[0] + f[1]) / 2.0,
                        (_, 0 | 1) => f[c],
                        _ => 0.0,
                    };
                    let _ = self.ring.push(s);
                }
            }
        }
    }
}

/// `device` (by name from `devices::outputs`), or the default output if it's unset or gone, with a
/// 48 kHz stereo f32 config when it has one.
pub(crate) fn output(device: Option<String>) -> Result<(cpal::Device, cpal::StreamConfig), AudioError> {
    let host = cpal::default_host();
    let named = device.and_then(|name| {
        let found = host.output_devices().ok()?.find(|d| d.to_string() == name);
        if found.is_none() {
            tracing::warn!("output device {name:?} not found, using the default");
        }
        found
    });
    let device = named.or_else(|| host.default_output_device()).ok_or(AudioError::NoDevice)?;
    let config = device
        .supported_output_configs()?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32 && c.channels() == 2)
        .find_map(|c| c.try_with_sample_rate(RATE));
    // shortcut: the fallback assumes the host accepts f32 at its default config (true for WASAPI shared, ALSA plug, Pulse/PipeWire)
    let config = match config {
        Some(c) => c.config(),
        None => device.default_output_config()?.config(),
    };
    Ok((device, config))
}

/// Queue depth to keep: two device periods (a device with large periods drains a 40 ms queue at once),
/// at least `min`, at most half the ring so a top-up always fits.
fn fill_target(min: usize, period: usize, capacity: usize) -> usize {
    min.max(2 * period).min(capacity / 2)
}

/// Linear resampler between two rates, `C` interleaved channels.
pub(crate) struct Resampler<const C: usize> {
    /// Input frames per output frame.
    step: f64,
    /// Position in the current input, where 0 is the last frame of the previous one.
    pos: f64,
    prev: [f32; C],
}

impl<const C: usize> Resampler<C> {
    pub(crate) fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, prev: [0.0; C] }
    }

    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / C;
        let prev = self.prev;
        let at = |i: usize, c: usize| if i == 0 { prev[c] } else { input[(i - 1) * C + c] };
        while self.pos < frames as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..C {
                out.push(at(i, c) + (at(i + 1, c) - at(i, c)) * t);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev = std::array::from_fn(|c| input[(frames - 1) * C + c]);
        }
    }
}

struct User {
    jitter: Jitter,
    decoder: opus::Decoder,
    /// Decoded stereo samples not yet mixed.
    pcm: Vec<f32>,
    lost: u8,
    last_seen: Instant,
    last_loud: Option<Instant>,
}

impl User {
    /// Append the next frame (decoded, concealed or silent) to `pcm`; false when there's nothing to play.
    fn refill(&mut self, now: Instant) -> bool {
        let mut buf = [0.0; MAX_FRAME];
        let decoded = match self.jitter.pop() {
            Popped::Empty => return false,
            Popped::Frame(opus) => {
                self.lost = 0;
                self.decoder.decode_float(&opus, &mut buf, false)
            }
            Popped::Lost if self.lost < PLC_FRAMES => {
                self.lost += 1;
                self.decoder.decode_float(&[], &mut buf[..FRAME], false)
            }
            Popped::Lost => Ok(0),
        };
        let n = decoded.unwrap_or_else(|e| {
            tracing::debug!("opus decode: {e}");
            0
        }) * 2;
        if n == 0 {
            self.pcm.resize(self.pcm.len() + FRAME, 0.0);
            return true;
        }
        let pcm = &buf[..n];
        if self.lost == 0 && rms(pcm) > SPEAKING_RMS {
            self.last_loud = Some(now);
        }
        self.pcm.extend_from_slice(pcm);
        true
    }
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

/// Everything `tick` does except talking to the device.
#[derive(Default)]
pub(crate) struct Engine {
    users: HashMap<u64, User>,
    gains: HashMap<u64, f32>,
    deafened: bool,
    /// The incoming-call ring, while ringing.
    ring: Option<Ring>,
}

impl Engine {
    pub(crate) fn push(&mut self, user_id: u64, seq: u16, opus: Vec<u8>, now: Instant) {
        let user = match self.users.entry(user_id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => match opus::Decoder::new(RATE, opus::Channels::Stereo) {
                Ok(decoder) => e.insert(User {
                    jitter: Jitter::default(),
                    decoder,
                    pcm: Vec::new(),
                    lost: 0,
                    last_seen: now,
                    last_loud: None,
                }),
                Err(err) => {
                    tracing::warn!("opus decoder: {err}");
                    return;
                }
            },
        };
        user.last_seen = now;
        user.jitter.push(seq, opus);
    }

    pub(crate) fn set_gain(&mut self, user_id: u64, gain: f32) {
        self.gains.insert(user_id, if gain.is_nan() { 1.0 } else { gain.clamp(0.0, 2.0) });
    }

    pub(crate) fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
    }

    pub(crate) fn set_ringing(&mut self, on: bool) {
        if on != self.ring.is_some() {
            self.ring = on.then(|| Ring::new(RATE));
        }
    }

    /// Mix the next 10 ms (`CHUNK` stereo samples at 48 kHz) into `out`.
    pub(crate) fn render(&mut self, now: Instant, out: &mut [f32]) {
        self.users.retain(|_, u| now.duration_since(u.last_seen) < IDLE);
        for u in self.users.values_mut() {
            while u.pcm.len() < CHUNK && u.refill(now) {}
            if !u.pcm.is_empty() && u.pcm.len() < CHUNK {
                u.pcm.resize(CHUNK, 0.0);
            }
        }
        // still decoded while deafened, so the UI keeps showing who speaks
        let sources: Vec<(&[f32], f32)> = self
            .users
            .iter()
            .filter(|(_, u)| !u.pcm.is_empty())
            .map(|(id, u)| {
                let gain = if self.deafened { 0.0 } else { self.gains.get(id).copied().unwrap_or(1.0) };
                (&u.pcm[..CHUNK], gain)
            })
            .collect();
        mixer::mix(out, &sources);
        if let Some(r) = &mut self.ring {
            for f in out.chunks_exact_mut(2) {
                let s = r.next_sample();
                f.iter_mut().for_each(|o| *o = (*o + s).clamp(-1.0, 1.0));
            }
        }
        for u in self.users.values_mut() {
            let n = u.pcm.len().min(CHUNK);
            u.pcm.drain(..n);
        }
    }

    pub(crate) fn levels(&self, now: Instant) -> Vec<(u64, bool)> {
        let mut levels: Vec<_> = self
            .users
            .iter()
            .map(|(id, u)| (*id, u.last_loud.is_some_and(|t| now.duration_since(t) < SPEAKING_HOLD)))
            .collect();
        levels.sort_unstable();
        levels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tone(frames: usize) -> Vec<Vec<u8>> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip).unwrap();
        (0..frames)
            .map(|f| {
                let pcm: Vec<f32> = (0..960 * 2)
                    .map(|i| {
                        let t = (f * 960 + i / 2) as f32 / 48_000.0;
                        0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    })
                    .collect();
                enc.encode_vec_float(&pcm, 4000).unwrap()
            })
            .collect()
    }

    #[test]
    fn resample_48k_to_44k_length() {
        let mut r = Resampler::<2>::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&vec![0.1; 960 * 2], &mut out);
        assert!((out.len() as i64 / 2 - 882).abs() <= 1, "{} frames", out.len() / 2);
        out.clear();
        for _ in 0..100 {
            r.process(&vec![0.1; 480 * 2], &mut out);
        }
        assert!((out.len() as i64 / 2 - 44_100).abs() <= 1, "{} frames over 1 s", out.len() / 2);
    }

    #[test]
    fn resample_44k_mono_to_48k_length() {
        let mut r = Resampler::<1>::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.1; 441], &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 1, "{} samples over 1 s", out.len());
        assert!(out.iter().skip(2).all(|s| (s - 0.1).abs() < 1e-6), "a constant stays constant");
    }

    #[test]
    fn decode_round_trip() {
        let mut e = Engine::default();
        let now = Instant::now();
        for (seq, f) in tone(10).into_iter().enumerate() {
            e.push(7, seq as u16, f, now);
        }
        let mut out = Vec::new();
        let mut chunk = [0.0; CHUNK];
        for _ in 0..20 {
            e.render(now, &mut chunk);
            out.extend_from_slice(&chunk);
        }
        let expected = 0.5 / 2f32.sqrt();
        let got = rms(&out[out.len() / 2..]); // skip codec warm-up
        assert!((got - expected).abs() < expected * 0.2, "rms {got} vs {expected}");
        assert_eq!(e.levels(now), vec![(7, true)]);
    }

    #[test]
    fn gain_deafen_levels_and_idle_drop() {
        let mut e = Engine::default();
        let t0 = Instant::now();
        for (seq, f) in tone(6).into_iter().enumerate() {
            e.push(7, seq as u16, f, t0);
        }
        e.set_gain(7, 5.0); // clamped to 2.0
        let mut chunk = [0.0; CHUNK];
        for _ in 0..6 {
            e.render(t0, &mut chunk);
        }
        assert!(rms(&chunk) > 0.5, "gain 2.0 applied");
        e.set_deafened(true);
        e.render(t0, &mut chunk);
        assert_eq!(rms(&chunk), 0.0);
        assert_eq!(e.levels(t0), vec![(7, true)], "still shown speaking while deafened");
        assert_eq!(e.levels(t0 + Duration::from_millis(300)), vec![(7, false)]);
        e.render(t0 + Duration::from_secs(6), &mut chunk);
        assert!(e.levels(t0 + Duration::from_secs(6)).is_empty(), "unseen 5 s: dropped");
    }

    #[test]
    fn ring_mixes_into_the_call() {
        let (mut e, now) = (Engine::default(), Instant::now());
        let mut chunk = [0.0; CHUNK];
        e.set_ringing(true);
        e.set_deafened(true); // the ring isn't a call voice: it still sounds
        e.render(now, &mut chunk);
        assert!(rms(&chunk) > 0.01, "ringing");
        assert_eq!(chunk[0..CHUNK].chunks(2).filter(|f| f[0] != f[1]).count(), 0, "same on both channels");
        e.set_ringing(false);
        e.render(now, &mut chunk);
        assert_eq!(rms(&chunk), 0.0, "stopped");
    }

    #[test]
    fn nan_gain_is_unity() {
        let mut e = Engine::default();
        e.set_gain(7, f32::NAN);
        assert_eq!(e.gains[&7], 1.0);
    }

    #[test]
    fn undecodable_frames_play_silence() {
        let mut e = Engine::default();
        let now = Instant::now();
        for seq in 0..6 {
            e.push(7, seq, vec![0xff; 3], now);
        }
        let mut chunk = [1.0; CHUNK];
        for _ in 0..8 {
            e.render(now, &mut chunk);
            assert_eq!(rms(&chunk), 0.0);
        }
        assert_eq!(e.levels(now), vec![(7, false)]);
    }

    #[test]
    fn fill_target_covers_two_device_periods() {
        let (min, cap) = (3840, 38400); // 40 ms and 400 ms of 48 kHz stereo
        assert_eq!(fill_target(min, 0, cap), min, "before the first callback");
        assert_eq!(fill_target(min, 512, cap), min, "small period: 40 ms floor");
        assert_eq!(fill_target(min, 4800, cap), 9600, "50 ms period: 100 ms queued");
        assert_eq!(fill_target(min, 30000, cap), cap / 2, "never more than the ring can hold");
    }
}
