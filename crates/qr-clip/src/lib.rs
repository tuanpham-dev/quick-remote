//! X11 CLIPBOARD sync: watches the selection (XFixes) and reports changes,
//! and takes ownership to serve content that came from the other side.
//!
//! Text travels as `UTF8_STRING`, images as `image/png` bytes unchanged.
//! Large transfers use the INCR protocol in both directions.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

impl Content {
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            Content::Text(t) => {
                h.update(b"t");
                h.update(t.as_bytes());
            }
            Content::Png(p) => {
                h.update(b"p");
                h.update(p);
            }
        }
        h.finalize().into()
    }
}

/// Largest clipboard item accepted either way.
pub const MAX: usize = 16 * 1024 * 1024;
/// Chunk size for INCR transfers we serve.
const INCR_CHUNK: usize = 256 * 1024;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    png: Atom,
    incr: Atom,
    prop: Atom,
}

fn atom(c: &RustConnection, name: &str) -> Result<Atom> {
    Ok(c.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Commands into the clipboard thread.
pub enum Command {
    /// Own the clipboard with this content (it came from the peer).
    Set(Content),
}

/// Runs the clipboard loop on `display` in a thread. Local changes (not
/// ones we set) arrive on the returned receiver; send `Command::Set` to
/// publish remote content.
pub fn spawn(display: &str) -> Result<(mpsc::Sender<Command>, mpsc::Receiver<Content>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (out_tx, out_rx) = mpsc::channel();
    let mut c = Clip::open(display)?;
    std::thread::Builder::new().name("qr-clipboard".into()).spawn(move || {
        if let Err(e) = c.run(cmd_rx, out_tx) {
            tracing::warn!("clipboard stopped: {e:#}");
        }
    })?;
    Ok((cmd_tx, out_rx))
}

pub struct Clip {
    conn: RustConnection,
    win: Window,
    a: Atoms,
    /// What we currently own and serve.
    owned: Option<Content>,
    /// Hash and time of the last content we set from the peer. Our own
    /// ownership changes are filtered by window; this only stops an app
    /// (a clipboard manager) that re-takes the same content right away
    /// from echoing it back.
    last: Option<([u8; 32], Instant)>,
    /// Outgoing INCR transfers: (requestor, property, data, offset).
    incr_out: Vec<(Window, Atom, Vec<u8>, usize)>,
}

impl Clip {
    pub fn open(display: &str) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).with_context(|| format!("opening X display {display}"))?;
        conn.xfixes_query_version(5, 0)?.reply().context("XFIXES is required")?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let a = Atoms {
            clipboard: atom(&conn, "CLIPBOARD")?,
            targets: atom(&conn, "TARGETS")?,
            utf8: atom(&conn, "UTF8_STRING")?,
            png: atom(&conn, "image/png")?,
            incr: atom(&conn, "INCR")?,
            prop: atom(&conn, "QR_CLIP")?,
        };
        conn.xfixes_select_selection_input(win, a.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER)?;
        conn.flush()?;
        Ok(Self { conn, win, a, owned: None, last: None, incr_out: Vec::new() })
    }

    fn run(&mut self, cmds: mpsc::Receiver<Command>, out: mpsc::Sender<Content>) -> Result<()> {
        loop {
            while let Ok(Command::Set(content)) = cmds.try_recv() {
                self.set(content)?;
            }
            match self.conn.poll_for_event()? {
                Some(ev) => self.event(ev, &out)?,
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Takes ownership of CLIPBOARD with `content`.
    pub fn set(&mut self, content: Content) -> Result<()> {
        self.last = Some((content.hash(), Instant::now()));
        self.owned = Some(content);
        self.conn.set_selection_owner(self.win, self.a.clipboard, x11rb::CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }

    fn event(&mut self, ev: Event, out: &mpsc::Sender<Content>) -> Result<()> {
        match ev {
            Event::XfixesSelectionNotify(e) if e.owner != self.win && e.owner != x11rb::NONE => {
                self.owned = None;
                match self.fetch() {
                    Ok(Some(c)) => {
                        let echo =
                            self.last.is_some_and(|(h, t)| h == c.hash() && t.elapsed() < Duration::from_secs(2));
                        if !echo {
                            let _ = out.send(c);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("reading the clipboard failed: {e:#}"),
                }
            }
            Event::SelectionRequest(r) => self.serve(r)?,
            Event::SelectionClear(_) => self.owned = None,
            Event::PropertyNotify(p) if p.state == xproto::Property::DELETE => self.incr_continue(p.window, p.atom)?,
            _ => {}
        }
        Ok(())
    }

    /// Reads the current clipboard: PNG if offered, else UTF-8 text.
    fn fetch(&mut self) -> Result<Option<Content>> {
        let targets = self.convert(self.a.targets)?;
        let atoms: Vec<Atom> =
            targets.as_chunks::<4>().0.iter().map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]])).collect();
        if atoms.contains(&self.a.png) {
            return Ok(Some(Content::Png(self.convert(self.a.png)?)));
        }
        if atoms.contains(&self.a.utf8) {
            let t = self.convert(self.a.utf8)?;
            return Ok(Some(Content::Text(String::from_utf8_lossy(&t).into_owned())));
        }
        Ok(None)
    }

    /// ConvertSelection and read the result, following INCR.
    fn convert(&mut self, target: Atom) -> Result<Vec<u8>> {
        self.conn.delete_property(self.win, self.a.prop)?;
        self.conn.convert_selection(self.win, self.a.clipboard, target, self.a.prop, x11rb::CURRENT_TIME)?;
        self.conn.flush()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let notify: SelectionNotifyEvent = loop {
            match self.conn.poll_for_event()? {
                Some(Event::SelectionNotify(n)) if n.requestor == self.win => break n,
                // Keep serving while we wait (we may own a different item).
                Some(Event::SelectionRequest(r)) => self.serve(r)?,
                Some(_) => {}
                None if Instant::now() > deadline => bail!("the clipboard owner didn't answer"),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };
        if notify.property == x11rb::NONE {
            bail!("the clipboard owner refused the conversion");
        }
        let p = self.conn.get_property(true, self.win, self.a.prop, AtomEnum::ANY, 0, u32::MAX)?.reply()?;
        if p.type_ != self.a.incr {
            return Ok(p.value);
        }
        // INCR: chunks arrive as property updates until an empty one.
        let mut data = Vec::new();
        self.conn.flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.conn.poll_for_event()? {
                Some(Event::PropertyNotify(n))
                    if n.window == self.win && n.atom == self.a.prop && n.state == xproto::Property::NEW_VALUE =>
                {
                    let p = self.conn.get_property(true, self.win, self.a.prop, AtomEnum::ANY, 0, u32::MAX)?.reply()?;
                    if p.value.is_empty() {
                        return Ok(data);
                    }
                    data.extend_from_slice(&p.value);
                    if data.len() > MAX {
                        bail!("clipboard item larger than 16 MiB");
                    }
                }
                Some(Event::SelectionRequest(r)) => self.serve(r)?,
                Some(_) => {}
                None if Instant::now() > deadline => bail!("INCR transfer timed out"),
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// Answers another app's request for what we own.
    fn serve(&mut self, r: SelectionRequestEvent) -> Result<()> {
        let prop = if r.property == x11rb::NONE { r.target } else { r.property };
        let mut ok = true;
        match &self.owned {
            Some(c) if r.target == self.a.targets => {
                let t = match c {
                    Content::Text(_) => self.a.utf8,
                    Content::Png(_) => self.a.png,
                };
                self.conn.change_property32(
                    PropMode::REPLACE,
                    r.requestor,
                    prop,
                    AtomEnum::ATOM,
                    &[self.a.targets, t],
                )?;
            }
            Some(c) => {
                let (ty, data) = match c {
                    Content::Text(t) if r.target == self.a.utf8 || r.target == u32::from(AtomEnum::STRING) => {
                        (self.a.utf8, t.as_bytes().to_vec())
                    }
                    Content::Png(p) if r.target == self.a.png => (self.a.png, p.clone()),
                    _ => (x11rb::NONE, Vec::new()),
                };
                if ty == x11rb::NONE {
                    ok = false;
                } else if data.len() > INCR_CHUNK {
                    // Announce INCR and send chunks as the requestor deletes them.
                    self.conn.change_window_attributes(
                        r.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    )?;
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        r.requestor,
                        prop,
                        self.a.incr,
                        &[data.len() as u32],
                    )?;
                    self.incr_out.push((r.requestor, prop, data, 0));
                } else {
                    self.conn.change_property8(PropMode::REPLACE, r.requestor, prop, ty, &data)?;
                }
            }
            None => ok = false,
        }
        if !ok {
            let name = self.conn.get_atom_name(r.target).ok().and_then(|c| c.reply().ok());
            tracing::debug!(
                target = ?name.map(|n| String::from_utf8_lossy(&n.name).into_owned()),
                owned = self.owned.is_some(),
                "refused a clipboard request"
            );
        }
        let ev = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property: if ok { prop } else { x11rb::NONE },
        };
        self.conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
        self.conn.flush()?;
        Ok(())
    }

    fn incr_continue(&mut self, window: Window, prop: Atom) -> Result<()> {
        let Some(i) = self.incr_out.iter().position(|t| t.0 == window && t.1 == prop) else { return Ok(()) };
        let (w, p, data, off) = &mut self.incr_out[i];
        let end = (*off + INCR_CHUNK).min(data.len());
        let ty = self.owned.as_ref().map_or(self.a.utf8, |c| match c {
            Content::Text(_) => self.a.utf8,
            Content::Png(_) => self.a.png,
        });
        self.conn.change_property8(PropMode::REPLACE, *w, *p, ty, &data[*off..end])?;
        let finished = *off == data.len();
        *off = end;
        if finished {
            self.incr_out.remove(i);
        }
        self.conn.flush()?;
        Ok(())
    }
}
