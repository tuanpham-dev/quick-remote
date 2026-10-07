//! Virtual-display capture straight from the compositor with
//! wlr-screencopy: no portal, no PipeWire. Frames come only when the
//! output changes (`copy_with_damage`); a still screen repeats the last
//! frame every `keepalive` so the encoder can sharpen it.
//!
//! (pipewiresrc 1.4 could not take xdg-desktop-portal-wlr's format
//! renegotiation, and stalled at random on stream start.)

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self as frame, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    manager: Option<ZwlrScreencopyManagerV1>,
    outputs: Vec<(wl_output::WlOutput, String)>,
    // The frame in progress.
    shm_buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

impl State {
    fn reset_frame(&mut self) {
        self.shm_buffer = None;
        self.buffer_done = false;
        self.y_invert = false;
        self.ready = false;
        self.failed = false;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => st.manager = Some(reg.bind(name, version.min(3), qh, ())),
                // Version 4 announces the output's name.
                "wl_output" if version >= 4 => {
                    let i = st.outputs.len();
                    st.outputs.push((reg.bind(name, 4, qh, i), String::new()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(st: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            st.outputs[*i].1 = name;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(st: &mut Self, f: &ZwlrScreencopyFrameV1, ev: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            frame::Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                st.shm_buffer = Some((format, width, height, stride));
                // Before version 3 the shm buffer is the only offer.
                if f.version() < 3 {
                    st.buffer_done = true;
                }
            }
            frame::Event::BufferDone => st.buffer_done = true,
            frame::Event::Flags { flags: WEnum::Value(fl) } => st.y_invert = fl.contains(frame::Flags::YInvert),
            frame::Event::Ready { .. } => st.ready = true,
            frame::Event::Failed => st.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ZwlrScreencopyManagerV1);

/// Reads and dispatches events for up to `timeout`.
fn dispatch_for(q: &mut EventQueue<State>, st: &mut State, timeout: Duration) -> Result<()> {
    q.dispatch_pending(st)?;
    q.flush()?;
    if let Some(guard) = q.prepare_read() {
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
        if n > 0 {
            guard.read()?;
        }
    }
    q.dispatch_pending(st)?;
    Ok(())
}

/// A shared-memory buffer the compositor copies into.
struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: *mut u8,
    len: usize,
    key: (wl_shm::Format, u32, u32, u32),
    _fd: OwnedFd,
}

// SAFETY: the mapping is owned by this struct and used from one thread.
unsafe impl Send for ShmBuffer {}

impl ShmBuffer {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, key: (wl_shm::Format, u32, u32, u32)) -> Result<Self> {
        let (format, w, h, stride) = key;
        let len = stride as usize * h as usize;
        // SAFETY: plain syscalls; the fd is owned right away.
        let fd = unsafe {
            let raw = libc::memfd_create(c"qr-screencopy".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(raw)
        };
        // SAFETY: valid fd and length.
        let map = unsafe {
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) < 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if map == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
        Ok(Self { buffer, pool, map: map as *mut u8, len, key, _fd: fd })
    }

    /// The picture, rows tightly packed and top first.
    fn packed(&self, y_invert: bool) -> Vec<u8> {
        let (_, w, h, stride) = self.key;
        let (row, stride) = (w as usize * 4, stride as usize);
        // SAFETY: the mapping is `len` bytes and stays mapped while self lives.
        let src = unsafe { std::slice::from_raw_parts(self.map, self.len) };
        let mut out = Vec::with_capacity(row * h as usize);
        for r in 0..h as usize {
            let s = if y_invert { h as usize - 1 - r } else { r } * stride;
            out.extend_from_slice(&src[s..s + row]);
        }
        out
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

/// GStreamer raw format of a wl_shm format (little-endian byte order).
fn gst_format(f: wl_shm::Format) -> Option<&'static str> {
    Some(match f {
        wl_shm::Format::Xrgb8888 => "BGRx",
        wl_shm::Format::Argb8888 => "BGRA",
        wl_shm::Format::Xbgr8888 => "RGBx",
        wl_shm::Format::Abgr8888 => "RGBA",
        _ => return None,
    })
}

/// A compositor connection set up to capture one output.
pub struct Opened {
    queue: EventQueue<State>,
    state: State,
    output: wl_output::WlOutput,
    buffer: ShmBuffer,
    pub width: u32,
    pub height: u32,
    /// GStreamer raw format of the frames.
    pub format: &'static str,
    _conn: Connection,
}

/// Connects to the compositor at `socket` and prepares to capture
/// `output` (learning its frame size and format).
pub fn open(socket: &Path, output: &str) -> Result<Opened> {
    let stream = UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?;
    let conn = Connection::from_socket(stream)?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let manager = state.manager.clone().context("the compositor has no wlr-screencopy")?;
    let shm = state.shm.clone().context("the compositor has no wl_shm")?;
    let out = state
        .outputs
        .iter()
        .find(|(_, n)| n == output)
        .map(|(o, _)| o.clone())
        .with_context(|| format!("no output {output}"))?;

    // Ask for one frame to learn the buffer the compositor wants.
    let f = manager.capture_output(1, &out, &qh, ());
    let end = Instant::now() + Duration::from_secs(3);
    while !state.buffer_done && !state.failed {
        if Instant::now() > end {
            bail!("the compositor offered no frame buffer");
        }
        dispatch_for(&mut queue, &mut state, Duration::from_millis(100))?;
    }
    f.destroy();
    let key = state.shm_buffer.context("the compositor offered no shared-memory buffer")?;
    let format = gst_format(key.0).with_context(|| format!("unsupported frame format {:?}", key.0))?;
    let buffer = ShmBuffer::new(&shm, &qh, key)?;
    state.reset_frame();
    Ok(Opened { queue, state, output: out, buffer, width: key.1, height: key.2, format, _conn: conn })
}

/// A running capture; dropping it stops the thread.
pub struct ScreenCopy {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ScreenCopy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Opened {
    /// Captures on a thread, handing each frame (tightly packed rows) to
    /// `on_frame`.
    pub fn run(self, keepalive: Duration, mut on_frame: impl FnMut(Vec<u8>) + Send + 'static) -> ScreenCopy {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("screencopy".into())
            .spawn(move || {
                if let Err(e) = self.capture_loop(&stop2, keepalive, &mut on_frame) {
                    tracing::warn!("screen capture stopped: {e:#}");
                }
            })
            .expect("spawning the capture thread");
        ScreenCopy { stop, thread: Some(thread) }
    }

    fn capture_loop(mut self, stop: &AtomicBool, keepalive: Duration, on_frame: &mut dyn FnMut(Vec<u8>)) -> Result<()> {
        let qh = self.queue.handle();
        let manager = self.state.manager.clone().expect("checked in open");
        let mut last: Option<Vec<u8>> = None;
        let mut last_sent = Instant::now();
        let mut warned = false;
        while !stop.load(Ordering::Relaxed) {
            self.state.reset_frame();
            let f = manager.capture_output(1, &self.output, &qh, ());
            while !self.state.buffer_done && !self.state.failed && !stop.load(Ordering::Relaxed) {
                dispatch_for(&mut self.queue, &mut self.state, Duration::from_millis(100))?;
            }
            if self.state.failed || self.state.shm_buffer != Some(self.buffer.key) {
                // The output changed size (a resize restarts the stream
                // with a new capture) or went away.
                if !warned {
                    tracing::info!(offered = ?self.state.shm_buffer, "capture paused: the output changed");
                    warned = true;
                }
                f.destroy();
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // The first frame unconditionally; then only on change.
            if last.is_none() {
                f.copy(&self.buffer.buffer);
            } else {
                f.copy_with_damage(&self.buffer.buffer);
            }
            while !self.state.ready && !self.state.failed && !stop.load(Ordering::Relaxed) {
                let wait = keepalive.saturating_sub(last_sent.elapsed()).min(Duration::from_millis(100));
                dispatch_for(&mut self.queue, &mut self.state, wait)?;
                if !self.state.ready
                    && let Some(l) = &last
                    && last_sent.elapsed() >= keepalive
                {
                    on_frame(l.clone());
                    last_sent = Instant::now();
                }
            }
            if self.state.ready {
                let p = self.buffer.packed(self.state.y_invert);
                on_frame(p.clone());
                last = Some(p);
                last_sent = Instant::now();
            }
            f.destroy();
        }
        Ok(())
    }
}
