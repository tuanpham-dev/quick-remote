//! Stand-ins for the Linux-only sources (virtual display, Wayland and
//! GNOME desktops) on other systems: they can't be started, so the code
//! that uses them compiles unchanged and never reaches the rest.

// Never built: these exist only to type-check the shared code.
#![allow(dead_code)]

pub mod vdisplay {
    use std::convert::Infallible;
    use std::path::PathBuf;

    use anyhow::{Result, bail};
    use qr_proto::Monitor;

    pub struct VirtualDisplay {
        pub runtime: PathBuf,
        never: Infallible,
    }

    impl VirtualDisplay {
        pub fn start(
            _w: u32,
            _h: u32,
            _refresh: u32,
            _desktop: Option<&str>,
            _gpu: bool,
            _layout: Option<&str>,
        ) -> Result<Self> {
            bail!("the virtual display needs Linux")
        }
        pub fn add_output(&self, _w: u32, _h: u32, _refresh: u32) -> Result<String> {
            match self.never {}
        }
        pub fn wayland_socket(&self) -> PathBuf {
            match self.never {}
        }
        pub fn set_mode(&self, _output: &str, _w: u32, _h: u32, _refresh: u32) -> Result<()> {
            match self.never {}
        }
        pub fn set_scale(&self, _output: &str, _scale_pct: u16) -> Result<()> {
            match self.never {}
        }
        pub fn shutdown(&mut self) {
            match self.never {}
        }
        pub fn monitors(&self) -> Result<Vec<Monitor>> {
            match self.never {}
        }
        pub fn pipewire_fd(&self) -> Result<i32> {
            match self.never {}
        }
        pub fn output_state(&self) -> Result<(u32, u32, f64)> {
            match self.never {}
        }
        pub fn sway_socket(&self) -> PathBuf {
            match self.never {}
        }
        pub fn env(&self) -> &[(String, String)] {
            match self.never {}
        }
    }
}

pub mod xdesktop {
    use std::convert::Infallible;

    use anyhow::{Result, bail};

    pub struct XDesktop {
        pub display: String,
        never: Infallible,
    }

    impl XDesktop {
        pub fn start(
            _vd: &super::vdisplay::VirtualDisplay,
            _command: &str,
            _layout: Option<&str>,
            _dpi: u32,
        ) -> Result<Self> {
            bail!("the managed desktop needs Linux")
        }
        pub fn env(&self) -> Vec<(String, String)> {
            match self.never {}
        }
        pub fn shutdown(&mut self) {
            match self.never {}
        }
    }
}

pub mod mutter {
    use std::convert::Infallible;

    use anyhow::{Result, bail};
    use qr_proto::Monitor;

    use crate::input::Injector;

    pub struct MutterSession {
        never: Infallible,
    }

    pub const PENDING_MONITOR: u8 = 254;

    /// Stand-in for a D-Bus object path.
    pub struct Path;

    impl MutterSession {
        pub async fn start() -> Result<Self> {
            bail!("GNOME's remote desktop needs Linux")
        }
        pub fn monitors(&self) -> Vec<Monitor> {
            match self.never {}
        }
        pub async fn stream(&self, _monitor: u8) -> Result<(Monitor, u32)> {
            match self.never {}
        }
        pub fn injector(&self) -> Result<MutterInput> {
            match self.never {}
        }
        pub async fn record_virtual(&self) -> Result<(Path, u32)> {
            match self.never {}
        }
        pub fn add_virtual(&self, _name: &str, _monitor: Monitor, _path: Path, _node: u32) -> u8 {
            match self.never {}
        }
        pub fn virtual_monitor(&self, _name: &str) -> Option<Monitor> {
            match self.never {}
        }
        pub fn is_virtual(&self, _id: u8) -> bool {
            match self.never {}
        }
        pub async fn stop_virtual(&self, _name: &str) {
            match self.never {}
        }
        #[allow(clippy::type_complexity)]
        pub async fn layout(&self) -> Result<Vec<(String, i32, i32, u32, u32, u32, bool)>> {
            match self.never {}
        }
        pub async fn refresh(&self) -> Result<()> {
            match self.never {}
        }
        pub async fn configure(
            &self,
            _connector: &str,
            _mode: Option<(u32, u32, u32)>,
            _scale_pct: u16,
            _primary: bool,
        ) -> Result<(u16, bool)> {
            match self.never {}
        }
    }

    pub struct MutterInput {
        never: Infallible,
    }

    impl Injector for MutterInput {
        fn key(&mut self, _: u16, _: bool) -> Result<()> {
            match self.never {}
        }
        fn button(&mut self, _: u8, _: bool) -> Result<()> {
            match self.never {}
        }
        fn abs(&mut self, _: u16, _: u16) -> Result<()> {
            match self.never {}
        }
        fn rel(&mut self, _: i32, _: i32) -> Result<()> {
            match self.never {}
        }
        fn wheel(&mut self, _: i32, _: i32) -> Result<()> {
            match self.never {}
        }
        fn text(&mut self, _: &str) -> Result<()> {
            match self.never {}
        }
        fn name(&self) -> &'static str {
            match self.never {}
        }
    }
}

pub mod kwin {
    use std::convert::Infallible;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, bail};
    use qr_proto::Monitor;

    pub struct KwinSession {
        pub socket: PathBuf,
        never: Infallible,
    }

    pub fn supported(_socket: &Path) -> bool {
        false
    }

    impl KwinSession {
        pub fn start(_socket: &Path, _fps: u32) -> Result<Self> {
            bail!("KDE's screencast needs Linux")
        }
        pub fn monitors(&self) -> Vec<Monitor> {
            match self.never {}
        }
        pub fn stream(&self, _monitor: &Monitor) -> Result<u32> {
            match self.never {}
        }
        pub fn can_create_virtual(&self) -> bool {
            match self.never {}
        }
        pub fn is_ours(&self, _name: &str) -> bool {
            match self.never {}
        }
        pub fn create_virtual(&self, _name: &str, _size: (u32, u32), _scale_pct: u16) -> Result<u32> {
            match self.never {}
        }
        pub fn virtual_node(&self, _name: &str) -> Option<Result<u32>> {
            match self.never {}
        }
        pub fn remove_virtual(&self, _name: &str) {
            match self.never {}
        }
        pub fn injector(&self) -> Result<super::mutter::MutterInput> {
            match self.never {}
        }
    }

    pub fn kscreen(_args: &[&str]) -> Result<String> {
        bail!("KDE needs Linux")
    }
    pub fn kscreen_modes(_name: &str) -> Result<Vec<(u32, u32, u32)>> {
        bail!("KDE needs Linux")
    }
    pub fn kscreen_scale_pct(_name: &str) -> Result<u16> {
        bail!("KDE needs Linux")
    }
}

pub mod screencopy {
    use std::convert::Infallible;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Result, bail};
    use qr_proto::Monitor;

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Format {
        Raw(&'static str),
        DmaNv12(u64),
    }

    pub enum Frame<'a> {
        Packed(Vec<u8>),
        // Keeps the lifetime used by the Linux type.
        Dma(&'a Infallible),
    }

    pub struct Opened {
        pub width: u32,
        pub height: u32,
        pub format: Format,
        never: Infallible,
    }

    pub struct ScreenCopy;

    pub fn supported(_socket: &Path) -> bool {
        false
    }

    pub fn monitors(_socket: &Path, _fps: u32) -> Result<Vec<Monitor>> {
        bail!("Wayland capture needs Linux")
    }

    pub fn open(_socket: &Path, _output: &str, _gpu: bool, _overlay_cursor: bool) -> Result<Opened> {
        bail!("Wayland capture needs Linux")
    }

    impl Opened {
        pub fn run(self, _keepalive: Duration, _on_frame: impl FnMut(Frame<'_>) + Send + 'static) -> ScreenCopy {
            match self.never {}
        }
    }
}
