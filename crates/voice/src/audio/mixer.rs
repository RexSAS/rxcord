//! Sum per-user stereo PCM with gain.

/// Mix stereo interleaved `sources` (each scaled by its gain) into `out`, clamped to [-1, 1].
pub fn mix(out: &mut [f32], sources: &[(&[f32], f32)]) {
    out.fill(0.0);
    for (src, gain) in sources {
        for (o, s) in out.iter_mut().zip(*src) {
            *o += s * gain;
        }
    }
    for o in out {
        *o = o.clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_gain_and_clamp() {
        let a = [0.5, -0.5, 0.9, -0.9];
        let b = [0.25, 0.25, 0.9, -0.9];
        let mut out = [7.0; 4];
        mix(&mut out, &[(&a, 1.0), (&b, 2.0)]);
        assert_eq!(out, [1.0, 0.0, 1.0, -1.0]);
        mix(&mut out, &[(&a, 0.5)]);
        assert_eq!(out, [0.25, -0.25, 0.45, -0.45]);
    }

    #[test]
    fn mixer_mute_for_me() {
        let a = [0.5, -0.5];
        let mut out = [7.0; 2];
        mix(&mut out, &[(&a, 0.0)]);
        assert_eq!(out, [0.0, 0.0]);
    }
}
