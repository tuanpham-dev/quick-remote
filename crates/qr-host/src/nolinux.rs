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
        pub fn start(_w: u32, _h: u32, _refresh: u32, _desktop: Option<&str>, _gpu: bool) -> Result<Self> {
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
        pub fn monitors(&self) -> Result<Vec<Monitor>> {
            match self.never {}
        }
        pub fn pipewire_fd(&self) -> Result<i32> {
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

    impl MutterSession {
        pub async fn start() -> Result<Self> {
            bail!("GNOME's remote desktop needs Linux")
        }
        pub fn monitors(&self) -> Vec<Monitor> {
            match self.never {}
        }
        pub fn node(&self, _monitor: u8) -> Option<u32> {
            match self.never {}
        }
        pub fn injector(&self) -> Result<MutterInput> {
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

    pub fn open(_socket: &Path, _output: &str, _gpu: bool) -> Result<Opened> {
        bail!("Wayland capture needs Linux")
    }

    impl Opened {
        pub fn run(self, _keepalive: Duration, _on_frame: impl FnMut(Frame<'_>) + Send + 'static) -> ScreenCopy {
            match self.never {}
        }
    }
}
