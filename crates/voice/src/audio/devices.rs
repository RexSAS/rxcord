//! Audio device names (as shown in settings and passed to `Playback::start`).

use cpal::traits::HostTrait;

pub fn inputs() -> Vec<String> {
    cpal::default_host().input_devices().map(|ds| ds.map(|d| d.to_string()).collect()).unwrap_or_default()
}

pub fn outputs() -> Vec<String> {
    cpal::default_host().output_devices().map(|ds| ds.map(|d| d.to_string()).collect()).unwrap_or_default()
}
