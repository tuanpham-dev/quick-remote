//! A GNOME (mutter) Wayland desktop through mutter's own remote-desktop
//! D-Bus API, as gnome-remote-desktop uses it: one RemoteDesktop session
//! with a ScreenCast stream per monitor (PipeWire nodes) and input through
//! the session's Notify* calls. No permission dialog, no uinput.
//!
//! (The portal would ask the user to pick a screen, which a host started
//! remotely can't answer.)

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use qr_proto::Monitor;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::input::Injector;

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const SC: &str = "org.gnome.Mutter.ScreenCast";
const DC: &str = "org.gnome.Mutter.DisplayConfig";

/// One monitor's stream.
#[derive(Debug, Clone)]
pub struct Stream {
    pub monitor: Monitor,
    pub node: u32,
    path: OwnedObjectPath,
    /// A capture has read it: the next one records the monitor anew.
    used: bool,
}

/// The monitors' streams, shared with the input (absolute positions go to
/// a stream).
type Streams = std::sync::Arc<std::sync::Mutex<Vec<Stream>>>;

/// A running remote-desktop session (keep it alive while streaming).
///
/// A monitor stream dies when its monitor changes (size, rotation), and
/// mutter starts no stream recorded after the session did: the session is
/// then replaced as a whole, and the input and clipboard follow it
/// (`rd_session`).
pub struct MutterSession {
    streams: Streams,
    /// The current remote-desktop session.
    rd_session: tokio::sync::watch::Sender<OwnedObjectPath>,
    /// One replacement at a time.
    replacing: tokio::sync::Mutex<()>,
    conn: zbus::Connection,
}

type Spec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type PhysMonitor = (Spec, Vec<Mode>, HashMap<String, OwnedValue>);
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, HashMap<String, OwnedValue>);

/// The monitors as laid out: (connector, x, y, w, h, refresh mHz, primary).
async fn layout(conn: &zbus::Connection) -> Result<Vec<(String, i32, i32, u32, u32, u32, bool)>> {
    let p = zbus::Proxy::new(conn, DC, "/org/gnome/Mutter/DisplayConfig", DC).await?;
    let (_serial, monitors, logical, _props): (u32, Vec<PhysMonitor>, Vec<Logical>, HashMap<String, OwnedValue>) =
        p.call("GetCurrentState", &()).await.context("reading mutter's display layout")?;
    let current = |spec: &Spec| -> Option<(i32, i32, f64)> {
        let (_, modes, _) = monitors.iter().find(|(s, _, _)| s == spec)?;
        modes
            .iter()
            .find(|m| m.6.get("is-current").and_then(|v| bool::try_from(v).ok()).unwrap_or(false))
            .map(|m| (m.1, m.2, m.3))
    };
    let mut out = Vec::new();
    for (x, y, scale, transform, primary, specs, _) in &logical {
        let Some(spec) = specs.first() else { continue };
        let Some((w, h, hz)) = current(spec) else { continue };
        // Turned a quarter (90°, 270°, flipped or not): the mode's sides
        // swap on the desktop.
        let (w, h) = if transform % 2 == 1 { (h, w) } else { (w, h) };
        let s = if *scale > 0.0 { *scale } else { 1.0 };
        out.push((
            spec.0.clone(),
            *x,
            *y,
            (w as f64 / s) as u32,
            (h as f64 / s) as u32,
            (hz * 1000.0) as u32,
            *primary,
        ));
    }
    out.sort_by_key(|m| (!m.6, m.1, m.2));
    Ok(out)
}

/// A remote-desktop session with a stream for every monitor, started:
/// its path and the streams.
async fn open(conn: &zbus::Connection) -> Result<(OwnedObjectPath, Vec<Stream>)> {
    let conn = conn.clone();
    let mons = layout(&conn).await?;
    if mons.is_empty() {
        bail!("mutter reports no monitors");
    }
    let rd = zbus::Proxy::new(&conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD).await?;
    let rd_session: OwnedObjectPath =
        rd.call("CreateSession", &()).await.context("mutter refused a remote-desktop session")?;
    let rds = zbus::Proxy::new(&conn, RD, rd_session.clone(), "org.gnome.Mutter.RemoteDesktop.Session").await?;
    let id: String = rds.get_property("SessionId").await?;
    let sc = zbus::Proxy::new(&conn, SC, "/org/gnome/Mutter/ScreenCast", SC).await?;
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("remote-desktop-session-id", Value::from(id.as_str()));
    let sc_session: OwnedObjectPath = sc.call("CreateSession", &(opts,)).await?;
    let scs = zbus::Proxy::new(&conn, SC, sc_session.clone(), "org.gnome.Mutter.ScreenCast.Session").await?;
    let mut pending = Vec::new();
    for (i, (connector, x, y, w, h, mhz, primary)) in mons.iter().enumerate() {
        let mut o: HashMap<&str, Value> = HashMap::new();
        // 1: the cursor drawn into the picture.
        o.insert("cursor-mode", Value::from(1u32));
        let path: OwnedObjectPath = scs.call("RecordMonitor", &(connector.as_str(), o)).await?;
        let sp = zbus::Proxy::new(&conn, SC, path.clone(), "org.gnome.Mutter.ScreenCast.Stream").await?;
        let added = sp.receive_signal("PipeWireStreamAdded").await?;
        let monitor = Monitor {
            id: i as u8,
            name: connector.clone(),
            width: *w as u16,
            height: *h as u16,
            refresh_mhz: *mhz,
            x: *x,
            y: *y,
            primary: *primary,
        };
        pending.push((monitor, path, added));
    }
    let _: () = rds.call("Start", &()).await.context("starting the remote-desktop session")?;
    let mut streams = Vec::new();
    for (monitor, path, mut added) in pending {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), added.next())
            .await
            .context("mutter started no PipeWire stream")?
            .context("the stream signal ended")?;
        let (node,): (u32,) = msg.body().deserialize()?;
        streams.push(Stream { monitor, node, path, used: false });
    }
    Ok((rd_session, streams))
}

impl MutterSession {
    /// Starts a session on the user's session bus with a stream for every
    /// monitor.
    pub async fn start() -> Result<Self> {
        let conn = zbus::Connection::session().await.context("connecting to the session bus")?;
        let (rd_session, streams) = open(&conn).await?;
        tracing::info!(monitors = ?streams.iter().map(|s| (&s.monitor.name, s.node)).collect::<Vec<_>>(), "mutter session");
        Ok(Self {
            streams: std::sync::Arc::new(std::sync::Mutex::new(streams)),
            rd_session: tokio::sync::watch::Sender::new(rd_session),
            replacing: tokio::sync::Mutex::new(()),
            conn,
        })
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        self.streams.lock().unwrap().iter().map(|s| s.monitor.clone()).collect()
    }

    /// The PipeWire node a capture of `monitor` reads, with the monitor as
    /// it is now: the session's stream of it the first time, else (a
    /// capture of it ended, e.g. failed on a size change) from a new
    /// session at the current layout.
    pub async fn stream(&self, monitor: u8) -> Result<(Monitor, u32)> {
        let _one = self.replacing.lock().await;
        let name = {
            let mut streams = self.streams.lock().unwrap();
            let s = streams.iter_mut().find(|s| s.monitor.id == monitor).context("no such monitor")?;
            if !s.used {
                s.used = true;
                return Ok((s.monitor.clone(), s.node));
            }
            s.monitor.name.clone()
        };
        let old = self.rd_session.borrow().clone();
        if let Ok(p) = zbus::Proxy::new(&self.conn, RD, old, "org.gnome.Mutter.RemoteDesktop.Session").await {
            let _: zbus::Result<()> = p.call("Stop", &()).await;
        }
        let (rd_session, fresh) = open(&self.conn).await.context("replacing the remote-desktop session")?;
        tracing::info!(monitors = ?fresh.iter().map(|s| (&s.monitor.name, s.monitor.width, s.monitor.height, s.node)).collect::<Vec<_>>(), "mutter session replaced");
        let mut streams = self.streams.lock().unwrap();
        *streams = fresh;
        self.rd_session.send_replace(rd_session);
        let s = streams
            .iter_mut()
            .find(|s| s.monitor.name == name)
            .with_context(|| format!("monitor {name} is gone"))?;
        s.used = true;
        Ok((s.monitor.clone(), s.node))
    }

    /// The injector for this session. Calls go out in order from one task
    /// (input never waits on the compositor). Needs the tokio runtime.
    pub fn injector(&self) -> Result<MutterInput> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Call>();
        let (conn, mut session) = (self.conn.clone(), self.rd_session.subscribe());
        tokio::spawn(async move {
            let mut p: Option<zbus::Proxy> = None;
            session.mark_changed();
            loop {
                // The session (first, or a replacement), ready before input
                // comes: mutter drops the first key press and the first
                // pointer motion of a session (and input right after them),
                // so spend them on a left Shift tap and a pixel's wiggle.
                if session.has_changed().unwrap_or(false) {
                    let path = session.borrow_and_update().clone();
                    if let Ok(np) =
                        zbus::Proxy::new(&conn, RD, path, "org.gnome.Mutter.RemoteDesktop.Session").await
                    {
                        let _ = np.call::<_, _, ()>("NotifyKeyboardKeycode", &(42u32, true)).await;
                        let _ = np.call::<_, _, ()>("NotifyKeyboardKeycode", &(42u32, false)).await;
                        let _ = np.call::<_, _, ()>("NotifyPointerMotionRelative", &(1.0f64, 0.0f64)).await;
                        let _ = np.call::<_, _, ()>("NotifyPointerMotionRelative", &(-1.0f64, 0.0f64)).await;
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        p = Some(np);
                    }
                }
                let c = tokio::select! {
                    c = rx.recv() => match c { Some(c) => c, None => return },
                    Ok(()) = session.changed() => {
                        session.mark_changed();
                        continue;
                    }
                };
                let Some(p) = &p else { continue };
                let r = match c {
                    Call::Key(code, down) => p.call::<_, _, ()>("NotifyKeyboardKeycode", &(code, down)).await,
                    Call::Keysym(k, down) => p.call::<_, _, ()>("NotifyKeyboardKeysym", &(k, down)).await,
                    Call::Button(code, down) => p.call::<_, _, ()>("NotifyPointerButton", &(code, down)).await,
                    Call::Abs(stream, x, y) => {
                        p.call::<_, _, ()>("NotifyPointerMotionAbsolute", &(stream.as_str(), x, y)).await
                    }
                    Call::Rel(dx, dy) => p.call::<_, _, ()>("NotifyPointerMotionRelative", &(dx, dy)).await,
                    Call::Axis(axis, steps) => p.call::<_, _, ()>("NotifyPointerAxisDiscrete", &(axis, steps)).await,
                };
                if let Err(e) = r {
                    tracing::debug!("mutter input: {e}");
                }
            }
        });
        // Which characters the keymap types (mutter drops other keysyms).
        let keymap = crate::wlkeymap::socket()
            .context("no Wayland socket")
            .and_then(|s| crate::wlkeymap::watch(&s))
            .inspect_err(|e| tracing::warn!("the desktop keymap is unknown ({e:#}); typing non-ASCII by code point"))
            .ok();
        Ok(MutterInput {
            tx,
            streams: self.streams.clone(),
            keymap,
        })
    }

    /// The desktop's clipboard through the session's selection API. Same
    /// interface as `qr_clip::spawn`. Needs the tokio runtime.
    pub async fn clipboard(
        &self,
    ) -> Result<(std::sync::mpsc::Sender<qr_clip::Command>, std::sync::mpsc::Receiver<qr_clip::Content>)> {
        use qr_clip::{Command, Content};
        const TEXT: &str = "text/plain;charset=utf-8";
        const PNG: &str = "image/png";
        // The session's clipboard, again on each replacement session.
        async fn attach(
            conn: &zbus::Connection,
            path: OwnedObjectPath,
        ) -> Result<(zbus::Proxy<'static>, zbus::proxy::SignalStream<'static>, zbus::proxy::SignalStream<'static>)>
        {
            let p = zbus::Proxy::new(conn, RD, path, "org.gnome.Mutter.RemoteDesktop.Session").await?;
            let no_opts: HashMap<&str, Value> = HashMap::new();
            let _: () = p.call("EnableClipboard", &(no_opts,)).await.context("enabling the clipboard")?;
            let owner_changed = p.receive_signal("SelectionOwnerChanged").await?;
            let transfer = p.receive_signal("SelectionTransfer").await?;
            Ok((p, owner_changed, transfer))
        }
        let (conn, mut session) = (self.conn.clone(), self.rd_session.subscribe());
        let path = session.borrow_and_update().clone();
        let (mut p, mut owner_changed, mut transfer) = attach(&conn, path).await?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Content>();
        // The sync command channel, into the async loop.
        let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        std::thread::spawn(move || {
            while let Ok(c) = cmd_rx.recv() {
                if atx.send(c).is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            let mut owned: Option<Content> = None;
            let mut last: Option<([u8; 32], std::time::Instant)> = None;
            loop {
                tokio::select! {
                    Ok(()) = session.changed() => {
                        let path = session.borrow_and_update().clone();
                        match attach(&conn, path).await {
                            Ok(x) => (p, owner_changed, transfer) = x,
                            Err(e) => tracing::warn!("the clipboard of the new session: {e:#}"),
                        }
                        owned = None;
                    }
                    Some(Command::Set(c)) = arx.recv() => {
                        let mime = match &c { Content::Text(_) => TEXT, Content::Png(_) => PNG };
                        let mut o: HashMap<&str, Value> = HashMap::new();
                        o.insert("mime-types", Value::from(vec![mime]));
                        if let Err(e) = p.call::<_, _, ()>("SetSelection", &(o,)).await {
                            tracing::debug!("setting the selection: {e}");
                            continue;
                        }
                        last = Some((c.hash(), std::time::Instant::now()));
                        owned = Some(c);
                    }
                    Some(m) = transfer.next() => {
                        // An app pastes what we own: write it to mutter.
                        let Ok((mime, serial)): zbus::Result<(String, u32)> = m.body().deserialize() else { continue };
                        let data = match (&owned, mime.as_str()) {
                            (Some(Content::Text(t)), m) if m.starts_with("text/") => Some(t.clone().into_bytes()),
                            (Some(Content::Png(d)), PNG) => Some(d.clone()),
                            _ => None,
                        };
                        let fd: zbus::Result<zbus::zvariant::OwnedFd> = p.call("SelectionWrite", &(serial,)).await;
                        let ok = match (fd, data) {
                            (Ok(fd), Some(d)) => tokio::task::spawn_blocking(move || {
                                use std::io::Write;
                                let fd = std::os::fd::OwnedFd::from(fd);
                                blocking(&fd);
                                let mut f = std::fs::File::from(fd);
                                f.write_all(&d).is_ok()
                            })
                            .await
                            .unwrap_or(false),
                            _ => false,
                        };
                        let _: zbus::Result<()> = p.call("SelectionWriteDone", &(serial, ok)).await;
                    }
                    Some(m) = owner_changed.next() => {
                        // Someone else copied: read it.
                        let opts: HashMap<String, OwnedValue> = match m.body().deserialize::<(HashMap<String, OwnedValue>,)>() {
                            Ok((o,)) => o,
                            Err(e) => {
                                tracing::debug!("selection owner change: {e}");
                                continue;
                            }
                        };
                        let ours = opts.get("session-is-owner").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
                        tracing::debug!(ours, ?opts, "selection owner changed");
                        if ours {
                            continue;
                        }
                        owned = None;
                        let mimes = opts.get("mime-types").map(mime_list).unwrap_or_default();
                        let mime = if mimes.iter().any(|m| m == PNG) {
                            PNG
                        } else if mimes.iter().any(|m| m.starts_with("text/plain")) {
                            TEXT
                        } else {
                            continue;
                        };
                        let fd: zbus::zvariant::OwnedFd = match p.call("SelectionRead", &(mime,)).await {
                            Ok(fd) => fd,
                            Err(e) => {
                                tracing::debug!("reading the selection: {e}");
                                continue;
                            }
                        };
                        let out = out_tx.clone();
                        let prev = last;
                        tokio::task::spawn_blocking(move || {
                            use std::io::Read;
                            let mut data = Vec::new();
                            let fd = std::os::fd::OwnedFd::from(fd);
                            blocking(&fd);
                            let _ = std::fs::File::from(fd).take(64 << 20).read_to_end(&mut data);
                            let c = if mime == PNG { Content::Png(data) } else { Content::Text(String::from_utf8_lossy(&data).into_owned()) };
                            if !prev.is_some_and(|(h, t)| h == c.hash() && t.elapsed() < std::time::Duration::from_secs(2)) {
                                let _ = out.send(c);
                            }
                        });
                    }
                    else => return,
                }
            }
        });
        Ok((cmd_tx, out_rx))
    }
}

impl Drop for MutterSession {
    fn drop(&mut self) {
        let (conn, path) = (self.conn.clone(), self.rd_session.borrow().clone());
        tokio::spawn(async move {
            if let Ok(p) = zbus::Proxy::new(&conn, RD, path, "org.gnome.Mutter.RemoteDesktop.Session").await {
                let _: zbus::Result<()> = p.call("Stop", &()).await;
            }
        });
    }
}

enum Call {
    Key(u32, bool),
    Keysym(u32, bool),
    Button(i32, bool),
    Abs(OwnedObjectPath, f64, f64),
    Rel(f64, f64),
    Axis(u32, i32),
}

/// mutter's selection pipes come non-blocking: read and write them in full.
fn blocking(fd: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    // SAFETY: flag changes on our own fd.
    unsafe {
        let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl & !libc::O_NONBLOCK);
        }
    }
}

/// A `mime-types` option: an array of strings, or (as mutter sends it in
/// `SelectionOwnerChanged`) a one-field structure holding one.
fn mime_list(v: &OwnedValue) -> Vec<String> {
    let strings = |v: &Value| -> Option<Vec<String>> { Vec::<String>::try_from(v.try_clone().ok()?).ok() };
    match &**v {
        Value::Structure(s) => s.fields().first().and_then(strings),
        other => strings(other),
    }
    .unwrap_or_default()
}

/// Input through the remote-desktop session.
pub struct MutterInput {
    tx: tokio::sync::mpsc::UnboundedSender<Call>,
    /// Monitors and their stream objects, for absolute positions.
    streams: Streams,
    /// The keysyms the desktop's keymap has, when known.
    keymap: Option<crate::wlkeymap::Keysyms>,
}

impl MutterInput {
    fn send(&self, c: Call) -> Result<()> {
        self.tx.send(c).map_err(|_| anyhow::anyhow!("the mutter session is gone"))
    }
}

/// Keysym of a character (X11 keysym encoding).
fn keysym(c: char) -> u32 {
    match c {
        '\n' => 0xff0d,
        '\t' => 0xff09,
        c if (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

impl Injector for MutterInput {
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.send(Call::Key(code as u32, down))
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let code: i32 = match button {
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            4 => 0x113,
            5 => 0x114,
            _ => return Ok(()),
        };
        self.send(Call::Button(code, down))
    }

    fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        // Desktop coordinates back to the monitor and its stream.
        let (x, y) = (x as i32, y as i32);
        let hit = self.streams.lock().unwrap().iter().find_map(|s| {
            let m = &s.monitor;
            (x >= m.x && y >= m.y && x < m.x + m.width as i32 && y < m.y + m.height as i32)
                .then(|| (s.path.clone(), (x - m.x) as f64, (y - m.y) as f64))
        });
        let Some((path, mx, my)) = hit else { return Ok(()) };
        self.send(Call::Abs(path, mx, my))
    }

    fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.send(Call::Rel(dx as f64, dy as f64))
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        // Axis 0 vertical, 1 horizontal; positive is down/right, in notches.
        if dy / 120 != 0 {
            self.send(Call::Axis(0, -dy / 120))?;
        }
        if dx / 120 != 0 {
            self.send(Call::Axis(1, dx / 120))?;
        }
        Ok(())
    }

    /// Characters the keymap has go as keysyms. mutter drops the others,
    /// so they're entered by code point: Ctrl+Shift+U, hex digits, space
    /// (IBus and GTK's own input method; most GNOME apps).
    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let k = keysym(c);
            let typeable = c.is_ascii_control()
                || match &self.keymap {
                    Some(m) if !m.read().unwrap().is_empty() => m.read().unwrap().contains_key(&k),
                    _ => c.is_ascii(),
                };
            if typeable {
                self.send(Call::Keysym(k, true))?;
                self.send(Call::Keysym(k, false))?;
                continue;
            }
            const CTRL: u32 = 29;
            const SHIFT: u32 = 42;
            self.send(Call::Key(CTRL, true))?;
            self.send(Call::Key(SHIFT, true))?;
            self.send(Call::Keysym('u' as u32, true))?;
            self.send(Call::Keysym('u' as u32, false))?;
            self.send(Call::Key(SHIFT, false))?;
            self.send(Call::Key(CTRL, false))?;
            for d in format!("{:x}", c as u32).chars().chain([' ']) {
                self.send(Call::Keysym(d as u32, true))?;
                self.send(Call::Keysym(d as u32, false))?;
            }
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mutter"
    }
}
