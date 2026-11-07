//! Audio engine: devices, jitter buffer, Opus decode, mixer, cpal output; capture, RNNoise, voice activity, Opus encode.

pub mod capture;
pub mod devices;
pub mod jitter;
pub mod mixer;
pub mod playback;
pub mod vad;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio device")]
    NoDevice,
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
}
