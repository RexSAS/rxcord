//! Audio engine: devices, jitter buffer, Opus decode, mixer, cpal output; capture, RNNoise, voice activity, Opus encode.

pub mod capture;
pub mod devices;
pub mod jitter;
pub mod mixer;
pub mod playback;
pub mod tone;
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

/// A stream error callback that flags `failed` only when the stream is really gone: xruns (USB mics
/// report one on every data gap), reroutes and refused real-time priority leave it running.
pub(crate) fn on_error(failed: std::sync::Arc<std::sync::atomic::AtomicBool>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e| {
        use cpal::ErrorKind as K;
        if !matches!(e.kind(), K::Xrun | K::DeviceChanged | K::RealtimeDenied) {
            tracing::warn!("audio stream: {e}");
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn only_fatal_errors_fail_the_stream() {
        let failed = Arc::new(AtomicBool::new(false));
        let mut cb = super::on_error(failed.clone());
        cb(cpal::ErrorKind::Xrun.into());
        cb(cpal::ErrorKind::DeviceChanged.into());
        cb(cpal::ErrorKind::RealtimeDenied.into());
        assert!(!failed.load(Ordering::Relaxed), "glitches keep the device");
        cb(cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(failed.load(Ordering::Relaxed), "unplugged: reopen");
    }
}
