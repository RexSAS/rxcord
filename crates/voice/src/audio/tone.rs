//! The incoming-call ring: two beeps of 440 + 480 Hz, 0.4 s on every 2 s, generated (no sound files).

use cpal::traits::{DeviceTrait, StreamTrait};

use super::AudioError;

/// Peak of each sine: together ≈ −18 dBFS (0.126).
const AMP: f32 = 0.063;

/// The ring as an endless sample source at `rate` Hz (mono).
pub struct Ring {
    rate: u32,
    /// Samples produced so far.
    n: u64,
}

impl Ring {
    pub fn new(rate: u32) -> Ring {
        Ring { rate, n: 0 }
    }

    pub fn next_sample(&mut self) -> f32 {
        let rate = self.rate as u64;
        let i = self.n % (rate * 2); // 2 s period
        self.n += 1;
        if i >= rate * 2 / 5 {
            return 0.0; // after the 0.4 s beep
        }
        // the phase restarts with each beep: both sines start and (0.4 s = whole cycles) end at zero, no click
        let t = i as f32 / self.rate as f32;
        let w = 2.0 * std::f32::consts::PI * t;
        AMP * ((440.0 * w).sin() + (480.0 * w).sin())
    }
}

/// The ring on its own output stream, while no call is playing; dropping it closes the device.
pub struct RingOut {
    _stream: cpal::Stream,
}

impl RingOut {
    /// Open `device` (by name), or the default output.
    pub fn start(device: Option<String>) -> Result<RingOut, AudioError> {
        let (device, config) = super::playback::output(device)?;
        let channels = config.channels as usize;
        let mut ring = Ring::new(config.sample_rate);
        let stream = device.build_output_stream(
            config,
            // real-time: no locks, no allocation
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    frame.fill(ring.next_sample());
                }
            },
            |e| tracing::debug!("ring output: {e}"),
            None,
        )?;
        stream.play()?;
        Ok(RingOut { _stream: stream })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_tone_cadence() {
        let mut r = Ring::new(48_000);
        let s: Vec<f32> = (0..48_000 * 4).map(|_| r.next_sample()).collect();
        let (on, period) = (19_200, 96_000); // 0.4 s, 2 s
        for p in [0, period] {
            let beep = &s[p..p + on];
            assert!(beep.iter().filter(|x| x.abs() > 0.01).count() > on * 3 / 4, "beep at {p} sounds");
            assert!(s[p + on..p + period].iter().all(|x| *x == 0.0), "silent after the beep at {p}");
        }
        assert_eq!(&s[..on], &s[period..period + on], "every period the same");
        let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
        let dbfs = 20.0 * peak.log10();
        assert!((-19.0..=-17.0).contains(&dbfs), "quiet: {dbfs} dBFS");
        // 440 and 480 Hz: the sum beats at 40 Hz, so the envelope has 16 dips in 0.4 s
        let dips = beep_dips(&s[..on], 48_000);
        assert_eq!(dips, 16);
        let mut r44 = Ring::new(44_100);
        let n = (0..44_100 * 2).filter(|_| r44.next_sample() != 0.0).count();
        assert!(n > 17_000 && n <= 17_640, "0.4 s at any device rate: {n}");
    }

    /// Envelope minima (1 ms windows near silence) in `s`: the 40 Hz beating of 440 + 480 Hz.
    fn beep_dips(s: &[f32], rate: usize) -> usize {
        let w = rate / 1000;
        let quiet: Vec<bool> = s.chunks(w).map(|c| c.iter().fold(0f32, |m, x| m.max(x.abs())) < 0.02).collect();
        quiet.windows(2).filter(|p| p[1] && !p[0]).count()
    }
}
