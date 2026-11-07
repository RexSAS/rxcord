use std::time::Duration;

/// `min(2^attempt, 60)` seconds scaled by `0.5 + 0.5 * jitter` (jitter in [0,1)).
pub fn backoff(attempt: u32, jitter: f64) -> Duration {
    let jitter = if jitter.is_nan() { 0.0 } else { jitter.clamp(0.0, 1.0) };
    let secs = 2u64.saturating_pow(attempt).min(60) as f64;
    Duration::from_secs_f64(secs * (0.5 + 0.5 * jitter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_60() {
        let d = backoff(10, 0.999);
        assert!(d <= Duration::from_secs(60) && d >= Duration::from_secs(59));
    }

    #[test]
    fn backoff_first_is_about_1s() {
        assert_eq!(backoff(0, 0.0), Duration::from_millis(500));
    }
}
