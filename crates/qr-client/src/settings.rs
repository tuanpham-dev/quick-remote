//! Display settings, kept in the client's config directory
//! (`display.json`): the remote desktop's size (follow the window or a
//! fixed resolution), its pixel ratio, which monitor on KDE/GNOME, and how
//! the picture is shown.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Fit,
    Fill,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    #[default]
    Follow,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Virtual,
    Real,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: SizeMode,
    pub fixed_w: u32,
    pub fixed_h: u32,
    pub hz: u32,
    /// Remote pixels per logical pixel; 0 = the window's own (at most 2).
    pub ratio: f32,
    pub target: Target,
    pub view: ViewMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: SizeMode::Follow,
            fixed_w: 1920,
            fixed_h: 1080,
            hz: 60,
            ratio: 0.0,
            target: Target::Virtual,
            view: ViewMode::Fit,
        }
    }
}

pub const PRESETS: [(u32, u32); 5] = [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440), (3840, 2160)];
pub const RATES: [u32; 3] = [30, 60, 120];
pub const RATIOS: [f32; 3] = [1.0, 1.5, 2.0];

fn path(dir: &Path) -> PathBuf {
    dir.join("display.json")
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path(dir), b);
        }
    }

    /// The pixel ratio in effect for a window with `scale_factor`.
    pub fn ratio(&self, scale_factor: f64) -> f32 {
        if self.ratio > 0.0 { self.ratio } else { (scale_factor as f32).clamp(1.0, 2.0) }
    }
}

/// An even size within what hosts take.
pub fn clamp_size(w: f64, h: f64) -> (u16, u16) {
    let even = |v: f64, lo: f64, hi: f64| (v.round().clamp(lo, hi) as u16) & !1;
    (even(w, 640.0, 7680.0), even(h, 360.0, 4320.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults() {
        let dir = std::env::temp_dir().join(format!("qr-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        let s = Settings { mode: SizeMode::Fixed, fixed_w: 2560, fixed_h: 1440, ratio: 1.5, ..Default::default() };
        s.save(&dir);
        assert_eq!(Settings::load(&dir), s);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(clamp_size(1001.0, 701.0), (1000, 700));
    }
}
