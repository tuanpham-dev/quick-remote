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
}

/// A running remote-desktop session (keep it alive while streaming).
pub struct MutterSession {
    pub streams: Vec<Stream>,
    rd_session: OwnedObjectPath,
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
    for (x, y, scale, _transform, primary, specs, _) in &logical {
        let Some(spec) = specs.first() else { continue };
        let Some((w, h, hz)) = current(spec) else { continue };
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

impl MutterSession {
    /// Starts a session on the user's session bus with a stream for every
    /// monitor.
    pub async fn start() -> Result<Self> {
        let conn = zbus::Connection::session().await.context("connecting to the session bus")?;
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
            streams.push(Stream { monitor, node, path });
        }
        tracing::info!(monitors = ?streams.iter().map(|s| (&s.monitor.name, s.node)).collect::<Vec<_>>(), "mutter session");
        Ok(Self { streams, rd_session, conn })
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        self.streams.iter().map(|s| s.monitor.clone()).collect()
    }

    pub fn node(&self, monitor: u8) -> Option<u32> {
        self.streams.iter().find(|s| s.monitor.id == monitor).map(|s| s.node)
    }

    /// The injector for this session. Calls go out in order from one task
    /// (input never waits on the compositor). Needs the tokio runtime.
    pub fn injector(&self) -> Result<MutterInput> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Call>();
        let (conn, path) = (self.conn.clone(), self.rd_session.clone());
        tokio::spawn(async move {
            let Ok(p) = zbus::Proxy::new(&conn, RD, path, "org.gnome.Mutter.RemoteDesktop.Session").await else {
                return;
            };
            while let Some(c) = rx.recv().await {
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
        // mutter drops the first key press and the first pointer motion of
        // a session: spend them on a left Shift tap and a pixel's wiggle.
        for c in [Call::Key(42, true), Call::Key(42, false), Call::Rel(1.0, 0.0), Call::Rel(-1.0, 0.0)] {
            let _ = tx.send(c);
        }
        Ok(MutterInput { tx, streams: self.streams.iter().map(|s| (s.monitor.clone(), s.path.clone())).collect() })
    }
}

impl Drop for MutterSession {
    fn drop(&mut self) {
        let (conn, path) = (self.conn.clone(), self.rd_session.clone());
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

/// Input through the remote-desktop session.
pub struct MutterInput {
    tx: tokio::sync::mpsc::UnboundedSender<Call>,
    /// Monitors and their stream objects, for absolute positions.
    streams: Vec<(Monitor, OwnedObjectPath)>,
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
        let Some((m, path)) = self
            .streams
            .iter()
            .find(|(m, _)| x >= m.x && y >= m.y && x < m.x + m.width as i32 && y < m.y + m.height as i32)
        else {
            return Ok(());
        };
        self.send(Call::Abs(path.clone(), (x - m.x) as f64, (y - m.y) as f64))
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

    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let k = keysym(c);
            self.send(Call::Keysym(k, true))?;
            self.send(Call::Keysym(k, false))?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mutter"
    }
}
