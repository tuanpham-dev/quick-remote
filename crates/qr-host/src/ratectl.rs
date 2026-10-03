//! Rate control from client feedback (every 50 ms).
//!
//! LAN profile: a high fixed target, cut by 15% only when loss exceeds what
//! FEC repairs, recovering 5% a second.
//!
//! Internet profile: delay-gradient control. While the frame transit delay
//! is flat the target grows 8% a second; when it rises (queues building) or
//! loss exceeds FEC, the target drops to 85% of what actually arrived. The
//! worst-case reaction is two feedback intervals (100 ms).
//!
//! In both, the frame rate gives way before per-frame quality: when the
//! bits per frame fall below a floor the frame rate steps down (to a minimum
//! of 5 fps), and it steps back up once there is room for twice the floor.

use std::time::{Duration, Instant};

use qr_proto::msg::{Feedback, Profile};

#[derive(Debug, Clone)]
pub struct Limits {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub min_kbps: u32,
    pub max_kbps: u32,
}

impl Limits {
    fn pixels(&self) -> f64 {
        self.width as f64 * self.height as f64
    }

    /// Default LAN target: 0.25 bits per pixel per frame (31 Mbit/s at
    /// 1080p60, 124 Mbit/s at 4K60), capped at `max_kbps`.
    pub fn lan_target(&self) -> u32 {
        ((self.pixels() * self.max_fps as f64 * 0.25 / 1000.0) as u32).clamp(self.min_kbps, self.max_kbps)
    }

    /// Below this many bits per frame, lower the frame rate instead
    /// (0.03 bpp: ~62 kbit per 1080p frame).
    fn frame_floor_kbit(&self) -> f64 {
        self.pixels() * 0.03 / 1000.0
    }
}

pub struct RateCtl {
    pub profile: Profile,
    limits: Limits,
    pub target_kbps: u32,
    pub fps: u32,
    pub parity: f32,
    /// Smoothed shard loss, for per-block FEC sizing.
    pub loss_ewma: f32,
    hold_increase_until: Option<Instant>,
    base_delay: Option<(i32, Instant)>,
    last_change: Instant,
}

pub const FPS_STEPS: [u32; 7] = [240, 144, 120, 60, 30, 15, 5];

impl RateCtl {
    pub fn new(profile: Profile, limits: Limits, now: Instant) -> Self {
        let target = match profile {
            Profile::Lan => limits.lan_target(),
            Profile::Internet => 4_000u32.clamp(limits.min_kbps, limits.max_kbps),
        };
        let mut r = Self {
            profile,
            fps: limits.max_fps,
            limits,
            target_kbps: target,
            parity: if profile == Profile::Lan { 0.10 } else { 0.20 },
            // Assume some loss until feedback says otherwise.
            loss_ewma: 0.01,
            hold_increase_until: None,
            base_delay: None,
            last_change: now,
        };
        r.fit_fps();
        r
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Applies one feedback report. Returns true when target, fps or
    /// parity changed.
    pub fn on_feedback(&mut self, fb: &Feedback, now: Instant) -> bool {
        let before = (self.target_kbps, self.fps, (self.parity * 100.0) as u32);
        let window = Duration::from_millis(fb.window_ms.max(1) as u64);
        let sent_shards = fb.shards_rx + fb.shards_lost;
        let loss = if sent_shards > 0 { fb.shards_lost as f32 / sent_shards as f32 } else { 0.0 };
        self.loss_ewma = self.loss_ewma * 0.8 + loss * 0.2;
        let rx_kbps = (fb.bytes_rx as f64 * 8.0 / window.as_secs_f64() / 1000.0) as u32;
        let beyond_fec = fb.frames_lost > 0;
        let dt = window.as_secs_f64().min(0.5);

        match self.profile {
            Profile::Lan => {
                if beyond_fec {
                    self.decrease((self.target_kbps as f64 * 0.85) as u32, now);
                } else if self.may_increase(now) {
                    let cap = self.limits.lan_target();
                    self.target_kbps = ((self.target_kbps as f64 * 1.05f64.powf(dt)) as u32 + 1).min(cap);
                }
            }
            Profile::Internet => {
                // Queueing delay relative to the lowest delay seen recently.
                let base = match self.base_delay {
                    Some((d, t)) if now.duration_since(t) < Duration::from_secs(10) && d <= fb.delay_min_us => d,
                    _ => {
                        self.base_delay = Some((fb.delay_min_us, now));
                        fb.delay_min_us
                    }
                };
                let queueing_ms = (fb.delay_max_us - base) as f64 / 1000.0;
                let rising = fb.delay_slope > 3_000 && queueing_ms > 5.0;
                if beyond_fec || rising || queueing_ms > 40.0 {
                    let to = if rx_kbps > 0 { (rx_kbps as f64 * 0.85) as u32 } else { self.target_kbps / 2 };
                    self.decrease(to.min((self.target_kbps as f64 * 0.95) as u32), now);
                } else if self.may_increase(now) && rx_kbps as f64 >= self.target_kbps as f64 * 0.6 {
                    // Only grow when the stream actually uses the budget;
                    // a still screen sends little and proves nothing. Grow
                    // 8%/s near the last congestion point, 50%/s once the
                    // link has been quiet for 3 s (recovering from a squeeze
                    // at 8%/s alone takes ~40 s).
                    let quiet =
                        self.hold_increase_until.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(2500));
                    let g: f64 = if quiet { 1.5 } else { 1.08 };
                    self.target_kbps = ((self.target_kbps as f64 * g.powf(dt)) as u32 + 1).min(self.limits.max_kbps);
                }
            }
        }
        // Parity follows loss: twice the observed loss plus 5%.
        let (lo, hi) = if self.profile == Profile::Lan { (0.10, 0.25) } else { (0.20, 0.50) };
        self.parity = (self.loss_ewma * 2.0 + 0.05).clamp(lo, hi);
        self.fit_fps();
        let after = (self.target_kbps, self.fps, (self.parity * 100.0) as u32);
        if after != before {
            self.last_change = now;
        }
        after != before
    }

    fn may_increase(&self, now: Instant) -> bool {
        self.hold_increase_until.is_none_or(|t| now >= t)
    }

    fn decrease(&mut self, to: u32, now: Instant) {
        self.target_kbps = to.clamp(self.limits.min_kbps, self.limits.max_kbps);
        self.hold_increase_until = Some(now + Duration::from_millis(500));
    }

    /// Picks the highest frame-rate step whose per-frame budget stays above
    /// the floor, with hysteresis: stepping up needs twice the floor.
    fn fit_fps(&mut self) {
        let floor = self.limits.frame_floor_kbit();
        let per_frame = |fps: u32| self.target_kbps as f64 / fps as f64;
        let steps: Vec<u32> = FPS_STEPS.iter().copied().filter(|f| *f <= self.limits.max_fps).collect();
        let mut fps = self.fps.min(self.limits.max_fps);
        // Down while starved.
        while per_frame(fps) < floor {
            match steps.iter().find(|f| **f < fps) {
                Some(f) => fps = *f,
                None => break,
            }
        }
        // Up while there's room for twice the floor at the next step.
        while let Some(up) = steps.iter().rev().find(|f| **f > fps) {
            if per_frame(*up) >= floor * 2.0 {
                fps = *up;
            } else {
                break;
            }
        }
        self.fps = fps;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits { width: 1920, height: 1080, max_fps: 60, min_kbps: 300, max_kbps: 150_000 }
    }

    fn fb(bytes_per_50ms: u32, lost: u32, frames_lost: u16, delay: (i32, i32, i32)) -> Feedback {
        Feedback {
            window_ms: 50,
            bytes_rx: bytes_per_50ms,
            shards_rx: 100,
            shards_lost: lost,
            frames_rx: 3,
            frames_lost,
            delay_min_us: delay.0,
            delay_max_us: delay.1,
            delay_slope: delay.2,
            ..Default::default()
        }
    }

    #[test]
    fn lan_defaults_and_backoff_on_unrepaired_loss() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(Profile::Lan, limits(), t0);
        assert_eq!(r.target_kbps, 31_104);
        assert_eq!(r.fps, 60);
        r.on_feedback(&fb(190_000, 0, 1, (0, 0, 0)), t0);
        assert_eq!(r.target_kbps, 26_438);
        // No growth for 500 ms after a cut.
        r.on_feedback(&fb(190_000, 0, 0, (0, 0, 0)), t0 + Duration::from_millis(200));
        assert_eq!(r.target_kbps, 26_438);
        for i in 0..200 {
            r.on_feedback(&fb(190_000, 0, 0, (0, 0, 0)), t0 + Duration::from_millis(600 + i * 50));
        }
        assert_eq!(r.target_kbps, 31_104, "recovers to the LAN target, not beyond");
    }

    #[test]
    fn internet_converges_below_link_and_drops_fps_first() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(Profile::Internet, limits(), t0);
        // A 10 Mbit/s link: while the target is under it, delay is flat and
        // everything arrives; above it, delay climbs and arrivals cap.
        let mut t = t0;
        for _ in 0..600 {
            t += Duration::from_millis(50);
            let over = r.target_kbps > 10_000;
            let rx = r.target_kbps.min(10_000);
            let f = if over {
                fb(rx * 50 / 8, 0, 0, (25_000, 60_000, 20_000))
            } else {
                fb(rx * 50 / 8, 0, 0, (25_000, 26_000, 0))
            };
            r.on_feedback(&f, t);
        }
        assert!((6_000..=10_500).contains(&r.target_kbps), "target {}", r.target_kbps);
        assert_eq!(r.fps, 60);

        // Squeeze to 1 Mbit/s: frame rate gives way first.
        for _ in 0..100 {
            t += Duration::from_millis(50);
            let rx = r.target_kbps.min(1_000);
            let over = r.target_kbps > 1_000;
            let f = fb(rx * 50 / 8, 0, 0, if over { (25_000, 90_000, 30_000) } else { (25_000, 26_000, 0) });
            r.on_feedback(&f, t);
        }
        assert!(r.target_kbps <= 1_100, "target {}", r.target_kbps);
        assert!(r.fps <= 15, "fps {}", r.fps);
    }

    #[test]
    fn parity_tracks_loss() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(Profile::Internet, limits(), t0);
        for i in 0..40 {
            r.on_feedback(&fb(10_000, 10, 0, (0, 0, 0)), t0 + Duration::from_millis(i * 50));
        }
        // 10 lost of 110 sent: 9.1% loss → 2 × 9.1% + 5% = 23%.
        assert!((0.22..=0.24).contains(&r.parity), "parity {}", r.parity);
    }
}
