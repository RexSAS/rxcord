use std::time::Duration;

/// `min(2^attempt, 60)` seconds scaled by `0.5 + 0.5 * jitter` (jitter in [0,1)).
pub fn backoff(attempt: u32, jitter: f64) -> Duration {
    let jitter = if jitter.is_nan() { 0.0 } else { jitter.clamp(0.0, 1.0) };
    let secs = 2u64.saturating_pow(attempt).min(60) as f64;
    Duration::from_secs_f64(secs * (0.5 + 0.5 * jitter))
}

/// Delay before a reconnect (`attempt` as for `backoff`). A fresh identify (`resume == false`, e.g. op 9
/// invalid session) waits at least a random 1-5 s, as Discord asks; `r1`/`r2` are random in [0,1).
pub fn reconnect_delay(attempt: u32, resume: bool, r1: f64, r2: f64) -> Duration {
    let d = backoff(attempt, r1);
    let r2 = if r2.is_nan() { 0.0 } else { r2.clamp(0.0, 1.0) };
    if resume { d } else { d.max(Duration::from_secs_f64(1.0 + 4.0 * r2)) }
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
    fn reidentify_waits_1_to_5s() {
        assert_eq!(reconnect_delay(0, false, 0.0, 0.0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(0, false, 0.0, 1.0), Duration::from_secs(5));
        assert_eq!(reconnect_delay(0, true, 0.0, 1.0), Duration::from_millis(500));
        assert_eq!(reconnect_delay(10, false, 1.0, 0.0), Duration::from_secs(60)); // backoff still caps it below
    }

    #[test]
    fn backoff_first_is_about_1s() {
        assert_eq!(backoff(0, 0.0), Duration::from_millis(500));
    }
}
