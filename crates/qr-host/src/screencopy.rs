//! Virtual-display capture straight from the compositor with
//! wlr-screencopy: no portal, no PipeWire. Frames come only when the
//! output changes (`copy_with_damage`); a still screen repeats the last
//! frame every `keepalive` so the encoder can sharpen it.
//!
//! When the compositor offers DMA-BUF frames, it copies on the GPU into
//! our buffer and `gpuconv` makes NV12 for the encoder there (zero copy);
//! otherwise frames come through shared memory. `QR_VD_CAPTURE=shm` forces
//! the latter.
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
use qr_media::encode::DmaFrame;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self as frame, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    manager: Option<ZwlrScreencopyManagerV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    outputs: Vec<(wl_output::WlOutput, String)>,
    // The frame in progress.
    shm_buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    /// DMA-BUF offer: DRM fourcc, width, height.
    dma_buffer: Option<(u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

impl State {
    fn reset_frame(&mut self) {
        self.shm_buffer = None;
        self.dma_buffer = None;
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
                "zwp_linux_dmabuf_v1" if version >= 2 => st.dmabuf = Some(reg.bind(name, version.min(3), qh, ())),
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
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        ev: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = ev {
            st.outputs[*i].1 = name;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        st: &mut Self,
        f: &ZwlrScreencopyFrameV1,
        ev: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            frame::Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                st.shm_buffer = Some((format, width, height, stride));
                // Before version 3 the shm buffer is the only offer.
                if f.version() < 3 {
                    st.buffer_done = true;
                }
            }
            frame::Event::LinuxDmabuf { format, width, height } => st.dma_buffer = Some((format, width, height)),
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
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

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
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
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

/// What frames come as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    /// Tightly packed rows of this GStreamer raw format.
    Raw(&'static str),
    /// NV12 DMA-BUFs with this DRM modifier.
    DmaNv12(u64),
}

/// A captured frame.
pub enum Frame<'a> {
    Packed(Vec<u8>),
    Dma(&'a DmaFrame),
}

/// Where the compositor copies to.
enum Target {
    Shm(ShmBuffer),
    Gpu { conv: Box<crate::gpuconv::GpuConv>, buffer: wl_buffer::WlBuffer, offer: (u32, u32, u32) },
}

/// A compositor connection set up to capture one output.
pub struct Opened {
    queue: EventQueue<State>,
    state: State,
    output: wl_output::WlOutput,
    target: Target,
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// Whether the compositor draws the pointer into the copies.
    overlay_cursor: bool,
    _conn: Connection,
}

/// NV12 buffers for the encoder: it keeps an input buffer while its
/// picture is a reference, plus one queued and two in the source.
const OUTPUTS: usize = 6;

/// The GPU target for a DMA-BUF offer, if everything on the way works.
fn gpu_target(state: &State, qh: &QueueHandle<State>, offer: (u32, u32, u32)) -> Result<Target> {
    let dmabuf = state.dmabuf.as_ref().context("the compositor has no linux-dmabuf")?;
    let node = std::env::var("QR_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let (fourcc, w, h) = offer;
    let conv = crate::gpuconv::GpuConv::new(&node, w, h, fourcc, OUTPUTS)?;
    let params = dmabuf.create_params(qh, ());
    let bo = &conv.capture;
    for (i, (fd, offset, stride)) in bo.planes.iter().enumerate() {
        params.add(fd.as_fd(), i as u32, *offset, *stride, (bo.modifier >> 32) as u32, bo.modifier as u32);
    }
    let buffer = params.create_immed(w as i32, h as i32, fourcc, zwp_linux_buffer_params_v1::Flags::empty(), qh, ());
    params.destroy();
    Ok(Target::Gpu { conv: Box::new(conv), buffer, offer })
}

/// Connects to the compositor at `socket` and prepares to capture
/// `output` (learning its frame size and format). `gpu`: zero-copy NV12
/// frames are acceptable (not for 4:4:4 encoding). `overlay_cursor`: the
/// pointer is drawn into the picture (off when the client draws it from
/// forwarded shapes).
pub fn open(socket: &Path, output: &str, gpu: bool, overlay_cursor: bool) -> Result<Opened> {
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
    let gpu = match (state.dma_buffer, std::env::var("QR_VD_CAPTURE").as_deref()) {
        _ if !gpu => None,
        (_, Ok("shm")) => None,
        (None, _) => {
            tracing::info!("the compositor offers no DMA-BUF frames: capturing through shared memory");
            None
        }
        (Some(offer), _) => match gpu_target(&state, &qh, offer) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("zero-copy capture is off, using shared memory: {e:#}");
                None
            }
        },
    };
    if let Some(target) = gpu {
        let Target::Gpu { conv, .. } = &target else { unreachable!() };
        let (w, h, modifier) = (conv.capture.width, conv.capture.height, crate::gpuconv::I915_Y_TILED);
        tracing::info!(w, h, "zero-copy capture");
        state.reset_frame();
        return Ok(Opened {
            queue,
            state,
            output: out,
            target,
            width: w,
            height: h,
            format: Format::DmaNv12(modifier),
            overlay_cursor,
            _conn: conn,
        });
    }
    let key = state.shm_buffer.context("the compositor offered no shared-memory buffer")?;
    let format = gst_format(key.0).with_context(|| format!("unsupported frame format {:?}", key.0))?;
    let buffer = ShmBuffer::new(&shm, &qh, key)?;
    state.reset_frame();
    Ok(Opened {
        queue,
        state,
        output: out,
        target: Target::Shm(buffer),
        overlay_cursor,
        width: key.1,
        height: key.2,
        format: Format::Raw(format),
        _conn: conn,
    })
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
    /// Captures on a thread, handing each frame to `on_frame`.
    pub fn run(self, keepalive: Duration, mut on_frame: impl FnMut(Frame<'_>) + Send + 'static) -> ScreenCopy {
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

    fn capture_loop(
        mut self,
        stop: &AtomicBool,
        keepalive: Duration,
        on_frame: &mut dyn FnMut(Frame<'_>),
    ) -> Result<()> {
        let qh = self.queue.handle();
        if let Target::Gpu { conv, .. } = &self.target {
            conv.bind()?;
        }
        let manager = self.state.manager.clone().expect("checked in open");
        // A full copy (not waiting for damage): the first frame, and a still
        // screen's repeat after `keepalive`.
        let mut full = true;
        let mut last_sent = Instant::now();
        let mut warned = false;
        // Per-second timing (debug log): frames, compositor copy, packing.
        let (mut n, mut copy_t, mut pack_t, mut since) = (0u32, Duration::ZERO, Duration::ZERO, Instant::now());
        while !stop.load(Ordering::Relaxed) {
            self.state.reset_frame();
            let f = manager.capture_output(self.overlay_cursor as i32, &self.output, &qh, ());
            while !self.state.buffer_done && !self.state.failed && !stop.load(Ordering::Relaxed) {
                dispatch_for(&mut self.queue, &mut self.state, Duration::from_millis(100))?;
            }
            let same = match &self.target {
                Target::Shm(b) => self.state.shm_buffer == Some(b.key),
                Target::Gpu { offer, .. } => self.state.dma_buffer == Some(*offer),
            };
            if self.state.failed || !same {
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
            let asked = Instant::now();
            let wl = match &self.target {
                Target::Shm(b) => &b.buffer,
                Target::Gpu { buffer, .. } => buffer,
            };
            let was_full = full;
            if full {
                f.copy(wl);
            } else {
                f.copy_with_damage(wl);
            }
            full = false;
            while !self.state.ready && !self.state.failed && !stop.load(Ordering::Relaxed) {
                let wait = if was_full {
                    Duration::from_millis(100)
                } else {
                    keepalive.saturating_sub(last_sent.elapsed()).min(Duration::from_millis(100))
                };
                dispatch_for(&mut self.queue, &mut self.state, wait)?;
                // Nothing changed for `keepalive`: give up waiting for damage
                // and take a full copy instead (which is waited out).
                if !was_full && !self.state.ready && last_sent.elapsed() >= keepalive {
                    full = true;
                    break;
                }
            }
            if self.state.ready {
                let packing = Instant::now();
                copy_t += packing - asked;
                match &mut self.target {
                    Target::Shm(b) => on_frame(Frame::Packed(b.packed(self.state.y_invert))),
                    Target::Gpu { conv, .. } => match conv.convert(self.state.y_invert) {
                        Some(d) => on_frame(Frame::Dma(d)),
                        None => tracing::debug!("frame skipped: every GPU buffer is with the encoder"),
                    },
                }
                pack_t += packing.elapsed();
                n += 1;
                last_sent = Instant::now();
            }
            f.destroy();
            if since.elapsed() >= Duration::from_secs(1) && n > 0 {
                tracing::debug!(
                    frames = n,
                    copy_ms = copy_t.as_secs_f64() * 1000.0 / n as f64,
                    pack_ms = pack_t.as_secs_f64() * 1000.0 / n as f64,
                    "capture"
                );
                (n, copy_t, pack_t, since) = (0, Duration::ZERO, Duration::ZERO, Instant::now());
            }
        }
        Ok(())
    }
}

/// A compositor's outputs as monitors (names from wl_output, layout from
/// xdg-output), in the compositor's order.
pub fn monitors(socket: &Path, fps: u32) -> Result<Vec<qr_proto::Monitor>> {
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self as xdg_output, ZxdgOutputV1};

    #[derive(Default)]
    struct Out {
        name: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }
    #[derive(Default)]
    struct St {
        outputs: Vec<(wl_output::WlOutput, Out)>,
        mgr: Option<ZxdgOutputManagerV1>,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for St {
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
                    "wl_output" if version >= 4 => {
                        let i = st.outputs.len();
                        st.outputs.push((reg.bind(name, 4, qh, i), Out::default()));
                    }
                    "zxdg_output_manager_v1" => st.mgr = Some(reg.bind(name, version.min(3), qh, ())),
                    _ => {}
                }
            }
        }
    }
    impl Dispatch<wl_output::WlOutput, usize> for St {
        fn event(
            st: &mut Self,
            _: &wl_output::WlOutput,
            ev: wl_output::Event,
            i: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_output::Event::Name { name } = ev {
                st.outputs[*i].1.name = name;
            }
        }
    }
    impl Dispatch<ZxdgOutputV1, usize> for St {
        fn event(
            st: &mut Self,
            _: &ZxdgOutputV1,
            ev: xdg_output::Event,
            i: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let o = &mut st.outputs[*i].1;
            match ev {
                xdg_output::Event::LogicalPosition { x, y } => (o.x, o.y) = (x, y),
                xdg_output::Event::LogicalSize { width, height } => (o.w, o.h) = (width, height),
                _ => {}
            }
        }
    }
    delegate_noop!(St: ZxdgOutputManagerV1);

    let conn = Connection::from_socket(
        UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?,
    )?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = St::default();
    queue.roundtrip(&mut st)?;
    let mgr = st.mgr.clone().context("the compositor has no xdg-output")?;
    let xdg: Vec<ZxdgOutputV1> =
        st.outputs.iter().enumerate().map(|(i, (o, _))| mgr.get_xdg_output(o, &qh, i)).collect();
    queue.roundtrip(&mut st)?;
    drop(xdg);
    Ok(st
        .outputs
        .iter()
        .enumerate()
        .map(|(i, (_, o))| qr_proto::Monitor {
            id: i as u8,
            name: o.name.clone(),
            width: o.w as u16,
            height: o.h as u16,
            refresh_mhz: fps * 1000,
            x: o.x,
            y: o.y,
            primary: i == 0,
        })
        .collect())
}

/// Whether the compositor at `socket` offers wlr-screencopy.
pub fn supported(socket: &Path) -> bool {
    let Ok(stream) = UnixStream::connect(socket) else { return false };
    let Ok(conn) = Connection::from_socket(stream) else { return false };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state).is_ok() && state.manager.is_some()
}
