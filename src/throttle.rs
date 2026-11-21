//! Basic brute-force throttle: after `max` failures from one IP inside `window`, further
//! attempts are refused until the window (counted from the first failure) has passed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Throttle {
    max: u32,
    window: Duration,
    inner: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Throttle {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// `Err(retry_after_secs)` while the IP is blocked.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let mut m = self.inner.lock().unwrap();
        if let Some(&(count, start)) = m.get(&ip) {
            let elapsed = now.saturating_duration_since(start);
            if elapsed >= self.window {
                m.remove(&ip);
            } else if count >= self.max {
                return Err((self.window - elapsed).as_secs().max(1));
            }
        }
        Ok(())
    }

    pub fn fail(&self, ip: IpAddr, now: Instant) {
        let mut m = self.inner.lock().unwrap();
        if m.len() > 10_000 {
            let w = self.window;
            m.retain(|_, (_, s)| now.saturating_duration_since(*s) < w);
        }
        let e = m.entry(ip).or_insert((0, now));
        if now.saturating_duration_since(e.1) >= self.window {
            *e = (0, now);
        }
        e.0 += 1;
    }

    pub fn reset(&self, ip: IpAddr) {
        self.inner.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_failures_then_recovers() {
        let t = Throttle::new(3, Duration::from_secs(600));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(t.check(ip, t0).is_ok());
            t.fail(ip, t0);
        }
        let retry = t.check(ip, t0 + Duration::from_secs(60)).unwrap_err();
        assert!((539..=540).contains(&retry));
        // other IPs unaffected
        assert!(t.check("192.0.2.2".parse().unwrap(), t0).is_ok());
        // window elapsed
        assert!(t.check(ip, t0 + Duration::from_secs(601)).is_ok());
    }

    #[test]
    fn success_resets() {
        let t = Throttle::new(2, Duration::from_secs(600));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let t0 = Instant::now();
        t.fail(ip, t0);
        t.fail(ip, t0);
        assert!(t.check(ip, t0).is_err());
        t.reset(ip);
        assert!(t.check(ip, t0).is_ok());
    }
}
