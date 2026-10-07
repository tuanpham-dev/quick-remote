//! The clipboard of a wlroots compositor (the virtual display, sway,
//! Hyprland…) through wlr-data-control: no window or focus needed. Same
//! interface as `qr_clip::spawn`: local changes come out of the receiver,
//! `Command::Set` takes the selection with the peer's content.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qr_clip::{Command, Content};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self as device, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self as offer, ZwlrDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self as source, ZwlrDataControlSourceV1,
};

const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];
const PNG: &str = "image/png";
/// Largest clipboard content taken from an app.
const MAX: usize = 64 << 20;

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwlrDataControlManagerV1>,
    /// Offers' MIME types, until their selection arrives.
    mimes: HashMap<ObjectId, Vec<String>>,
    /// A new selection to read (None: cleared).
    selection: Option<Option<ZwlrDataControlOfferV1>>,
    /// What we offer, for requests to our source.
    owned: Option<(ZwlrDataControlSourceV1, Content)>,
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
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                "zwlr_data_control_manager_v1" => st.manager = Some(reg.bind(name, version.min(2), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        ev: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            device::Event::DataOffer { id } => {
                st.mimes.insert(id.id(), Vec::new());
            }
            device::Event::Selection { id } => st.selection = Some(id),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        st: &mut Self,
        o: &ZwlrDataControlOfferV1,
        ev: offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = ev {
            st.mimes.entry(o.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        st: &mut Self,
        s: &ZwlrDataControlSourceV1,
        ev: source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            source::Event::Send { mime_type, fd } => {
                let Some((_, content)) = st.owned.as_ref().filter(|(o, _)| o == s) else { return };
                let data = match (content, mime_type == PNG) {
                    (Content::Png(p), true) => p.clone(),
                    (Content::Text(t), false) => t.clone().into_bytes(),
                    _ => return,
                };
                // Big contents: write without blocking the event loop.
                std::thread::spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(&data);
                });
            }
            source::Event::Cancelled => {
                if st.owned.as_ref().is_some_and(|(o, _)| o == s) {
                    st.owned = None;
                }
                s.destroy();
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwlrDataControlManagerV1);

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a two-int array for pipe2; the fds are owned right away.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe: {}", std::io::Error::last_os_error());
    }
    // SAFETY: fresh fds from pipe2.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn dispatch(q: &mut EventQueue<State>, st: &mut State, timeout: Duration) -> Result<()> {
    q.dispatch_pending(st)?;
    q.flush()?;
    if let Some(guard) = q.prepare_read() {
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) } > 0 {
            guard.read()?;
        }
    }
    q.dispatch_pending(st)?;
    Ok(())
}

/// Starts the clipboard loop on the compositor at `socket`.
pub fn spawn(socket: &Path) -> Result<(mpsc::Sender<Command>, mpsc::Receiver<Content>)> {
    let conn = Connection::from_socket(
        UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?,
    )?;
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State::default();
    q.roundtrip(&mut st)?;
    let manager = st.manager.clone().context("the compositor has no wlr-data-control")?;
    let seat = st.seat.clone().context("the compositor has no seat")?;
    let dev = manager.get_data_device(&seat, &qh, ());
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (out_tx, out_rx) = mpsc::channel::<Content>();
    std::thread::Builder::new().name("qr-wlclip".into()).spawn(move || {
        let _conn = conn;
        // The last content set from the peer: an app taking the same
        // content right back isn't news.
        let mut last: Option<([u8; 32], Instant)> = None;
        loop {
            if let Err(e) = dispatch(&mut q, &mut st, Duration::from_millis(50)) {
                tracing::warn!("clipboard stopped: {e:#}");
                return;
            }
            while let Ok(Command::Set(c)) = cmd_rx.try_recv() {
                let src = manager.create_data_source(&qh, ());
                match &c {
                    Content::Text(_) => TEXT_TYPES.iter().for_each(|m| src.offer(m.to_string())),
                    Content::Png(_) => src.offer(PNG.into()),
                }
                dev.set_selection(Some(&src));
                last = Some((c.hash(), Instant::now()));
                if let Some((old, _)) = st.owned.replace((src, c)) {
                    old.destroy();
                }
            }
            let Some(sel) = st.selection.take() else { continue };
            let Some(o) = sel else { continue };
            let mimes = st.mimes.remove(&o.id()).unwrap_or_default();
            // Our own offer coming back: nothing new.
            if st.owned.is_some() && mimes.iter().any(|m| m == PNG || TEXT_TYPES.contains(&m.as_str())) {
                let ours = last.is_some_and(|(_, t)| t.elapsed() < Duration::from_millis(500));
                if ours {
                    o.destroy();
                    continue;
                }
            }
            let mime = if mimes.iter().any(|m| m == PNG) {
                PNG
            } else if let Some(t) = TEXT_TYPES.iter().find(|t| mimes.iter().any(|m| m == *t)) {
                t
            } else {
                o.destroy();
                continue;
            };
            let Ok((r, w)) = pipe() else { continue };
            o.receive(mime.into(), w.as_fd());
            drop(w);
            let _ = q.flush();
            o.destroy();
            let (out, mime) = (out_tx.clone(), mime.to_string());
            let prev = last;
            // Read in a thread: the source app writes while we keep
            // dispatching (it may even be us).
            std::thread::spawn(move || {
                let mut data = Vec::new();
                let _ = std::fs::File::from(r).take(MAX as u64).read_to_end(&mut data);
                let c = if mime == PNG {
                    Content::Png(data)
                } else {
                    Content::Text(String::from_utf8_lossy(&data).into_owned())
                };
                let echo = prev.is_some_and(|(h, t)| h == c.hash() && t.elapsed() < Duration::from_secs(2));
                if !echo {
                    let _ = out.send(c);
                }
            });
        }
    })?;
    Ok((cmd_tx, out_rx))
}
