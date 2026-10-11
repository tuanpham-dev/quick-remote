//! WebSocket viewers (`--ws-listen`): the session over plain TCP for
//! browsers that cannot reach the host over UDP (the Perch Remote Desktop
//! extension, whose traffic rides Perch's proxy). Loopback only: this
//! transport has no pairing, so whatever fronts it is the guard.
//!
//! Any number of viewers share one session with the host. The hub opens a
//! loopback QUIC session (with the web gateway's trusted identity, as the
//! WebTransport gateway does) when the first viewer says `Hello`, rebuilds
//! the host's FEC shards into whole frames (loopback loses nothing) and
//! sends one message per frame to every viewer. The first viewer is the
//! driver: only its display requests apply. See docs/protocol.md,
//! "WebSocket transport", for the framing and the seat rules.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use qr_proto::datagram::{FrameMeta, fflags, kind};
use qr_proto::msg::{Feedback, HostStats};
use qr_proto::{Msg, Reader, Writer};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::HostCtx;

/// The first byte of every message.
pub mod channel {
    pub const CONTROL: u8 = 0;
    pub const INPUT: u8 = 1;
    pub const VIDEO: u8 = 2;
    pub const DATAGRAM: u8 = 3;
    pub const FILE: u8 = 4;
    pub const SEAT: u8 = 5;
}

/// Channel 5 sub-kinds.
pub mod seat {
    pub const SEAT: u8 = 1;
    pub const CLAIM: u8 = 2;
}

/// Queued video a viewer may have before it is skipped to the next keyframe.
const BACKLOG: Duration = Duration::from_millis(100);
/// Queued frames above which a viewer is skipped whatever their timing.
const BACKLOG_FRAMES: usize = 60;
/// Forced keyframes: at most one per this.
const KEYFRAME_EVERY: Duration = Duration::from_millis(250);
/// A departed driver gets its seat back when it returns within this.
const RECLAIM: Duration = Duration::from_secs(10);
/// A viewer's feedback counts while it is this fresh.
const FEEDBACK_FRESH: Duration = Duration::from_millis(100);
/// A viewer whose frames keep arriving this much later than its best is
/// falling behind (TCP buffers along the way hold the rest): it is sent
/// nothing until the next keyframe, which skips the backlog in one go.
/// One late window is jitter, not lag: the excess has to last
/// `LAG_PERSIST` reports (150 ms), and a catch-up is not repeated within
/// `LAG_COOLDOWN`, so a busy but healthy link keeps its frame rate.
const LAG_SET_US: i64 = 300_000;
const LAG_CLEAR_US: i64 = 100_000;
const LAG_PERSIST: u8 = 3;
const LAG_COOLDOWN: Duration = Duration::from_secs(2);
/// How long the best delay seen stays the reference.
const LAG_WINDOW: Duration = Duration::from_secs(5);

/// A viewer's seat, as told to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seat {
    pub driver: bool,
    pub scale_pct: u16,
    pub driver_ratio_pct: u16,
    pub viewers: u16,
    pub cursor_in_picture: bool,
}

impl Seat {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(10);
        w.u8(channel::SEAT).u8(seat::SEAT).bool(self.driver).u16(self.scale_pct).u16(self.driver_ratio_pct);
        w.u16(self.viewers).bool(self.cursor_in_picture);
        w.buf
    }

    #[cfg(test)]
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        anyhow::ensure!(r.u8()? == channel::SEAT && r.u8()? == seat::SEAT, "not a seat message");
        let s = Self {
            driver: r.bool()?,
            scale_pct: r.u16()?,
            driver_ratio_pct: r.u16()?,
            viewers: r.u16()?,
            cursor_in_picture: r.bool()?,
        };
        r.finish()?;
        Ok(s)
    }
}

#[cfg(test)]
pub fn encode_claim(viewer_id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(channel::SEAT).u8(seat::CLAIM).str(viewer_id);
    w.buf
}

/// The viewer id of a `Claim` (the body after the channel byte).
fn decode_claim(body: &[u8]) -> Result<String> {
    let mut r = Reader::new(body);
    anyhow::ensure!(r.u8()? == seat::CLAIM, "not a claim");
    let id = r.str()?;
    r.finish()?;
    Ok(id)
}

/// One frame for channel 2.
pub fn video_message(stream: u8, meta: &FrameMeta, au: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + qr_proto::datagram::FRAME_META_LEN + au.len());
    w.u8(channel::VIDEO).u8(stream);
    FrameMeta { len: au.len() as u32, ..*meta }.write(&mut w);
    w.buf.extend_from_slice(au);
    w.buf
}

/// A file chunk for channel 4.
fn file_message(id: u32, last: bool, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(6 + data.len());
    w.u8(channel::FILE).u32(id).bool(last);
    w.buf.extend_from_slice(data);
    w.buf
}

fn control_message(m: &Msg) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.push(channel::CONTROL);
    v.extend_from_slice(&m.encode());
    v
}

fn binary(v: Vec<u8>) -> Message {
    Message::Binary(Bytes::from(v))
}

/// Whether a viewer with `queued` frame send times (oldest first, host
/// microseconds) and one more frame at `next_us` has fallen behind.
pub fn backlogged(queued: &VecDeque<u64>, next_us: u64) -> bool {
    match queued.front() {
        None => false,
        Some(&oldest) => queued.len() >= BACKLOG_FRAMES || next_us.saturating_sub(oldest) > BACKLOG.as_micros() as u64,
    }
}

struct Viewer {
    id: u64,
    /// The id the browser keeps across reloads (`Hello.name` as
    /// `perch:<id>`, or `Claim`), for getting the driver's seat back.
    viewer_id: String,
    tx: mpsc::UnboundedSender<Message>,
    /// Send times of the video frames queued to this viewer's socket.
    queue: Arc<Mutex<VecDeque<u64>>>,
    driver: bool,
    paused: bool,
    /// Nothing until the next keyframe (joined, unpaused, or fell behind).
    want_key: bool,
    /// Sent `Hello`: part of the session.
    joined: Option<Instant>,
    /// The scale it asked for last (the driver's ratio, for followers).
    ratio_pct: u16,
    feedback: Option<(Feedback, Instant)>,
    /// The smallest delays reported lately: the reference for lag.
    delay_floor: VecDeque<(Instant, i32)>,
    /// Fell behind: nothing until the next keyframe catches it up.
    lagging: bool,
    /// Consecutive reports over `LAG_SET_US`, and the last catch-up.
    lag_hits: u8,
    lag_at: Option<Instant>,
}

impl Viewer {
    /// The best delay this viewer reported in the last seconds, on its
    /// own clock.
    fn floor(&self) -> Option<i32> {
        self.delay_floor.iter().map(|(_, d)| *d).min()
    }

    /// Judges lag from a feedback report: how far the slowest frame of the
    /// window trails the best delay seen in the last seconds.
    fn note_feedback(&mut self, fb: &Feedback, now: Instant) {
        if fb.frames_rx > 0 {
            self.delay_floor.push_back((now, fb.delay_min_us));
        }
        while self.delay_floor.front().is_some_and(|(t, _)| now - *t > LAG_WINDOW) {
            self.delay_floor.pop_front();
        }
        if let Some(floor) = self.floor()
            && fb.frames_rx > 0
        {
            let excess = fb.delay_max_us as i64 - floor as i64;
            if excess > LAG_SET_US {
                self.lag_hits = self.lag_hits.saturating_add(1);
                let cooled = self.lag_at.is_none_or(|t| now - t >= LAG_COOLDOWN);
                if self.lag_hits >= LAG_PERSIST && !self.lagging && cooled {
                    self.lagging = true;
                    self.want_key = true;
                    self.lag_at = Some(now);
                    self.lag_hits = 0;
                }
            } else if excess < LAG_CLEAR_US {
                self.lag_hits = 0;
            }
        }
        self.feedback = Some((fb.clone(), now));
    }
}

/// What a late viewer needs to catch up, from the host's own messages.
#[derive(Default)]
struct Cache {
    hello_ack: Option<Msg>,
    session_start: Option<Msg>,
    monitor_list: Option<Msg>,
    stream_started: Option<Msg>,
    display_info: Option<Msg>,
    cursor_info: Option<Msg>,
    cursor: Option<Msg>,
    error: Option<Msg>,
    stats: Option<HostStats>,
}

impl Cache {
    fn handshake(&self) -> Vec<&Msg> {
        [
            &self.hello_ack,
            &self.session_start,
            &self.monitor_list,
            &self.stream_started,
            &self.display_info,
            &self.cursor_info,
            &self.cursor,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// The session with the host.
struct Link {
    conn: quinn::Connection,
    control: mpsc::UnboundedSender<Vec<u8>>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    /// Viewer → host file streams by id.
    files: HashMap<u32, mpsc::UnboundedSender<(Vec<u8>, bool)>>,
    generation: u64,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// What the host was last told about pausing.
    host_paused: bool,
}

impl Drop for Link {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.conn.close(0u32.into(), b"bye");
    }
}

#[derive(Default)]
struct Inner {
    viewers: Vec<Viewer>,
    link: Option<Link>,
    /// A link is being opened (the first `Hello` is in flight).
    connecting: bool,
    cache: Cache,
    left_driver: Option<(String, Instant)>,
    next_id: u64,
    generation: u64,
    last_keyreq: Option<Instant>,
}

pub struct Hub {
    ctx: Arc<HostCtx>,
    gw_id: qr_net::Identity,
    host_addr: SocketAddr,
    inner: Mutex<Inner>,
}

impl Hub {
    pub fn new(ctx: Arc<HostCtx>, gw_id: qr_net::Identity, host_addr: SocketAddr) -> Arc<Self> {
        let host_addr = SocketAddr::new(
            if host_addr.ip().is_unspecified() { "127.0.0.1".parse().expect("ip") } else { host_addr.ip() },
            host_addr.port(),
        );
        Arc::new(Self { ctx, gw_id, host_addr, inner: Mutex::new(Inner::default()) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("lock")
    }

    fn add_viewer(&self, tx: mpsc::UnboundedSender<Message>) -> (u64, Arc<Mutex<VecDeque<u64>>>) {
        let mut i = self.lock();
        i.next_id += 1;
        let id = i.next_id;
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        i.viewers.push(Viewer {
            id,
            viewer_id: format!("viewer-{id}"),
            tx,
            queue: queue.clone(),
            driver: false,
            paused: false,
            want_key: true,
            joined: None,
            ratio_pct: 100,
            feedback: None,
            delay_floor: VecDeque::new(),
            lagging: false,
            lag_hits: 0,
            lag_at: None,
        });
        (id, queue)
    }

    fn remove_viewer(&self, id: u64) {
        let mut i = self.lock();
        let Some(pos) = i.viewers.iter().position(|v| v.id == id) else { return };
        let v = i.viewers.remove(pos);
        if v.driver {
            i.left_driver = Some((v.viewer_id.clone(), Instant::now()));
            // The longest-connected viewer takes the wheel.
            if let Some(next) = i.viewers.iter_mut().filter(|v| v.joined.is_some()).min_by_key(|v| v.joined) {
                next.driver = true;
            }
        }
        if i.viewers.iter().all(|v| v.joined.is_none()) {
            i.link = None;
            i.connecting = false;
            i.cache = Cache::default();
            tracing::info!("last viewer left: session closed");
        } else {
            Self::sync_pause(&mut i);
            self.broadcast_seats(&mut i);
        }
    }

    fn seat_for(&self, i: &Inner, v: &Viewer) -> Seat {
        let scale_pct = match &i.cache.display_info {
            Some(Msg::DisplayInfo { scale_pct, .. }) => *scale_pct,
            _ => 100,
        };
        let driver_ratio_pct = i.viewers.iter().find(|v| v.driver).map_or(100, |d| d.ratio_pct);
        Seat {
            driver: v.driver,
            scale_pct,
            driver_ratio_pct,
            viewers: i.viewers.iter().filter(|v| v.joined.is_some()).count() as u16,
            cursor_in_picture: self.ctx.cursor_in_picture,
        }
    }

    fn broadcast_seats(&self, i: &mut Inner) {
        let seats: Vec<(usize, Seat)> = i
            .viewers
            .iter()
            .enumerate()
            .filter(|(_, v)| v.joined.is_some())
            .map(|(n, v)| (n, self.seat_for(i, v)))
            .collect();
        for (n, s) in seats {
            let _ = i.viewers[n].tx.send(binary(s.encode()));
        }
    }

    /// Tells the host to pause once every viewer is paused.
    fn sync_pause(i: &mut Inner) {
        let all = !i.viewers.is_empty() && i.viewers.iter().filter(|v| v.joined.is_some()).all(|v| v.paused);
        if let Some(l) = &mut i.link
            && l.host_paused != all
        {
            l.host_paused = all;
            let _ = l.control.send(Msg::Pause { paused: all }.encode());
        }
    }

    fn request_keyframe(i: &mut Inner) {
        let now = Instant::now();
        if i.last_keyreq.is_some_and(|t| now - t < KEYFRAME_EVERY) {
            return;
        }
        if let Some(l) = &i.link {
            i.last_keyreq = Some(now);
            let _ = l.control.send(Msg::KeyframeReq { stream: 0 }.encode());
        }
    }

    fn make_driver(i: &mut Inner, id: u64) {
        for v in &mut i.viewers {
            v.driver = v.id == id;
        }
        i.left_driver = None;
    }

    /// A message from a viewer.
    async fn on_viewer_msg(self: &Arc<Self>, id: u64, data: &[u8]) -> Result<()> {
        let (Some(&ch), body) = (data.first(), &data[1.min(data.len())..]) else { return Ok(()) };
        match ch {
            channel::CONTROL => {
                let m = Msg::decode(body).context("control message")?;
                self.on_control(id, m, body).await
            }
            channel::INPUT => {
                let i = self.lock();
                if let Some(l) = &i.link {
                    let _ = l.input.send(body.to_vec());
                }
                Ok(())
            }
            channel::DATAGRAM => {
                let conn = self.lock().link.as_ref().map(|l| l.conn.clone());
                if let Some(c) = conn {
                    let _ = c.send_datagram(Bytes::copy_from_slice(body));
                }
                Ok(())
            }
            channel::FILE => {
                let mut r = Reader::new(body);
                let (fid, last) = (r.u32()?, r.bool()?);
                let chunk = r.rest().to_vec();
                let mut i = self.lock();
                let Some(l) = &mut i.link else { return Ok(()) };
                let tx = l.files.entry(fid).or_insert_with(|| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let conn = l.conn.clone();
                    tokio::spawn(upload(conn, fid, rx));
                    tx
                });
                let _ = tx.send((chunk, last));
                if last {
                    l.files.remove(&fid);
                }
                Ok(())
            }
            channel::SEAT => {
                let viewer_id = decode_claim(body)?;
                let mut i = self.lock();
                if let Some(v) = i.viewers.iter_mut().find(|v| v.id == id) {
                    if !viewer_id.is_empty() {
                        v.viewer_id = viewer_id;
                    }
                    tracing::info!(viewer = %v.viewer_id, "takes the driver's seat");
                    Self::make_driver(&mut i, id);
                    self.broadcast_seats(&mut i);
                }
                Ok(())
            }
            other => {
                tracing::debug!(channel = other, "unknown channel from a viewer");
                Ok(())
            }
        }
    }

    async fn on_control(self: &Arc<Self>, id: u64, m: Msg, raw: &[u8]) -> Result<()> {
        // Whether this viewer should open the session, decided under the lock.
        let mut open_with: Option<Vec<u8>> = None;
        {
            let mut i = self.lock();
            let Some(n) = i.viewers.iter().position(|v| v.id == id) else { return Ok(()) };
            match m {
                Msg::Hello { ref name, .. } => {
                    let now = Instant::now();
                    let vid = name.strip_prefix("perch:").filter(|s| !s.is_empty()).map(str::to_string);
                    let reclaim = matches!((&vid, &i.left_driver), (Some(v), Some((left, at))) if v == left && now - *at < RECLAIM);
                    let nobody_drives = !i.viewers.iter().any(|v| v.driver);
                    {
                        let v = &mut i.viewers[n];
                        v.joined = Some(now);
                        v.want_key = true;
                        if let Some(vid) = vid {
                            v.viewer_id = vid;
                        }
                    }
                    if reclaim || nobody_drives {
                        Self::make_driver(&mut i, id);
                    }
                    if i.link.is_none() && !i.connecting {
                        i.connecting = true;
                        i.generation += 1;
                        // A shared session streams H.264: the one codec every
                        // browser decodes, so a viewer joining later (a laptop
                        // after a phone with hardware HEVC) gets a picture.
                        open_with = Some(match &m {
                            Msg::Hello { major, minor, name, os, want_width, want_height, want_refresh, decoders } => {
                                let h264: Vec<_> =
                                    decoders.iter().filter(|c| c.codec == qr_proto::Codec::H264).cloned().collect();
                                Msg::Hello {
                                    major: *major,
                                    minor: *minor,
                                    name: name.clone(),
                                    os: os.clone(),
                                    want_width: *want_width,
                                    want_height: *want_height,
                                    want_refresh: *want_refresh,
                                    decoders: if h264.is_empty() { decoders.clone() } else { h264 },
                                }
                                .encode()
                            }
                            _ => raw.to_vec(),
                        });
                    } else if i.link.is_some() {
                        let catch_up: Vec<Message> =
                            i.cache.handshake().into_iter().map(|m| binary(control_message(m))).collect();
                        for m in catch_up {
                            let _ = i.viewers[n].tx.send(m);
                        }
                        Self::request_keyframe(&mut i);
                    }
                    self.broadcast_seats(&mut i);
                }
                Msg::Ping { seq, t_client } => {
                    let pong = Msg::Pong { seq, t_client, t_host: qr_net::clock::now_us() };
                    let _ = i.viewers[n].tx.send(binary(control_message(&pong)));
                }
                Msg::Feedback(fb) => {
                    let now = Instant::now();
                    let v = &mut i.viewers[n];
                    let was = v.lagging;
                    v.note_feedback(&fb, now);
                    if v.lagging != was {
                        tracing::info!(viewer = %v.viewer_id, lagging = v.lagging, "viewer link");
                    }
                    if v.lagging && !was {
                        Self::request_keyframe(&mut i);
                    }
                }
                Msg::Pause { paused } => {
                    let v = &mut i.viewers[n];
                    v.paused = paused;
                    if !paused {
                        v.want_key = true;
                        Self::request_keyframe(&mut i);
                    }
                    Self::sync_pause(&mut i);
                }
                Msg::KeyframeReq { .. } | Msg::FrameLost { .. } => {
                    i.viewers[n].want_key = true;
                    Self::request_keyframe(&mut i);
                }
                Msg::DisplayRequest { scale_pct, .. } => {
                    i.viewers[n].ratio_pct = scale_pct;
                    if i.viewers[n].driver {
                        if let Some(l) = &i.link {
                            let _ = l.control.send(raw.to_vec());
                        }
                        self.broadcast_seats(&mut i);
                    }
                }
                Msg::Bye => bail!("the viewer said bye"),
                Msg::Clipboard { .. }
                | Msg::FileOffer { .. }
                | Msg::FileAccept { .. }
                | Msg::FileCancel { .. }
                | Msg::FileDone { .. }
                | Msg::PadAttach { .. }
                | Msg::PadDetach { .. } => {
                    if let Some(l) = &i.link {
                        let _ = l.control.send(raw.to_vec());
                    }
                }
                // Everything that shapes the stream: the driver's call.
                _ => {
                    if i.viewers[n].driver
                        && let Some(l) = &i.link
                    {
                        let _ = l.control.send(raw.to_vec());
                    }
                }
            }
        }
        if let Some(hello) = open_with {
            let generation = self.lock().generation;
            match self.open_link(hello, generation).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!("opening the session for WebSocket viewers: {e:#}");
                    let mut i = self.lock();
                    i.connecting = false;
                    let err = Msg::Error { code: qr_proto::msg::ErrorCode::Internal as u16, text: format!("{e:#}") };
                    for v in i.viewers.iter().filter(|v| v.joined.is_some()) {
                        let _ = v.tx.send(binary(control_message(&err)));
                        let _ = v.tx.send(Message::Close(None));
                    }
                }
            }
        }
        Ok(())
    }

    /// Opens the loopback session and starts its tasks.
    async fn open_link(self: &Arc<Self>, hello: Vec<u8>, generation: u64) -> Result<()> {
        let bind: SocketAddr = "127.0.0.1:0".parse()?;
        let socket = qr_net::endpoint::bind_socket(bind)?;
        let mut ep =
            quinn::Endpoint::new(quinn::EndpointConfig::default(), None, socket, Arc::new(quinn::TokioRuntime))?;
        ep.set_default_client_config(qr_net::endpoint::client_config(Some(&self.gw_id), &[qr_proto::ALPN])?);
        let local_port = ep.local_addr()?.port();
        // The host sizes datagrams and judges the path by this entry.
        self.ctx.web_peers.0.lock().expect("lock").insert(
            local_port,
            crate::web::WebPeer { addr: "127.0.0.1:0".parse()?, rtt: Duration::from_millis(1), max_datagram: 1200 },
        );
        let conn = match ep.connect(self.host_addr, "quickremote")?.await {
            Ok(c) => c,
            Err(e) => {
                self.ctx.web_peers.0.lock().expect("lock").remove(&local_port);
                return Err(e).context("connecting to the host");
            }
        };
        let (ctl_tx, ctl_rx) = conn.open_bi().await.context("control stream")?;
        let mut ctl_tx = qr_net::session::FramedSend(ctl_tx);
        ctl_tx.send_raw(&hello).await?;
        let mut input = conn.open_uni().await.context("input stream")?;
        input.write_all(&[qr_proto::uni::INPUT]).await?;

        let (control_s, mut control_r) = mpsc::unbounded_channel::<Vec<u8>>();
        let (input_s, mut input_r) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(async move {
            while let Some(m) = control_r.recv().await {
                if ctl_tx.send_raw(&m).await.is_err() {
                    return;
                }
            }
        }));
        tasks.push(tokio::spawn(async move {
            let mut input = qr_net::session::FramedSend(input);
            while let Some(m) = input_r.recv().await {
                if input.send_raw(&m).await.is_err() {
                    return;
                }
            }
        }));
        // Host control messages: cached and fanned out; the link ends with
        // the stream.
        {
            let (hub, mut rx) = (self.clone(), qr_net::session::FramedRecv(ctl_rx));
            tasks.push(tokio::spawn(async move {
                while let Ok(Some(body)) = rx.recv_raw().await {
                    hub.on_host_msg(&body);
                }
                hub.link_ended(generation);
            }));
        }
        // Datagrams: video rebuilt into frames, the rest passed on.
        {
            let (hub, conn) = (self.clone(), conn.clone());
            tasks.push(tokio::spawn(async move {
                let mut reasm = qr_fec::Reassembler::new(0, Duration::from_millis(34));
                let now = || qr_net::clock::base().elapsed();
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                loop {
                    let events = tokio::select! {
                        d = conn.read_datagram() => {
                            let Ok(d) = d else { break };
                            match d.first().copied() {
                                Some(kind::VIDEO) => match reasm.push(&d, now()) {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        tracing::debug!("bad video datagram: {e}");
                                        continue;
                                    }
                                },
                                Some(_) => {
                                    hub.broadcast_datagram(&d);
                                    continue;
                                }
                                None => continue,
                            }
                        }
                        _ = tick.tick() => reasm.poll(now()),
                    };
                    for ev in events {
                        match ev {
                            qr_fec::Event::Frame(f) => hub.on_frame(&f),
                            qr_fec::Event::Lost { .. } => {
                                let mut i = hub.lock();
                                for v in &mut i.viewers {
                                    v.want_key = true;
                                }
                                Self::request_keyframe(&mut i);
                            }
                        }
                    }
                }
            }));
        }
        // Files from the host go to the driver.
        {
            let (hub, conn) = (self.clone(), conn.clone());
            tasks.push(tokio::spawn(async move {
                while let Ok(mut st) = conn.accept_uni().await {
                    let hub = hub.clone();
                    tokio::spawn(async move {
                        let mut head = [0u8; 5];
                        if st.read_exact(&mut head).await.is_err() || head[0] != qr_proto::uni::FILE {
                            return;
                        }
                        let fid = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
                        let mut buf = vec![0u8; 256 * 1024];
                        loop {
                            match st.read(&mut buf).await {
                                Ok(Some(n)) => hub.send_to_driver(binary(file_message(fid, false, &buf[..n]))),
                                _ => {
                                    hub.send_to_driver(binary(file_message(fid, true, &[])));
                                    return;
                                }
                            }
                        }
                    });
                }
            }));
        }
        // Feedback: the slowest viewer's, every 50 ms; a quiet word now and
        // then even when nobody reports, so the host's watchdog stays calm.
        {
            let (hub, control) = (self.clone(), control_s.clone());
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(50));
                let mut last_sent = Instant::now();
                let mut seq = 0u32;
                loop {
                    tick.tick().await;
                    let now = Instant::now();
                    // Each viewer measures delays on its own clock, so they
                    // are compared and forwarded relative to that viewer's
                    // own floor: the host then sees queueing, not clocks.
                    let worst = {
                        let i = hub.lock();
                        i.viewers
                            .iter()
                            .filter_map(|v| v.feedback.as_ref().map(|f| (f, v.floor())))
                            .filter(|((_, at), _)| now - *at < FEEDBACK_FRESH)
                            .map(|((fb, _), floor)| {
                                let mut fb = fb.clone();
                                let floor = floor.unwrap_or(fb.delay_min_us);
                                fb.delay_min_us = fb.delay_min_us.saturating_sub(floor);
                                fb.delay_max_us = fb.delay_max_us.saturating_sub(floor);
                                fb
                            })
                            .max_by_key(|fb| fb.delay_max_us)
                    };
                    let fb = match worst {
                        Some(fb) => fb,
                        None if now - last_sent >= Duration::from_millis(200) => {
                            seq = seq.wrapping_add(1);
                            Feedback { seq, window_ms: 200, ..Default::default() }
                        }
                        None => continue,
                    };
                    last_sent = now;
                    if control.send(Msg::Feedback(fb).encode()).is_err() {
                        return;
                    }
                }
            }));
        }
        let mut i = self.lock();
        if i.generation != generation {
            // Everyone left while connecting: nothing to serve.
            drop(i);
            conn.close(0u32.into(), b"bye");
            for t in &tasks {
                t.abort();
            }
            self.ctx.web_peers.0.lock().expect("lock").remove(&local_port);
            return Ok(());
        }
        i.connecting = false;
        i.link = Some(Link {
            conn,
            control: control_s,
            input: input_s,
            files: HashMap::new(),
            generation,
            tasks,
            host_paused: false,
        });
        tracing::info!(port = local_port, "session opened for WebSocket viewers");
        Ok(())
    }

    fn on_host_msg(&self, body: &[u8]) {
        let m = match Msg::decode(body) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("bad host message: {e}");
                return;
            }
        };
        let mut i = self.lock();
        let seats = matches!(m, Msg::DisplayInfo { .. });
        match &m {
            Msg::HelloAck { .. } => i.cache.hello_ack = Some(m.clone()),
            Msg::SessionStart { .. } => i.cache.session_start = Some(m.clone()),
            Msg::MonitorList { .. } => i.cache.monitor_list = Some(m.clone()),
            Msg::StreamStarted { stream: 0, .. } => i.cache.stream_started = Some(m.clone()),
            Msg::DisplayInfo { stream: 0, .. } => i.cache.display_info = Some(m.clone()),
            Msg::CursorInfo { .. } => i.cache.cursor_info = Some(m.clone()),
            Msg::Cursor { .. } => i.cache.cursor = Some(m.clone()),
            Msg::Error { .. } => i.cache.error = Some(m.clone()),
            Msg::HostStats(s) => i.cache.stats = Some(s.clone()),
            _ => {}
        }
        let msg = binary(control_message(&m));
        for v in i.viewers.iter().filter(|v| v.joined.is_some()) {
            let _ = v.tx.send(msg.clone());
        }
        if seats {
            self.broadcast_seats(&mut i);
        }
    }

    fn on_frame(&self, f: &qr_fec::Frame) {
        let key = f.meta.flags & fflags::KEYFRAME != 0;
        let mut i = self.lock();
        let mut want = false;
        let mut msg: Option<Message> = None;
        for v in i.viewers.iter_mut().filter(|v| v.joined.is_some() && !v.paused) {
            if (v.want_key || v.lagging) && !key {
                want = true;
                continue;
            }
            let mut q = v.queue.lock().expect("lock");
            if !key && backlogged(&q, f.meta.send_us) {
                v.want_key = true;
                want = true;
                continue;
            }
            v.want_key = false;
            v.lagging = false;
            q.push_back(f.meta.send_us);
            drop(q);
            let m = msg.get_or_insert_with(|| binary(video_message(f.stream, &f.meta, &f.au)));
            let _ = v.tx.send(m.clone());
        }
        if want {
            Self::request_keyframe(&mut i);
        }
    }

    fn broadcast_datagram(&self, d: &[u8]) {
        let i = self.lock();
        let mut msg = Vec::with_capacity(d.len() + 1);
        msg.push(channel::DATAGRAM);
        msg.extend_from_slice(d);
        let m = binary(msg);
        for v in i.viewers.iter().filter(|v| v.joined.is_some() && !v.paused) {
            let _ = v.tx.send(m.clone());
        }
    }

    fn send_to_driver(&self, m: Message) {
        let i = self.lock();
        if let Some(v) = i.viewers.iter().find(|v| v.driver) {
            let _ = v.tx.send(m);
        }
    }

    /// The host closed the session (an error, or it was replaced): every
    /// viewer hears why and is let go.
    fn link_ended(&self, generation: u64) {
        let mut i = self.lock();
        if i.link.as_ref().is_none_or(|l| l.generation != generation) {
            return;
        }
        let err = i.cache.error.clone();
        tracing::info!(reason = ?err, "the host ended the WebSocket viewers' session");
        for v in i.viewers.iter().filter(|v| v.joined.is_some()) {
            if let Some(e) = &err {
                let _ = v.tx.send(binary(control_message(e)));
            }
            let _ = v.tx.send(Message::Close(None));
        }
        i.link = None;
        i.cache = Cache::default();
        for v in &mut i.viewers {
            v.joined = None;
            v.driver = false;
        }
    }

    /// `GET /status`.
    pub fn status_json(&self) -> serde_json::Value {
        let i = self.lock();
        let (mut width, mut height, mut codec) = (0u16, 0u16, "");
        if let Some(Msg::StreamStarted { width: w, height: h, codec: c, .. }) = &i.cache.stream_started {
            (width, height) = (*w, *h);
            codec = match *c {
                1 => "h264",
                2 => "h265",
                3 => "av1",
                _ => "",
            };
        }
        if width == 0 {
            let (w, h, _) = self.ctx.display_mode(0, 0, 0);
            (width, height) = (w as u16, h as u16);
        }
        let scale = match &i.cache.display_info {
            Some(Msg::DisplayInfo { scale_pct, .. }) => *scale_pct as f64 / 100.0,
            _ => 1.0,
        };
        let encoder = i
            .cache
            .stats
            .as_ref()
            .map(|s| s.encoder.clone())
            .or_else(|| self.ctx.encoders.first().map(|e| e.element.to_string()))
            .unwrap_or_default();
        let display: serde_json::Map<String, serde_json::Value> =
            self.ctx.desktop_env().into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
        json!({
            "running": true,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": format!("{}.{}", qr_proto::PROTO_MAJOR, qr_proto::PROTO_MINOR),
            "viewers": i.viewers.iter().filter(|v| v.joined.is_some()).count(),
            "session": i.link.is_some(),
            "width": width,
            "height": height,
            "scale": scale,
            "codec": codec,
            "encoder": encoder,
            "renderer": self.ctx.vd.as_ref().map(|v| if v.lock().expect("lock").gpu { "gpu" } else { "software" }),
            "source": self.ctx.args.source,
            "cursorInPicture": self.ctx.cursor_in_picture,
            "display": display,
            "missing": missing_programs(&self.ctx),
        })
    }
}

/// Programs a virtual-display host needs but cannot find on PATH.
fn missing_programs(ctx: &HostCtx) -> Vec<String> {
    if !ctx.args.source.starts_with("vdisplay") {
        return Vec::new();
    }
    let mut want = vec!["sway", "swaymsg", "pipewire", "wireplumber", "dbus-daemon"];
    let desktop_first = ctx.args.desktop.as_deref().and_then(|d| d.split_whitespace().next());
    if ctx.args.desktop.is_some() {
        want.extend(["Xwayland", "xrandr"]);
    }
    if let Some(d) = desktop_first {
        want.push(d);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    want.into_iter()
        .filter(|p| !std::env::split_paths(&path).any(|d| d.join(p).is_file()))
        .map(str::to_string)
        .collect()
}

/// A viewer's file upload: one host stream per file id.
async fn upload(conn: quinn::Connection, fid: u32, mut rx: mpsc::UnboundedReceiver<(Vec<u8>, bool)>) {
    let Ok(mut st) = conn.open_uni().await else { return };
    let mut head = vec![qr_proto::uni::FILE];
    head.extend_from_slice(&fid.to_le_bytes());
    if st.write_all(&head).await.is_err() {
        return;
    }
    while let Some((chunk, last)) = rx.recv().await {
        if !chunk.is_empty() && st.write_all(&chunk).await.is_err() {
            return;
        }
        if last {
            let _ = st.finish();
            // Let the data drain before the stream handle goes.
            let _ = st.stopped().await;
            return;
        }
    }
}

/// Serves `--ws-listen`: `/ws` upgrades to a viewer, `/status` answers JSON.
pub async fn run(hub: Arc<Hub>, listen: SocketAddr) -> Result<()> {
    if !listen.ip().is_loopback() {
        bail!("--ws-listen must be a loopback address (127.0.0.1 or ::1): the WebSocket transport has no pairing");
    }
    let tcp = TcpListener::bind(listen).await.with_context(|| format!("binding TCP {listen}"))?;
    println!("WS-LISTEN {}", tcp.local_addr()?);
    loop {
        let Ok((stream, peer)) = tcp.accept().await else { continue };
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(hub, stream, peer).await {
                tracing::debug!(%peer, "ws: {e:#}");
            }
        });
    }
}

/// The request head, peeked (not consumed): method, path and whether it
/// asks for a WebSocket.
async fn peek_request(stream: &TcpStream) -> Result<(String, bool)> {
    let mut buf = vec![0u8; 8192];
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let n = stream.peek(&mut buf).await?;
        let head = &buf[..n];
        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&head[..pos]);
            let path = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
            let upgrade = text.lines().any(|l| {
                let l = l.to_ascii_lowercase();
                l.starts_with("upgrade:") && l.contains("websocket")
            });
            return Ok((path.split('?').next().unwrap_or("/").to_string(), upgrade));
        }
        if n == buf.len() || n == 0 || Instant::now() > end {
            bail!("no request head");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn serve(hub: Arc<Hub>, mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let (path, upgrade) = peek_request(&stream).await?;
    if upgrade && path == "/ws" {
        let ws = tokio_tungstenite::accept_async(stream).await.context("WebSocket handshake")?;
        return serve_viewer(hub, ws, peer).await;
    }
    // Plain HTTP: consume the request, answer.
    let mut buf = vec![0u8; 8192];
    let _ = stream.read(&mut buf).await?;
    let (status, ctype, body) = if path == "/status" {
        ("200 OK", "application/json", serde_json::to_vec(&hub.status_json())?)
    } else {
        ("404 Not Found", "text/plain", b"not found".to_vec())
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn serve_viewer(
    hub: Arc<Hub>,
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    peer: SocketAddr,
) -> Result<()> {
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let (id, queue) = hub.add_viewer(tx);
    tracing::info!(%peer, viewer = id, "WebSocket viewer connected");
    // The writer: once a frame is on the socket, it leaves the backlog.
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let video = matches!(&m, Message::Binary(b) if b.first() == Some(&channel::VIDEO));
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() {
                return;
            }
            if video {
                queue.lock().expect("lock").pop_front();
            }
            if close {
                return;
            }
        }
    });
    let result = async {
        while let Some(m) = source.next().await {
            match m? {
                Message::Binary(b) => hub.on_viewer_msg(id, &b).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    hub.remove_viewer(id);
    writer.abort();
    tracing::info!(%peer, viewer = id, "WebSocket viewer left");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_round_trip() {
        let s = Seat { driver: true, scale_pct: 200, driver_ratio_pct: 150, viewers: 3, cursor_in_picture: false };
        assert_eq!(Seat::decode(&s.encode()).unwrap(), s);
        assert_eq!(decode_claim(&encode_claim("abc")[1..]).unwrap(), "abc");
    }

    #[test]
    fn video_message_layout() {
        let meta = FrameMeta {
            codec: 1,
            flags: fflags::KEYFRAME,
            width: 1280,
            height: 720,
            send_us: 42,
            ..Default::default()
        };
        let m = video_message(0, &meta, &[1, 2, 3]);
        assert_eq!(&m[..2], &[channel::VIDEO, 0]);
        let (parsed, au) = FrameMeta::parse(&m[2..]).unwrap();
        assert_eq!(parsed.len, 3);
        assert_eq!(parsed.width, 1280);
        assert_eq!(au, &[1, 2, 3]);
    }

    #[test]
    fn backlog_by_time_and_count() {
        let mut q: VecDeque<u64> = VecDeque::new();
        assert!(!backlogged(&q, 1_000_000));
        q.push_back(1_000_000);
        assert!(!backlogged(&q, 1_050_000));
        assert!(backlogged(&q, 1_100_001));
        let full: VecDeque<u64> = (0..BACKLOG_FRAMES as u64).collect();
        assert!(backlogged(&full, 60));
    }

    fn viewer(id: u64, joined: Option<Instant>) -> Viewer {
        let (tx, _rx) = mpsc::unbounded_channel();
        Viewer {
            id,
            viewer_id: format!("v{id}"),
            tx,
            queue: Default::default(),
            driver: false,
            paused: false,
            want_key: true,
            joined,
            ratio_pct: 100,
            feedback: None,
            delay_floor: VecDeque::new(),
            lagging: false,
            lag_hits: 0,
            lag_at: None,
        }
    }

    #[test]
    fn lag_from_feedback_sets_and_clears() {
        let now = Instant::now();
        let mut v = viewer(1, Some(now));
        let fb =
            |min: i32, max: i32| Feedback { frames_rx: 3, delay_min_us: min, delay_max_us: max, ..Default::default() };
        v.want_key = false;
        v.note_feedback(&fb(1000, 5000), now);
        assert!(!v.lagging);
        // One late window is jitter.
        v.note_feedback(&fb(1000, 400_000), now + Duration::from_millis(50));
        assert!(!v.lagging && !v.want_key);
        v.note_feedback(&fb(1000, 40_000), now + Duration::from_millis(100));
        // Three in a row is a backlog: catch up at the next keyframe.
        for i in 0..3 {
            v.note_feedback(&fb(1000, 400_000), now + Duration::from_millis(150 + 50 * i));
        }
        assert!(v.lagging && v.want_key);
        // The keyframe went out: back to every frame, and no second
        // catch-up within the cooldown however late the reports stay.
        v.lagging = false;
        v.want_key = false;
        for i in 0..5 {
            v.note_feedback(&fb(1000, 400_000), now + Duration::from_millis(400 + 50 * i));
        }
        assert!(!v.lagging);
        for i in 0..3 {
            v.note_feedback(&fb(1000, 400_000), now + Duration::from_millis(2500 + 50 * i));
        }
        assert!(v.lagging, "after the cooldown a lasting backlog catches up again");
        // The floor forgets old minima.
        let mut w = viewer(2, Some(now));
        w.note_feedback(&fb(1000, 5000), now);
        w.note_feedback(&fb(300_000, 320_000), now + Duration::from_secs(6));
        assert!(!w.lagging, "a new floor after the window, not a lag");
    }

    #[test]
    fn driver_moves_to_the_longest_connected_viewer() {
        let now = Instant::now();
        let mut i = Inner::default();
        i.viewers.push(viewer(1, Some(now)));
        i.viewers.push(viewer(2, Some(now + Duration::from_secs(2))));
        i.viewers.push(viewer(3, Some(now + Duration::from_secs(1))));
        Hub::make_driver(&mut i, 1);
        // The driver leaves (what remove_viewer does under the lock).
        let v = i.viewers.remove(0);
        assert!(v.driver);
        i.left_driver = Some((v.viewer_id.clone(), Instant::now()));
        if let Some(next) = i.viewers.iter_mut().filter(|v| v.joined.is_some()).min_by_key(|v| v.joined) {
            next.driver = true;
        }
        assert!(i.viewers.iter().find(|v| v.id == 3).unwrap().driver);
        assert!(!i.viewers.iter().find(|v| v.id == 2).unwrap().driver);
        // A claim moves it, and clears the reclaim.
        Hub::make_driver(&mut i, 2);
        assert!(i.viewers.iter().find(|v| v.id == 2).unwrap().driver);
        assert!(i.left_driver.is_none());
    }
}
