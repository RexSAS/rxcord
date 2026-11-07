use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

/// Tracks per-route exhaustion and a global lock; `acquire` sleeps until neither applies.
#[derive(Default)]
pub struct RateLimiter {
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Instant>,
    global: Option<Instant>,
}

impl RateLimiter {
    pub async fn acquire(&self, route: &str) {
        loop {
            let now = Instant::now();
            let until = {
                let mut s = self.inner.lock().unwrap();
                s.routes.retain(|_, t| *t > now);
                if s.global.is_some_and(|t| t <= now) {
                    s.global = None;
                }
                s.routes.get(route).copied().into_iter().chain(s.global).max()
            };
            match until {
                Some(t) => tokio::time::sleep_until(t).await,
                None => return,
            }
        }
    }

    pub fn update(&self, route: &str, remaining: Option<u32>, reset_after: Option<f64>) {
        let mut s = self.inner.lock().unwrap();
        match (remaining, reset_after) {
            (Some(0), Some(r)) => {
                s.routes.insert(route.to_string(), Instant::now() + secs(r));
            }
            _ => {
                s.routes.remove(route);
            }
        }
    }

    pub fn on_429(&self, route: &str, retry_after: f64, global: bool) {
        let t = Instant::now() + secs(retry_after);
        let mut s = self.inner.lock().unwrap();
        if global {
            s.global = Some(t);
        } else {
            s.routes.insert(route.to_string(), t);
        }
    }
}

fn secs(f: f64) -> Duration {
    Duration::try_from_secs_f64(f.max(0.0)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    #[tokio::test(start_paused = true)]
    async fn ratelimit_waits_until_reset() {
        let rl = RateLimiter::default();
        rl.update("GET /a", Some(0), Some(2.0));
        let t = Instant::now();
        rl.acquire("GET /a").await;
        assert!(t.elapsed() >= Duration::from_secs(2));
        rl.acquire("GET /b").await; // other route unaffected
    }

    #[tokio::test(start_paused = true)]
    async fn global_429_blocks_all_routes() {
        let rl = RateLimiter::default();
        rl.on_429("GET /a", 1.5, true);
        let t = Instant::now();
        rl.acquire("POST /z").await;
        assert!(t.elapsed() >= Duration::from_millis(1500));
    }
}
