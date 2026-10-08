//! How the remote picture sits in the window: a base scale from the view
//! mode (Fit, Fill window, 100%), times the user's zoom, and a pan offset
//! when the picture is larger than the window. All in window pixels; the
//! single source for drawing the picture and mapping the pointer.

use crate::settings::ViewMode;

pub const ZOOM_MIN: f32 = 0.25;
pub const ZOOM_MAX: f32 = 4.0;

#[derive(Debug, Clone)]
pub struct View {
    pub mode: ViewMode,
    /// Window pixels per remote pixel at 100% (the window's scale factor
    /// over the pixel ratio).
    pub natural: f32,
    pub zoom: f32,
    /// The picture's top-left in window pixels.
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    win: (f32, f32),
    pic: (f32, f32),
}

impl Default for View {
    fn default() -> Self {
        Self { mode: ViewMode::Fit, natural: 1.0, zoom: 1.0, x: 0.0, y: 0.0, scale: 1.0, win: (1.0, 1.0), pic: (1.0, 1.0) }
    }
}

impl View {
    fn base(&self) -> f32 {
        let fit = (self.win.0 / self.pic.0).min(self.win.1 / self.pic.1);
        match self.mode {
            // Fit shrinks a large picture but never enlarges past 100%.
            ViewMode::Fit => fit.min(self.natural),
            ViewMode::Fill => fit,
            ViewMode::Native => self.natural,
        }
    }

    /// Recomputes for a window and picture size, keeping the window's
    /// centre on the same remote point and the pan within bounds.
    pub fn layout(&mut self, win: (u32, u32), pic: (u32, u32)) {
        let (w, h) = (win.0.max(1) as f32, win.1.max(1) as f32);
        let (pw, ph) = (pic.0.max(1) as f32, pic.1.max(1) as f32);
        let prev = self.scale;
        let centre = ((self.win.0 / 2.0 - self.x) / prev, (self.win.1 / 2.0 - self.y) / prev);
        (self.win, self.pic) = ((w, h), (pw, ph));
        self.scale = self.base() * self.zoom;
        if (self.scale - prev).abs() > f32::EPSILON {
            self.x = w / 2.0 - centre.0 * self.scale;
            self.y = h / 2.0 - centre.1 * self.scale;
        }
        self.clamp();
    }

    fn clamp(&mut self) {
        let (dw, dh) = (self.pic.0 * self.scale, self.pic.1 * self.scale);
        self.x = if dw <= self.win.0 { (self.win.0 - dw) / 2.0 } else { self.x.clamp(self.win.0 - dw, 0.0) };
        self.y = if dh <= self.win.1 { (self.win.1 - dh) / 2.0 } else { self.y.clamp(self.win.1 - dh, 0.0) };
    }

    /// The picture's rectangle in window pixels: `(x, y, w, h)`.
    pub fn rect(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.pic.0 * self.scale, self.pic.1 * self.scale)
    }

    pub fn pans(&self) -> bool {
        self.pic.0 * self.scale > self.win.0 + 0.5 || self.pic.1 * self.scale > self.win.1 + 0.5
    }

    /// Zooms to `z`, keeping the remote point under `at` (window pixels;
    /// the centre when None) where it is.
    pub fn set_zoom(&mut self, z: f32, at: Option<(f32, f32)>) {
        let (px, py) = at.unwrap_or((self.win.0 / 2.0, self.win.1 / 2.0));
        let (rx, ry) = ((px - self.x) / self.scale, (py - self.y) / self.scale);
        self.zoom = z.clamp(ZOOM_MIN, ZOOM_MAX);
        self.scale = self.base() * self.zoom;
        self.x = px - rx * self.scale;
        self.y = py - ry * self.scale;
        self.clamp();
    }

    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.x += dx;
        self.y += dy;
        self.clamp();
    }

    /// The remote pixel under a window position, or None outside.
    pub fn to_remote(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let sx = (x as f32 - self.x) / self.scale;
        let sy = (y as f32 - self.y) / self.scale;
        if sx < 0.0 || sy < 0.0 || sx >= self.pic.0 || sy >= self.pic.1 {
            return None;
        }
        Some((sx as u16, sy as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: ViewMode, win: (u32, u32), pic: (u32, u32)) -> View {
        let mut v = View { mode, ..Default::default() };
        v.layout(win, pic);
        v
    }

    #[test]
    fn fit_shrinks_but_never_enlarges() {
        let v = view(ViewMode::Fit, (1000, 700), (1920, 1080));
        assert!((v.rect().2 - 1000.0).abs() < 0.5, "{:?}", v.rect());
        let small = view(ViewMode::Fit, (2000, 1200), (1280, 720));
        assert_eq!(small.rect().2, 1280.0);
        let fill = view(ViewMode::Fill, (2000, 1200), (1280, 720));
        assert!((fill.rect().2 - 2000.0).abs() < 0.5);
    }

    #[test]
    fn maps_points_at_any_zoom_and_pan() {
        let mut v = view(ViewMode::Fit, (1100, 630), (1920, 1080));
        let at = |v: &View, rx: f32, ry: f32| v.to_remote((v.x + (rx + 0.5) * v.scale) as f64, (v.y + (ry + 0.5) * v.scale) as f64);
        assert_eq!(at(&v, 400.0, 300.0), Some((400, 300)));
        v.set_zoom(2.0, Some((500.0, 300.0)));
        v.pan_by(-5000.0, -5000.0);
        assert!(v.pans());
        assert_eq!(at(&v, 1700.0, 900.0), Some((1700, 900)));
        // Zooming keeps the point under the cursor.
        let before = v.to_remote(600.0, 400.0);
        v.set_zoom(3.0, Some((600.0, 400.0)));
        let after = v.to_remote(600.0, 400.0);
        let (b, a) = (before.unwrap(), after.unwrap());
        assert!(b.0.abs_diff(a.0) <= 1 && b.1.abs_diff(a.1) <= 1, "{b:?} {a:?}");
    }
}
