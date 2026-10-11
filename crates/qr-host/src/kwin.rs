//! A KDE Plasma (KWin) Wayland desktop through KWin's own protocols, as
//! krfb uses them: `zkde_screencast_unstable_v1` streams each output to
//! PipeWire, and `org_kde_kwin_fake_input` takes input. No permission
//! dialog, no uinput. The clipboard goes through wlr-data-control
//! (`wlclip`), which KWin offers too.
//!
//! KWin only shows these protocols to programs whose desktop file lists
//! them (`X-KDE-Wayland-Interfaces`, matched by the executable's path; see
//! `deploy/kde`). Without one, the host writes its own into the user's
//! applications and KWin picks it up within a second, no restart. (The
//! portal would ask the user to pick a screen, which a host started
//! remotely can't answer.)

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qr_proto::Monitor;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::{
    self as stream, ZkdeScreencastStreamUnstableV1,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

use crate::input::Injector;

/// The cursor drawn into the picture.
const POINTER_EMBEDDED: u32 = 2;

#[derive(Default)]
struct State {
    /// A KDE-only global: this is KWin.
    kde: bool,
    outputs: Vec<(wl_output::WlOutput, String)>,
    screencast: Option<ZkdeScreencastUnstableV1>,
    fake: Option<OrgKdeKwinFakeInput>,
    /// Per requested stream: its node, or why it failed (shared with the
    /// threads asking for streams).
    nodes: Nodes,
}

type Nodes = Arc<Mutex<HashMap<usize, Option<Result<u32, String>>>>>;

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
                "wl_output" if version >= 4 => {
                    let i = st.outputs.len();
                    st.outputs.push((reg.bind(name, 4, qh, i), String::new()));
                }
                "zkde_screencast_unstable_v1" => st.screencast = Some(reg.bind(name, version.min(4), qh, ())),
                "org_kde_kwin_fake_input" => st.fake = Some(reg.bind(name, version.min(5), qh, ())),
                "kde_output_device_v2" | "org_kde_plasma_shell" => st.kde = true,
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

impl Dispatch<ZkdeScreencastStreamUnstableV1, usize> for State {
    fn event(
        st: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        ev: stream::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut nodes = st.nodes.lock().unwrap();
        match ev {
            stream::Event::Created { node } => {
                nodes.insert(*i, Some(Ok(node)));
            }
            stream::Event::Failed { error } => {
                nodes.insert(*i, Some(Err(error)));
            }
            stream::Event::Closed => {
                nodes.remove(i);
                tracing::debug!(stream = i, "KWin closed a screencast stream");
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ZkdeScreencastUnstableV1);
delegate_noop!(State: OrgKdeKwinFakeInput);

/// (KWin, offering its screencast to us) for the desktop at `socket`.
fn probe(socket: &Path) -> (bool, bool) {
    let Ok(s) = UnixStream::connect(socket) else { return (false, false) };
    let Ok(conn) = Connection::from_socket(s) else { return (false, false) };
    let mut q = conn.new_event_queue();
    let _registry = conn.display().get_registry(&q.handle(), ());
    let mut st = State::default();
    if q.roundtrip(&mut st).is_err() {
        return (false, false);
    }
    (st.kde || st.screencast.is_some(), st.screencast.is_some())
}

/// Whether the desktop at `socket` is KWin, which then offers its
/// screencast to us: after writing our desktop file if it didn't yet.
pub fn supported(socket: &Path) -> bool {
    match probe(socket) {
        (_, true) => true,
        (false, _) => false,
        (true, false) => {
            match authorize() {
                Ok(p) => tracing::info!(file = %p.display(), "KWin: listed our protocols in a desktop file"),
                Err(e) => {
                    tracing::warn!("KWin: writing our desktop file: {e:#}");
                    return false;
                }
            }
            // KWin re-reads the applications within a second or so.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(250));
                if probe(socket).1 {
                    return true;
                }
            }
            tracing::warn!("KWin still hides its screencast from this program");
            false
        }
    }
}

/// Writes `quickremote-host.desktop` into the user's applications with
/// this executable's path and the protocols KWin restricts.
fn authorize() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/share"),
    };
    let dir = data.join("applications");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("quickremote-host.desktop");
    let exec = exe.to_str().context("the executable's path isn't UTF-8")?;
    // Desktop-entry quoting: spaces need double quotes; quotes,
    // backslashes, `$` and backquotes would need two levels of escaping.
    if exec.contains(['"', '\\', '$', '`']) {
        bail!("the executable's path has characters a desktop file can't take as is: {exec}");
    }
    let exec = if exec.contains(' ') { format!("\"{exec}\"") } else { exec.to_string() };
    // Written beside and renamed into place: KDE notices changes by the
    // directory's timestamp, which rewriting an existing file (a host that
    // moved) leaves alone.
    let tmp = dir.join(".quickremote-host.desktop.tmp");
    std::fs::write(
        &tmp,
        format!(
            "[Desktop Entry]\nType=Application\nName=QuickRemote host\nExec={exec}\nNoDisplay=true\n\
             X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input\n"
        ),
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// A KWin session: screencast streams on demand, fake input.
pub struct KwinSession {
    pub socket: PathBuf,
    fps: u32,
    conn: Connection,
    qh: QueueHandle<State>,
    screencast: ZkdeScreencastUnstableV1,
    outputs: Vec<(wl_output::WlOutput, String)>,
    nodes: Nodes,
    next: AtomicUsize,
    /// Each monitor's current stream.
    open: Mutex<HashMap<u8, ZkdeScreencastStreamUnstableV1>>,
    /// Virtual outputs we created, by name: they exist while their stream
    /// does.
    virtuals: Mutex<HashMap<String, Virtual>>,
    fake: Option<OrgKdeKwinFakeInput>,
}

/// A virtual output of ours: its stream (closing it removes the output),
/// the key of its node, and what it was created with.
struct Virtual {
    stream: ZkdeScreencastStreamUnstableV1,
    key: usize,
    size: (u32, u32),
    scale_pct: u16,
}

impl KwinSession {
    /// Connects to the desktop at `socket` (streams start per monitor with
    /// [`KwinSession::stream`]).
    pub fn start(socket: &Path, fps: u32) -> Result<Self> {
        let conn = Connection::from_socket(
            UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?,
        )?;
        let mut q: EventQueue<State> = conn.new_event_queue();
        let qh = q.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut st = State::default();
        q.roundtrip(&mut st)?;
        q.roundtrip(&mut st)?; // the outputs' names
        let screencast = st.screencast.clone().context(
            "KWin doesn't offer its screencast to this program (install deploy/kde/quickremote-host.desktop)",
        )?;
        let fake = st.fake.clone();
        match &fake {
            Some(f) => f.authenticate("QuickRemote".into(), "Remote control from a QuickRemote client".into()),
            None => tracing::warn!("KWin doesn't offer fake input to this program: no remote input"),
        }
        conn.flush()?;
        let (outputs, nodes) = (st.outputs.clone(), st.nodes.clone());
        // Keep reading events (KWin drops clients that stop).
        std::thread::Builder::new().name("qr-kwin".into()).spawn(move || {
            while q.blocking_dispatch(&mut st).is_ok() {}
            tracing::warn!("the KWin connection closed");
        })?;
        let me = Self {
            socket: socket.to_path_buf(),
            fps,
            conn,
            qh,
            screencast,
            outputs,
            nodes,
            next: AtomicUsize::new(0),
            open: Mutex::default(),
            virtuals: Mutex::default(),
            fake,
        };
        tracing::info!(monitors = ?me.monitors().iter().map(|m| &m.name).collect::<Vec<_>>(), "KWin session");
        Ok(me)
    }

    /// The outputs as they are now.
    pub fn monitors(&self) -> Vec<Monitor> {
        crate::screencopy::monitors(&self.socket, self.fps).unwrap_or_else(|e| {
            tracing::warn!("listing KWin's outputs: {e:#}");
            Vec::new()
        })
    }

    /// A new PipeWire stream of `monitor` at its current size (replacing
    /// its previous one): its node. Blocks until KWin answers.
    pub fn stream(&self, monitor: &Monitor) -> Result<u32> {
        let (out, _) = self
            .outputs
            .iter()
            .find(|(_, n)| *n == monitor.name)
            .with_context(|| format!("KWin has no output {}", monitor.name))?;
        if let Some(old) = self.open.lock().unwrap().remove(&monitor.id) {
            old.close();
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        self.nodes.lock().unwrap().insert(i, None);
        let s = self.screencast.stream_output(out, POINTER_EMBEDDED, &self.qh, i);
        self.conn.flush()?;
        let node = self.wait_node(i, &monitor.name)?;
        self.open.lock().unwrap().insert(monitor.id, s);
        tracing::debug!(output = monitor.name, node, "KWin screencast");
        Ok(node)
    }

    /// The node of stream `i` once KWin has created it.
    fn wait_node(&self, i: usize, what: &str) -> Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.nodes.lock().unwrap().get(&i) {
                Some(Some(Ok(node))) => return Ok(*node),
                Some(Some(Err(e))) => bail!("KWin's screencast of {what} failed: {e}"),
                None => bail!("KWin closed the screencast of {what}"),
                Some(None) => {}
            }
            if Instant::now() > deadline {
                bail!("KWin started no PipeWire stream for {what}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Creates (or recreates) the virtual output `name`, `size` in pixels at
    /// `scale_pct`, streamed to PipeWire: its node. Blocks until KWin
    /// answers.
    pub fn create_virtual(&self, name: &str, size: (u32, u32), scale_pct: u16) -> Result<u32> {
        self.remove_virtual(name);
        let s = scale_pct.clamp(50, 400) as f64 / 100.0;
        // KWin takes the logical size; the output has size × scale pixels.
        let (lw, lh) = ((size.0 as f64 / s).round() as i32, (size.1 as f64 / s).round() as i32);
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        self.nodes.lock().unwrap().insert(i, None);
        let stream = self.screencast.stream_virtual_output(name.into(), lw, lh, s, POINTER_EMBEDDED, &self.qh, i);
        self.conn.flush()?;
        let node = self.wait_node(i, name)?;
        self.virtuals.lock().unwrap().insert(name.into(), Virtual { stream, key: i, size, scale_pct });
        tracing::info!(name, width = size.0, height = size.1, scale_pct, node, "KWin virtual output");
        Ok(node)
    }

    /// The node streaming virtual output `name`, recreating the output with
    /// its last size if KWin closed the stream (e.g. after a capture error).
    pub fn virtual_node(&self, name: &str) -> Option<Result<u32>> {
        let (key, size, scale_pct) = {
            let v = self.virtuals.lock().unwrap();
            let v = v.get(name)?;
            (v.key, v.size, v.scale_pct)
        };
        if let Some(Some(Ok(node))) = self.nodes.lock().unwrap().get(&key) {
            return Some(Ok(*node));
        }
        Some(self.create_virtual(name, size, scale_pct))
    }

    /// Removes virtual output `name` (if it exists).
    pub fn remove_virtual(&self, name: &str) {
        if let Some(v) = self.virtuals.lock().unwrap().remove(name) {
            v.stream.close();
            let _ = self.conn.flush();
            tracing::info!(name, "KWin virtual output removed");
        }
    }

    /// The virtual outputs' names.
    pub fn virtual_names(&self) -> Vec<String> {
        self.virtuals.lock().unwrap().keys().cloned().collect()
    }

    /// Whether KWin can add virtual outputs here. Its headless backend
    /// (`--virtual`, outputs named `Virtual-N`) can't: it answers a
    /// virtual-output stream with "Could not find output", and may crash
    /// when that failed stream is closed later, so it isn't asked at all.
    pub fn can_create_virtual(&self) -> bool {
        let mons = self.monitors();
        !mons.is_empty() && !mons.iter().all(|m| m.name.starts_with("Virtual-") || self.is_ours(&m.name))
    }

    /// Whether `name` is a virtual output of ours.
    pub fn is_ours(&self, name: &str) -> bool {
        self.virtuals.lock().unwrap().contains_key(name)
    }

    pub fn injector(&self) -> Result<KwinInput> {
        let fake = self.fake.clone().context("KWin offers no fake input to this program")?;
        let keymap = crate::wlkeymap::watch(&self.socket)
            .inspect_err(|e| tracing::warn!("the desktop keymap is unknown ({e:#}); typing ASCII as on a US layout"))
            .ok();
        Ok(KwinInput { conn: self.conn.clone(), fake, keymap, warned: false })
    }
}

/// Input through KWin's fake input (requests go straight out; the session
/// thread reads the events).
pub struct KwinInput {
    conn: Connection,
    fake: OrgKdeKwinFakeInput,
    keymap: Option<crate::wlkeymap::Keysyms>,
    warned: bool,
}

const SHIFT: u32 = 42;
const ALTGR: u32 = 100;

impl KwinInput {
    fn flush(&self) -> Result<()> {
        self.conn.flush().context("the KWin connection is gone")
    }

    fn tap(&self, code: u32, level: u32) {
        let mods: &[u32] = match level {
            1 => &[SHIFT],
            2 => &[ALTGR],
            3 => &[SHIFT, ALTGR],
            _ => &[],
        };
        mods.iter().for_each(|&m| self.fake.keyboard_key(m, 1));
        self.fake.keyboard_key(code, 1);
        self.fake.keyboard_key(code, 0);
        mods.iter().rev().for_each(|&m| self.fake.keyboard_key(m, 0));
    }
}

/// The evdev code and level typing `c` on a US layout (no keymap known).
fn us_key(c: char) -> Option<(u32, u32)> {
    const ROW: [(&str, &str, u32); 4] = [
        ("1234567890-=", "!@#$%^&*()_+", 2),
        ("qwertyuiop[]", "QWERTYUIOP{}", 16),
        ("asdfghjkl;'`", "ASDFGHJKL:\"~", 30),
        ("\\zxcvbnm,./", "|ZXCVBNM<>?", 43),
    ];
    match c {
        ' ' => return Some((57, 0)),
        '\n' => return Some((28, 0)),
        '\t' => return Some((15, 0)),
        _ => {}
    }
    for (plain, shifted, first) in ROW {
        if let Some(i) = plain.chars().position(|x| x == c) {
            return Some((first + i as u32, 0));
        }
        if let Some(i) = shifted.chars().position(|x| x == c) {
            return Some((first + i as u32, 1));
        }
    }
    None
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

impl Injector for KwinInput {
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.fake.keyboard_key(code as u32, down as u32);
        self.flush()
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let code = match button {
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            4 => 0x113,
            5 => 0x114,
            _ => return Ok(()),
        };
        self.fake.button(code, down as u32);
        self.flush()
    }

    fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        self.fake.pointer_motion_absolute(x as f64, y as f64);
        self.flush()
    }

    fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.fake.pointer_motion(dx as f64, dy as f64);
        self.flush()
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        // Axis 0 vertical, 1 horizontal; positive is down/right, 15 per
        // notch (libinput's wheel degrees).
        if dy != 0 {
            self.fake.axis(0, -dy as f64 * 15.0 / 120.0);
        }
        if dx != 0 {
            self.fake.axis(1, dx as f64 * 15.0 / 120.0);
        }
        self.flush()
    }

    /// Characters on the keymap are typed with their key (and Shift or
    /// AltGr); fake input has no way to type others.
    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            let at = match &self.keymap {
                Some(m) if !m.read().unwrap().is_empty() => {
                    m.read().unwrap().get(&keysym(c)).map(|&(kc, level)| (kc - 8, level))
                }
                _ => us_key(c),
            };
            match at {
                Some((code, level)) => self.tap(code, level),
                None if !self.warned => {
                    self.warned = true;
                    tracing::warn!(?c, "this character isn't on the desktop's keyboard layout; skipped");
                }
                None => {}
            }
        }
        self.flush()
    }

    fn name(&self) -> &'static str {
        "kwin"
    }
}

/// Runs `kscreen-doctor` (Plasma's display tool) with `args`: its output.
pub fn kscreen(args: &[&str]) -> Result<String> {
    // Its own defaults: with Qt's offscreen platform (or a forced backend)
    // it reads a fake screen or doesn't apply changes.
    let out = std::process::Command::new("kscreen-doctor")
        .args(args)
        .env_remove("QT_QPA_PLATFORM")
        .env_remove("KSCREEN_BACKEND")
        .output()
        .context("running kscreen-doctor (part of Plasma)")?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        bail!("kscreen-doctor {args:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// Output `name` as kscreen-doctor describes it.
fn kscreen_output(name: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(&kscreen(&["-j"])?)?;
    v["outputs"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["name"] == name))
        .cloned()
        .with_context(|| format!("kscreen-doctor doesn't list {name}"))
}

/// Output `name`'s scale, percent.
pub fn kscreen_scale_pct(name: &str) -> Result<u16> {
    Ok((kscreen_output(name)?["scale"].as_f64().context("no scale")? * 100.0).round() as u16)
}

/// The modes output `name` offers, as (width, height, refresh Hz).
pub fn kscreen_modes(name: &str) -> Result<Vec<(u32, u32, u32)>> {
    let out = kscreen_output(name)?;
    Ok(out["modes"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|m| {
                    Some((
                        m["size"]["width"].as_u64()? as u32,
                        m["size"]["height"].as_u64()? as u32,
                        m["refreshRate"].as_f64()?.round() as u32,
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #[test]
    fn us_layout_fallback() {
        assert_eq!(super::us_key('q'), Some((16, 0)));
        assert_eq!(super::us_key('Q'), Some((16, 1)));
        assert_eq!(super::us_key('!'), Some((2, 1)));
        assert_eq!(super::us_key('/'), Some((53, 0)));
        assert_eq!(super::us_key('é'), None);
    }
}
