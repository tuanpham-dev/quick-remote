//! PIN pairing: SPAKE2 over the control stream, bound to the TLS session.
//!
//! The client sends SPAKE2 message A, the host message B. Both derive the
//! shared key and MAC a transcript of both certificate fingerprints and a
//! TLS exporter value, so a man in the middle (who has to terminate TLS with
//! different certificates) fails confirmation even with the right PIN. The
//! client confirms first; the host answers only after checking it.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use rand::Rng;

pub use qr_pair::{Keys, Pairing, Transcript};

pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-qr-pair";

/// A fresh 6-digit PIN.
pub fn new_pin() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

/// Host-side limit on PIN guesses: after `max` failures within a minute,
/// the address is ignored for `ban`.
pub struct PinLimiter {
    max: usize,
    ban: Duration,
    failures: HashMap<IpAddr, VecDeque<Instant>>,
    banned: HashMap<IpAddr, Instant>,
}

impl Default for PinLimiter {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(60))
    }
}

impl PinLimiter {
    pub fn new(max: usize, ban: Duration) -> Self {
        Self { max, ban, failures: HashMap::new(), banned: HashMap::new() }
    }

    pub fn allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        match self.banned.get(&ip) {
            Some(until) if now < *until => false,
            Some(_) => {
                self.banned.remove(&ip);
                true
            }
            None => true,
        }
    }

    pub fn failure(&mut self, ip: IpAddr, now: Instant) {
        let q = self.failures.entry(ip).or_default();
        q.push_back(now);
        while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() >= self.max {
            q.clear();
            self.banned.insert(ip, now + self.ban);
        }
    }

    pub fn success(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(exporter: u8) -> Transcript {
        Transcript { client_fp: [1; 32], host_fp: [2; 32], exporter: [exporter; 32] }
    }

    fn run(client_pin: &str, host_pin: &str, tc: Transcript, th: Transcript) -> (bool, bool) {
        let (c, ma) = Pairing::client(client_pin);
        let (h, mb) = Pairing::host(host_pin);
        let kc = c.finish(&mb, &tc).unwrap();
        let kh = h.finish(&ma, &th).unwrap();
        let host_accepts = kh.check(true, &kc.client_mac());
        let client_accepts = kc.check(false, &kh.host_mac());
        (host_accepts, client_accepts)
    }

    #[test]
    fn right_pin_pairs() {
        assert_eq!(run("123456", "123456", t(9), t(9)), (true, true));
    }

    #[test]
    fn wrong_pin_fails_both() {
        assert_eq!(run("123456", "123457", t(9), t(9)), (false, false));
    }

    #[test]
    fn mitm_fails_with_right_pin() {
        // A relay that terminates TLS gives each side a different session.
        assert_eq!(run("123456", "123456", t(9), t(8)), (false, false));
    }

    #[test]
    fn limiter_bans_after_five() {
        let mut l = PinLimiter::default();
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..4 {
            l.failure(ip, t0);
        }
        assert!(l.allowed(ip, t0));
        l.failure(ip, t0);
        assert!(!l.allowed(ip, t0 + Duration::from_secs(59)));
        assert!(l.allowed(ip, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..100 {
            let p = new_pin();
            assert!(p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
