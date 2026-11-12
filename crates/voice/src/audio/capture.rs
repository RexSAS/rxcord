//! Microphone → RNNoise → voice activity → Opus, through one cpal input stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use nnnoiseless::DenoiseState;

use super::AudioError;
use super::playback::Resampler;
use super::vad::{Gate, GateOut};

const RATE: u32 = 48_000;
/// One RNNoise frame: 10 ms mono.
const DENOISE: usize = DenoiseState::FRAME_SIZE;
/// One Opus frame: 20 ms mono.
const FRAME: usize = 960;
/// RNNoise works on samples in the i16 range.
const SCALE: f32 = 32768.0;
/// 200 ms of 20 ms frames.
const HANGOVER: u32 = 10;
pub const DEFAULT_THRESHOLD: f32 = 0.6;
/// Opus silence, sent 5 times when speech stops so receivers don't interpolate.
pub const SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
pub const SILENCE_FRAMES: usize = 5;
const BITRATE: i32 = 64_000;
const MAX_PACKET: usize = 4000;

#[derive(Debug, PartialEq)]
pub enum CaptureOut {
    Opus(Vec<u8>),
    /// Op 5 for the driver: sent before the first packet after silence, and when we stop.
    Speaking(bool),
    /// The mic hears speech right now (even while muted), for the level dot; sent when it changes.
    Level(bool),
}

/// Our microphone while in voice; dropping it closes the device.
pub struct Capture {
    engine: Engine,
    /// `None` when the device runs at 48 kHz.
    resampler: Option<Resampler<1>>,
    channels: usize,
    ring: rtrb::Consumer<f32>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    failed: Arc<AtomicBool>,
    _stream: cpal::Stream,
}

impl Capture {
    /// Open `device` (by name from `devices::inputs`), or the default input if it's unset or gone.
    pub fn start(device: Option<String>, noise_suppression: bool) -> Result<Capture, AudioError> {
        let engine = Engine::new(noise_suppression)?;
        let host = cpal::default_host();
        let named = device.and_then(|name| {
            let found = host.input_devices().ok()?.find(|d| d.to_string() == name);
            if found.is_none() {
                tracing::warn!("input device {name:?} not found, using the default");
            }
            found
        });
        let device = named.or_else(|| host.default_input_device()).ok_or(AudioError::NoDevice)?;
        // 48 kHz in any sample format (f32 first, then fewest channels), else the device default:
        // many USB mics only offer integer formats
        let supported = device
            .supported_input_configs()?
            .filter_map(|c| c.try_with_sample_rate(RATE))
            .min_by_key(|c| (c.sample_format() != cpal::SampleFormat::F32, c.channels()));
        let supported = match supported {
            Some(c) => c,
            None => device.default_input_config()?,
        };
        let (format, config) = (supported.sample_format(), supported.config());
        let channels = config.channels as usize;
        let (producer, ring) = rtrb::RingBuffer::new(config.sample_rate as usize * channels * 2 / 5); // 400 ms
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        use cpal::SampleFormat as F;
        let stream = match format {
            F::I8 => input_stream::<i8>(&device, config, producer, flag),
            F::I16 => input_stream::<i16>(&device, config, producer, flag),
            F::I24 => input_stream::<cpal::I24>(&device, config, producer, flag),
            F::I32 => input_stream::<i32>(&device, config, producer, flag),
            F::U8 => input_stream::<u8>(&device, config, producer, flag),
            F::U16 => input_stream::<u16>(&device, config, producer, flag),
            F::F64 => input_stream::<f64>(&device, config, producer, flag),
            _ => input_stream::<f32>(&device, config, producer, flag), // cpal errors on a format it can't do
        }?;
        stream.play()?;
        tracing::debug!(rate = config.sample_rate, channels, ?format, noise_suppression, "audio input open");
        Ok(Capture {
            engine,
            resampler: (config.sample_rate != RATE).then(|| Resampler::new(config.sample_rate, RATE)),
            channels,
            ring,
            mono: Vec::new(),
            resampled: Vec::new(),
            failed,
            _stream: stream,
        })
    }

    /// Called every 10 ms: everything the mic produced since, as packets and state changes.
    pub fn tick(&mut self) -> Vec<CaptureOut> {
        self.mono.clear();
        while self.ring.slots() >= self.channels {
            let sum: f32 = (0..self.channels).map(|_| self.ring.pop().unwrap_or(0.0)).sum();
            self.mono.push(sum / self.channels as f32);
        }
        let mut out = Vec::new();
        match &mut self.resampler {
            Some(r) => {
                self.resampled.clear();
                r.process(&self.mono, &mut self.resampled);
                self.engine.push(&self.resampled, &mut out);
            }
            None => self.engine.push(&self.mono, &mut out),
        }
        out
    }

    /// Voice activity sensitivity, `0.1..=0.95`.
    pub fn set_threshold(&mut self, threshold: f32) {
        self.engine.set_threshold(threshold);
    }

    /// Muted: no packets at all (Speaking(false) once); the device stays open.
    pub fn set_muted(&mut self, muted: bool) {
        self.engine.muted = muted;
    }

    /// The device reported an error (e.g. unplugged): drop this and start again.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

/// An input stream of `T` samples, pushed into `producer` as f32.
fn input_stream<T: cpal::SizedSample>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: rtrb::Producer<f32>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    f32: cpal::FromSample<T>,
{
    device.build_input_stream(
        config,
        // real-time: no locks, no allocation; a full ring drops samples
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            for &s in data {
                let _ = producer.push(cpal::Sample::to_sample::<f32>(s));
            }
        },
        move |_| failed.store(true, Ordering::Relaxed),
        None,
    )
}

/// Everything `tick` does except talking to the device.
pub(crate) struct Engine {
    denoise: Box<DenoiseState<'static>>,
    noise_suppression: bool,
    gate: Gate,
    encoder: opus::Encoder,
    /// Samples waiting for a full RNNoise frame.
    input: Vec<f32>,
    /// Processed samples of the current Opus frame.
    frame: Vec<f32>,
    /// Highest speech probability among the RNNoise frames in `frame`.
    prob: f32,
    muted: bool,
    speaking: bool,
    level: bool,
}

impl Engine {
    pub(crate) fn new(noise_suppression: bool) -> Result<Engine, AudioError> {
        let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        Ok(Engine {
            denoise: DenoiseState::new(),
            noise_suppression,
            gate: Gate::new(DEFAULT_THRESHOLD, HANGOVER),
            encoder,
            input: Vec::with_capacity(DENOISE),
            frame: Vec::with_capacity(FRAME),
            prob: 0.0,
            muted: false,
            speaking: false,
            level: false,
        })
    }

    pub(crate) fn set_threshold(&mut self, threshold: f32) {
        self.gate.threshold = if threshold.is_nan() { DEFAULT_THRESHOLD } else { threshold.clamp(0.1, 0.95) };
    }

    /// 48 kHz mono samples in, in any amount.
    pub(crate) fn push(&mut self, mut samples: &[f32], out: &mut Vec<CaptureOut>) {
        while !samples.is_empty() {
            let n = (DENOISE - self.input.len()).min(samples.len());
            self.input.extend_from_slice(&samples[..n]);
            samples = &samples[n..];
            if self.input.len() < DENOISE {
                return;
            }
            let mut pcm = std::mem::take(&mut self.input);
            let prob = self.denoise(&mut pcm);
            self.prob = self.prob.max(prob);
            self.frame.extend_from_slice(&pcm);
            pcm.clear();
            self.input = pcm;
            if self.frame.len() == FRAME {
                self.send(out);
            }
        }
    }

    /// Run RNNoise over one frame in place (left as is when suppression is off); its speech probability.
    fn denoise(&mut self, pcm: &mut [f32]) -> f32 {
        let (mut scaled, mut clean) = ([0.0; DENOISE], [0.0; DENOISE]);
        for (d, s) in scaled.iter_mut().zip(pcm.iter()) {
            *d = s * SCALE;
        }
        let prob = self.denoise.process_frame(&mut clean, &scaled);
        if self.noise_suppression {
            for (d, s) in pcm.iter_mut().zip(clean) {
                *d = s / SCALE;
            }
        }
        prob
    }

    /// A full Opus frame is in `frame`.
    fn send(&mut self, out: &mut Vec<CaptureOut>) {
        let prob = std::mem::take(&mut self.prob);
        let level = prob >= self.gate.threshold;
        if level != self.level {
            self.level = level;
            out.push(CaptureOut::Level(level));
        }
        match (self.muted, self.gate.update(prob)) {
            (false, GateOut::Send) => {
                if !self.speaking {
                    self.speaking = true;
                    out.push(CaptureOut::Speaking(true));
                }
                match self.encoder.encode_vec_float(&self.frame, MAX_PACKET) {
                    Ok(p) => out.push(CaptureOut::Opus(p)),
                    Err(e) => tracing::debug!("opus encode: {e}"),
                }
            }
            // speech ended or muted mid-speech: a silence tail so receivers don't interpolate
            (false, GateOut::Stop) | (true, _) if self.speaking => {
                out.extend((0..SILENCE_FRAMES).map(|_| CaptureOut::Opus(SILENCE.to_vec())));
                self.speaking = false;
                out.push(CaptureOut::Speaking(false));
            }
            _ => {}
        }
        self.frame.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gate that's always open: threshold 0, no hangover.
    fn open_engine() -> Engine {
        let mut e = Engine::new(true).unwrap();
        e.gate = Gate::new(0.0, 0);
        e
    }

    /// Outputs other than `Level`.
    fn push(e: &mut Engine, samples: usize) -> Vec<CaptureOut> {
        let mut out = vec![];
        e.push(&vec![0.01; samples], &mut out);
        out.retain(|o| !matches!(o, CaptureOut::Level(_)));
        out
    }

    fn packets(out: &[CaptureOut]) -> usize {
        out.iter().filter(|o| matches!(o, CaptureOut::Opus(_))).count()
    }

    #[test]
    fn chunker_480_to_960() {
        let mut e = open_engine();
        let mut out = vec![];
        for _ in 0..2999 / 7 {
            e.push(&[0.01; 7], &mut out); // 2996 samples in odd pieces
        }
        assert_eq!(packets(&out), 3, "3 full Opus frames");
        assert_eq!((e.input.len(), e.frame.len()), (116, 0), "rest waits for the next RNNoise frame");
        e.push(&[0.01; 364], &mut out);
        assert_eq!((packets(&out), e.input.len(), e.frame.len()), (3, 0, DENOISE), "one RNNoise frame: half an Opus frame");
        e.push(&[0.01; 480], &mut out);
        assert_eq!(packets(&out), 4);
        assert_eq!(out[0], CaptureOut::Level(true));
        assert_eq!(out[1], CaptureOut::Speaking(true), "before the first packet");
        let CaptureOut::Opus(p) = &out[2] else { panic!("{:?}", out[2]) };
        let dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        assert_eq!(dec.get_nb_samples(p).unwrap(), FRAME, "20 ms per packet");
    }

    #[test]
    fn muted_sends_nothing() {
        let mut e = open_engine();
        assert_eq!(packets(&push(&mut e, FRAME)), 1);
        e.muted = true;
        let silence = CaptureOut::Opus(SILENCE.to_vec());
        let out = push(&mut e, FRAME * 5);
        assert_eq!(out.len(), SILENCE_FRAMES + 1, "{out:?}");
        assert!(out[..SILENCE_FRAMES].iter().all(|o| *o == silence), "silence tail: {out:?}");
        assert_eq!(out[SILENCE_FRAMES], CaptureOut::Speaking(false), "then Speaking(false) once");
        assert!(push(&mut e, FRAME * 5).is_empty());
        e.muted = false;
        let out = push(&mut e, FRAME);
        assert_eq!((out[0] == CaptureOut::Speaking(true), packets(&out)), (true, 1), "unmuted: talking again at once");
    }

    #[test]
    fn silence_frames_on_stop() {
        let mut e = open_engine();
        assert_eq!(packets(&push(&mut e, FRAME)), 1);
        e.gate.threshold = 2.0; // nothing is speech now
        let silence = CaptureOut::Opus(SILENCE.to_vec());
        let out = push(&mut e, FRAME);
        assert_eq!(out.len(), SILENCE_FRAMES + 1, "{out:?}");
        assert!(out[..SILENCE_FRAMES].iter().all(|o| *o == silence), "{out:?}");
        assert_eq!(out[SILENCE_FRAMES], CaptureOut::Speaking(false));
        assert!(push(&mut e, FRAME * 5).is_empty(), "then nothing");
    }

    #[test]
    fn threshold_is_clamped() {
        let mut e = Engine::new(true).unwrap();
        assert_eq!(e.gate.threshold, DEFAULT_THRESHOLD);
        e.set_threshold(0.0);
        assert_eq!(e.gate.threshold, 0.1);
        e.set_threshold(1.0);
        assert_eq!(e.gate.threshold, 0.95);
        e.set_threshold(f32::NAN);
        assert_eq!(e.gate.threshold, DEFAULT_THRESHOLD);
    }

    #[test]
    fn noise_suppression_off_still_vad() {
        let (mut on, mut off) = (Engine::new(true).unwrap(), Engine::new(false).unwrap());
        let mut seed = 1u32;
        let mut changed = false;
        for _ in 0..20 {
            let raw: Vec<f32> = (0..DENOISE)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                })
                .collect();
            let (mut a, mut b) = (raw.clone(), raw.clone());
            assert_eq!(on.denoise(&mut a), off.denoise(&mut b), "same RNNoise probability either way");
            assert_eq!(b, raw, "suppression off: raw samples go out");
            changed |= a != raw;
        }
        assert!(changed, "suppression on changes the samples");
    }
}
