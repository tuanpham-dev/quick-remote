//! Monotonic microsecond clock, and the client's estimate of the host clock.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static BASE: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the first call in this process (monotonic).
pub fn now_us() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// The `Instant` that [`now_us`] measures from.
pub fn base() -> Instant {
    *BASE.get_or_init(Instant::now)
}

pub fn instant_to_us(t: Instant) -> u64 {
    t.saturating_duration_since(base()).as_micros() as u64
}

/// Estimates `host_clock − client_clock` from ping/pong pairs: within a 5 s
/// window, the sample with the smallest round trip has the least queueing,
/// so its midpoint assumption is the most accurate.
#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<(u64, u64, i64)>,
    pub rtt_us: u64,
    pub offset_us: i64,
}

impl ClockSync {
    const WINDOW_US: u64 = 5_000_000;

    /// Feeds one pong: `t_client` as sent, `t_host` from the host, and
    /// `t_recv` when the pong arrived (both client clock).
    pub fn sample(&mut self, t_client: u64, t_host: u64, t_recv: u64) {
        let rtt = t_recv.saturating_sub(t_client);
        let offset = t_host as i64 - (t_client + rtt / 2) as i64;
        self.samples.push_back((t_recv, rtt, offset));
        while self.samples.front().is_some_and(|s| t_recv - s.0 > Self::WINDOW_US) {
            self.samples.pop_front();
        }
        let best = self.samples.iter().min_by_key(|s| s.1).expect("just pushed");
        self.offset_us = best.2;
        // Report a smoothed RTT for the overlay, not the minimum.
        self.rtt_us = if self.rtt_us == 0 { rtt } else { (self.rtt_us * 7 + rtt) / 8 };
    }

    pub fn ready(&self) -> bool {
        !self.samples.is_empty()
    }

    /// Converts a host timestamp to the client clock.
    pub fn to_client(&self, t_host: u64) -> i64 {
        t_host as i64 - self.offset_us
    }

    pub fn min_rtt(&self) -> Duration {
        Duration::from_micros(self.samples.iter().map(|s| s.1).min().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_min_rtt_offset() {
        let mut c = ClockSync::default();
        // True offset 1000; asymmetric queueing on the noisy samples.
        c.sample(0, 1000 + 500 + 400, 1000);
        c.sample(10_000, 10_000 + 1000 + 100, 10_200);
        c.sample(20_000, 20_000 + 1000 + 2000, 22_100);
        assert_eq!(c.offset_us, 1000);
        assert_eq!(c.to_client(5000), 4000);
    }
}
