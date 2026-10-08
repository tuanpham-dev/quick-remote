//! Rate control from client feedback (every 50 ms): one controller for every
//! path, local or internet.
//!
//! The target grows while delivery is clean (8% a second near the last cut,
//! doubling each second once the link has been quiet for 1.5 s) up to the default
//! quality target, and backs off to 85% of what actually arrived when the
//! link shows real congestion: loss that FEC could not repair in two windows
//! in a row, a frame delay that keeps rising for three windows, or a queue
//! above 80 ms in two windows. Single spikes (Wi-Fi jitter, a lost burst)
//! cause no cut: cutting on each one starved Wi-Fi links down to 5 fps.
//!
//! The frame rate gives way before per-frame quality: when the bits per
//! frame fall below a floor the frame rate steps down (to a minimum of
//! 5 fps), and it steps back up once there is room for twice the floor.

use std::time::{Duration, Instant};

use qr_proto::msg::Feedback;

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

    /// Default (and highest) target: 0.25 bits per pixel per frame
    /// (31 Mbit/s at 1080p60, 124 Mbit/s at 4K60), capped at `max_kbps`.
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
    limits: Limits,
    pub target_kbps: u32,
    pub fps: u32,
    pub parity: f32,
    /// Smoothed shard loss, for per-block FEC sizing.
    pub loss_ewma: f32,
    hold_increase_until: Option<Instant>,
    base_delay: Option<(i32, Instant)>,
    /// Consecutive windows with unrepaired loss, with a rising delay, and
    /// with a long queue.
    lossy_windows: u32,
    rising_windows: u32,
    queued_windows: u32,
    last_change: Instant,
    /// The client can't decode this fast: frame-rate ceiling until when.
    decode_cap: Option<(u32, Instant)>,
    /// Consecutive feedback windows with a slow decoder, and the last step
    /// the decoder caused.
    slow_windows: u32,
    last_decode_step: Option<Instant>,
    /// Bytes sent since the last feedback (None: unknown, taken as busy).
    sent_bytes: Option<u64>,
    /// No congestion signals before this: a (re)started stream's first
    /// frames arrive out of step with the client's reassembly.
    grace_until: Option<Instant>,
}

pub const FPS_STEPS: [u32; 7] = [240, 144, 120, 60, 30, 15, 5];

impl RateCtl {
    pub fn new(limits: Limits, now: Instant) -> Self {
        let mut r = Self {
            fps: limits.max_fps,
            target_kbps: limits.lan_target(),
            limits,
            parity: 0.10,
            // Assume some loss until feedback says otherwise.
            loss_ewma: 0.01,
            hold_increase_until: None,
            base_delay: None,
            lossy_windows: 0,
            rising_windows: 0,
            queued_windows: 0,
            last_change: now,
            decode_cap: None,
            slow_windows: 0,
            last_decode_step: None,
            sent_bytes: None,
            grace_until: None,
        };
        r.fit_fps();
        r
    }

    /// Ignores congestion signals for a second (a stream just started).
    pub fn grace(&mut self, now: Instant) {
        self.grace_until = Some(now + Duration::from_secs(1));
    }

    /// What the sender put on the wire since the last feedback.
    pub fn note_sent(&mut self, bytes: u64) {
        self.sent_bytes = Some(bytes);
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
        let dt = window.as_secs_f64().min(0.5);
        // Sending well under budget (under a quarter: a still screen, a burst now and then):
        // delivery delay then measures the bursts, not a queue, and what
        // arrived says nothing about what the path could carry.
        let idle = self
            .sent_bytes
            .take()
            .is_some_and(|b| (b as f64 * 8.0 / window.as_secs_f64() / 1000.0) < self.target_kbps as f64 * 0.25);
        let in_grace = self.grace_until.is_some_and(|t| now < t);

        // Queueing delay relative to the lowest delay seen in the last 10 s.
        let base = match self.base_delay {
            Some((d, t)) if now.duration_since(t) < Duration::from_secs(10) && d <= fb.delay_min_us => d,
            _ => {
                self.base_delay = Some((fb.delay_min_us, now));
                fb.delay_min_us
            }
        };
        let queueing_ms = (fb.delay_max_us - base) as f64 / 1000.0;
        // A window in which nothing arrived carries no delay information
        // (a squeezed link delivers only a few frames a second): it neither
        // continues nor breaks a run.
        if in_grace {
            (self.lossy_windows, self.rising_windows, self.queued_windows) = (0, 0, 0);
        } else if fb.frames_rx > 0 || fb.frames_lost > 0 {
            let count = |n: &mut u32, hit: bool| *n = if hit { *n + 1 } else { 0 };
            count(&mut self.lossy_windows, fb.frames_lost > 0);
            let arrived = fb.frames_rx > 0 && !idle;
            count(&mut self.rising_windows, arrived && fb.delay_slope > 3_000 && queueing_ms > 8.0);
            count(&mut self.queued_windows, arrived && queueing_ms > 80.0);
        }
        let congested = self.lossy_windows >= 2 || self.rising_windows >= 3 || self.queued_windows >= 2;
        if congested {
            let to = if idle {
                (self.target_kbps as f64 * 0.85) as u32
            } else if rx_kbps > 0 {
                (rx_kbps as f64 * 0.85) as u32
            } else {
                (self.target_kbps as f64 * 0.7) as u32
            };
            self.decrease(to.min((self.target_kbps as f64 * 0.95) as u32), now);
            (self.lossy_windows, self.rising_windows, self.queued_windows) = (0, 0, 0);
        } else if self.may_increase(now)
            && fb.frames_lost == 0
            && (rx_kbps as f64 >= self.target_kbps as f64 * 0.6 || self.target_kbps < self.limits.lan_target() / 4)
        {
            // Grow only while the stream uses the budget (a still screen
            // proves nothing), except from deep below the default, where
            // waiting for a busy screen would leave a squeeze in place.
            let quiet =
                self.hold_increase_until.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(1500));
            let g: f64 = if quiet { 2.0 } else { 1.08 };
            self.target_kbps = ((self.target_kbps as f64 * g.powf(dt)) as u32 + 1).min(self.limits.lan_target());
        }
        // A client whose decoder falls behind (more than two frame times per
        // frame) builds a backlog of latency; step the frame rate down for
        // 3 s at a time until it keeps up. One slow window is a hiccup, not
        // a backlog (3 in a row are), and after a step the queued frames
        // take a while to drain, so steps are at least 500 ms apart.
        let frame_us = 1_000_000 / self.fps.max(1);
        if fb.decode_us > 2 * frame_us {
            self.slow_windows += 1;
            if let Some((_, until)) = &mut self.decode_cap {
                *until = now + Duration::from_secs(3);
            }
        } else {
            self.slow_windows = 0;
        }
        if self.slow_windows >= 3
            && self.last_decode_step.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(500))
        {
            let below = FPS_STEPS.iter().copied().find(|f| *f < self.fps).unwrap_or(5);
            let cap = self.decode_cap.map_or(below, |(c, _)| c.min(below));
            self.decode_cap = Some((cap, now + Duration::from_secs(3)));
            self.last_decode_step = Some(now);
            self.slow_windows = 0;
        } else if self.decode_cap.is_some_and(|(_, until)| now >= until) {
            self.decode_cap = None;
        }
        // Parity follows loss: twice the observed loss plus 5%.
        self.parity = (self.loss_ewma * 2.0 + 0.05).clamp(0.10, 0.50);
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
        let ceiling = self.decode_cap.map_or(self.limits.max_fps, |(c, _)| c.min(self.limits.max_fps));
        let steps: Vec<u32> = FPS_STEPS.iter().copied().filter(|f| *f <= ceiling).collect();
        let mut fps = self.fps.min(ceiling);
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
    fn starts_at_the_default_and_cuts_only_on_persistent_loss() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
        assert_eq!(r.target_kbps, 31_104);
        assert_eq!(r.fps, 60);
        let ms = |n| t0 + Duration::from_millis(n);
        // One lossy window (a burst): no cut.
        r.on_feedback(&fb(190_000, 0, 1, (0, 0, 0)), ms(0));
        r.on_feedback(&fb(190_000, 0, 0, (0, 0, 0)), ms(50));
        assert_eq!(r.target_kbps, 31_104);
        // Two in a row: cut to 85% of what arrived (30.4 Mbit/s).
        r.on_feedback(&fb(190_000, 0, 1, (0, 0, 0)), ms(100));
        r.on_feedback(&fb(190_000, 0, 1, (0, 0, 0)), ms(150));
        assert_eq!(r.target_kbps, 25_840);
        // No growth for 500 ms after a cut, then back to the default.
        r.on_feedback(&fb(190_000, 0, 0, (0, 0, 0)), ms(300));
        assert_eq!(r.target_kbps, 25_840);
        for i in 0..100 {
            r.on_feedback(&fb(190_000, 0, 0, (0, 0, 0)), ms(700 + i * 50));
        }
        assert_eq!(r.target_kbps, 31_104, "recovers to the default, not beyond");
    }

    #[test]
    fn jitter_alone_does_not_cut() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
        // Wi-Fi: delay between 10 and 50 ms, slope up and down window by
        // window, nothing lost.
        for i in 0..400u64 {
            let up = i % 2 == 0;
            let f = fb(190_000, 0, 0, (10_000, if up { 50_000 } else { 20_000 }, if up { 8_000 } else { -8_000 }));
            r.on_feedback(&f, t0 + Duration::from_millis(i * 50));
        }
        assert_eq!(r.target_kbps, 31_104);
        assert_eq!(r.fps, 60);
    }

    #[test]
    fn converges_below_a_slow_link_and_drops_fps_first() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
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

        // The squeeze lifts: back at full rate within 5 s.
        for _ in 0..100 {
            t += Duration::from_millis(50);
            let f = fb(r.target_kbps * 50 / 8, 0, 0, (25_000, 26_000, 0));
            r.on_feedback(&f, t);
        }
        assert_eq!(r.fps, 60, "target {}", r.target_kbps);
    }

    #[test]
    fn a_squeeze_with_mostly_empty_windows_still_cuts() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
        // 1 Mbit/s link while sending ~31: a frame gets through every
        // fourth window, each later than the last.
        let mut t = t0;
        for i in 0..40u32 {
            t += Duration::from_millis(50);
            let f = if i % 4 == 0 {
                let d = 30_000 + i as i32 * 20_000;
                Feedback { frames_rx: 1, ..fb(6_000, 0, 0, (d, d + 10_000, 20_000)) }
            } else {
                Feedback { frames_rx: 0, shards_rx: 0, ..fb(0, 0, 0, (0, 0, 0)) }
            };
            r.on_feedback(&f, t);
        }
        assert!(r.target_kbps < 5_000, "target {}", r.target_kbps);
    }

    #[test]
    fn slow_decoder_lowers_fps_then_recovers() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
        let mut f = fb(190_000, 0, 0, (0, 0, 0));
        let ms = |n| t0 + Duration::from_millis(n);
        f.decode_us = 80_000; // 80 ms per frame at 60 fps
        r.on_feedback(&f, ms(0));
        assert_eq!(r.fps, 60, "one slow window is a hiccup");
        f.decode_us = 3_000;
        r.on_feedback(&f, ms(50));
        f.decode_us = 80_000;
        for t in [100, 150, 200] {
            r.on_feedback(&f, ms(t));
        }
        assert_eq!(r.fps, 30, "three in a row: a backlog");
        for t in [250, 300, 350] {
            r.on_feedback(&f, ms(t));
        }
        assert_eq!(r.fps, 30, "the backlog drains before the next step");
        for t in [750, 800, 850] {
            r.on_feedback(&f, ms(t));
        }
        assert_eq!(r.fps, 15, "still behind: another step down");
        f.decode_us = 3_000;
        r.on_feedback(&f, ms(4_000));
        assert_eq!(r.fps, 60, "back to full rate once the cap expires");
    }

    #[test]
    fn parity_tracks_loss() {
        let t0 = Instant::now();
        let mut r = RateCtl::new(limits(), t0);
        for i in 0..40 {
            r.on_feedback(&fb(10_000, 10, 0, (0, 0, 0)), t0 + Duration::from_millis(i * 50));
        }
        // 10 lost of 110 sent: 9.1% loss → 2 × 9.1% + 5% = 23%.
        assert!((0.22..=0.24).contains(&r.parity), "parity {}", r.parity);
    }

    #[test]
    fn a_still_screen_is_not_congestion() {
        let t0 = Instant::now();
        let ms = |v: u64| t0 + Duration::from_millis(v);
        let mut r = RateCtl::new(limits(), t0);
        let start = r.target_kbps;
        // Little sent; a big frame now and then arrives 150 ms late.
        for i in 0..40 {
            r.note_sent(2_000);
            r.on_feedback(&fb(2_000, 0, 0, (1_000, 151_000, 10_000)), ms(i * 50));
        }
        assert_eq!(r.target_kbps, start);
        assert_eq!(r.fps, 60);
        // A lost frame while idle trims the target; it doesn't drop it to
        // what a still screen delivers.
        r.note_sent(2_000);
        r.on_feedback(&fb(2_000, 0, 1, (0, 0, 0)), ms(2_000));
        r.note_sent(2_000);
        r.on_feedback(&fb(2_000, 0, 1, (0, 0, 0)), ms(2_050));
        assert!(r.target_kbps >= start * 8 / 10, "{} from {start}", r.target_kbps);
    }

    #[test]
    fn a_restarted_stream_gets_a_grace_second() {
        let t0 = Instant::now();
        let ms = |v: u64| t0 + Duration::from_millis(v);
        let mut r = RateCtl::new(limits(), t0);
        r.grace(t0);
        let start = r.target_kbps;
        for i in 0..10 {
            r.on_feedback(&fb(5_000, 0, 2, (0, 0, 0)), ms(i * 50));
        }
        assert_eq!(r.target_kbps, start);
        // After it, loss counts again.
        r.on_feedback(&fb(5_000, 0, 2, (0, 0, 0)), ms(1_100));
        r.on_feedback(&fb(5_000, 0, 2, (0, 0, 0)), ms(1_150));
        assert!(r.target_kbps < start);
    }
}
