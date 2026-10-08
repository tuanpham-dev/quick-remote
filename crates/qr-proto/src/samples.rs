//! One sample of every message, used for round-trip tests and to generate
//! `conformance/vectors.json` (`cargo run -p qr-proto --bin gen-vectors`).

use crate::datagram::{AudioPacket, FrameMeta, Motion, PadState, Probe, VideoShardHeader};
use crate::input::{InputEvent, pen};
use crate::msg::*;
use crate::rendezvous::RvMsg;
use crate::{PROTO_MAJOR, PROTO_MINOR, Writer};

pub struct Sample {
    pub name: &'static str,
    /// `msg`, `input`, `rv`, `video`, `meta`, `audio`, `motion`, `pad`, `probe`.
    pub kind: &'static str,
    pub bytes: Vec<u8>,
}

fn s(name: &'static str, kind: &'static str, bytes: Vec<u8>) -> Sample {
    Sample { name, kind, bytes }
}

pub fn msgs() -> Vec<(&'static str, Msg)> {
    vec![
        (
            "Hello",
            Msg::Hello {
                major: PROTO_MAJOR,
                minor: PROTO_MINOR,
                name: "laptop".into(),
                os: "linux".into(),
                want_width: 1920,
                want_height: 1080,
                want_refresh: 60,
                decoders: vec![
                    CodecCap { codec: Codec::H265, hw: true, max_width: 4096, max_height: 2304, chroma444: false },
                    CodecCap { codec: Codec::H264, hw: false, max_width: 4096, max_height: 2304, chroma444: true },
                ],
            },
        ),
        (
            "HelloAck",
            Msg::HelloAck {
                major: 1,
                minor: 0,
                name: "studio-pc".into(),
                host_id: "qr-7f3a-91c2-0d4e".into(),
                need_pin: true,
                rendezvous: "rv.example.net:4500".into(),
            },
        ),
        ("PairSpake", Msg::PairSpake { data: vec![0x41; 33] }),
        ("PairConfirm", Msg::PairConfirm { mac: [7; 32] }),
        ("SessionStart", Msg::SessionStart { profile: 1, path: 1, modes: mode::AUDIO | mode::CLIPBOARD }),
        ("Ping", Msg::Ping { seq: 9, t_client: 1_234_567 }),
        ("Pong", Msg::Pong { seq: 9, t_client: 1_234_567, t_host: 9_876_543_210 }),
        (
            "Feedback",
            Msg::Feedback(Feedback {
                seq: 3,
                window_ms: 50,
                bytes_rx: 400_000,
                shards_rx: 300,
                shards_lost: 2,
                shards_repaired: 2,
                frames_rx: 3,
                frames_lost: 0,
                delay_min_us: 300,
                delay_max_us: 900,
                delay_slope: -12,
                decode_us: 1800,
                present_us: 900,
            }),
        ),
        ("FrameLost", Msg::FrameLost { stream: 0, frame: 77 }),
        ("KeyframeReq", Msg::KeyframeReq { stream: 1 }),
        ("SetMode", Msg::SetMode { modes: mode::RELATIVE_MOUSE | mode::SHARP_TEXT }),
        (
            "MonitorList",
            Msg::MonitorList {
                monitors: vec![Monitor {
                    id: 0,
                    name: "HEADLESS-1".into(),
                    width: 1920,
                    height: 1080,
                    refresh_mhz: 60_000,
                    x: 0,
                    y: -10,
                    primary: true,
                }],
            },
        ),
        ("SelectMonitor", Msg::SelectMonitor { stream: 0, monitor: 1 }),
        (
            "StreamStarted",
            Msg::StreamStarted {
                stream: 0,
                monitor: 0,
                codec: Codec::H264 as u8,
                chroma444: false,
                width: 1920,
                height: 1080,
                fps: 60,
            },
        ),
        ("StreamStop", Msg::StreamStop { stream: 1 }),
        ("Resize", Msg::Resize { stream: 0, width: 2560, height: 1440, refresh: 120 }),
        ("Clipboard", Msg::Clipboard { kind: ClipKind::Text as u8, data: "héllo".as_bytes().to_vec() }),
        ("FileOffer", Msg::FileOffer { id: 5, name: "a.bin".into(), size: 1 << 33, sha256: [0xab; 32] }),
        ("FileAccept", Msg::FileAccept { id: 5 }),
        ("FileCancel", Msg::FileCancel { id: 5, reason: "cancelled".into() }),
        ("FileDone", Msg::FileDone { id: 5, ok: true }),
        ("PadAttach", Msg::PadAttach { idx: 2, kind: 1 }),
        ("PadDetach", Msg::PadDetach { idx: 2 }),
        ("Rumble", Msg::Rumble { idx: 0, strong: 65535, weak: 1000, ms: 200 }),
        (
            "HostStats",
            Msg::HostStats(HostStats {
                stream: 0,
                bitrate_kbps: 60_000,
                fps: 120,
                parity_pct: 10,
                profile: 1,
                encoder: "vah264lpenc".into(),
                capture_us: 400,
                encode_us: 3100,
                dropped_by_backpressure: 0,
            }),
        ),
        ("Error", Msg::Error { code: ErrorCode::PinWrong as u16, text: "That PIN doesn't match.".into() }),
        ("Bye", Msg::Bye),
        (
            "DisplayRequest",
            Msg::DisplayRequest { stream: 0, width: 2800, height: 1800, refresh: 60, scale_pct: 200, target: 0 },
        ),
        (
            "DisplayInfo",
            Msg::DisplayInfo {
                stream: 0,
                kind: 2,
                width: 1920,
                height: 1080,
                scale_pct: 100,
                note: "2800×1800 isn't available on this monitor".into(),
            },
        ),
        ("Pause", Msg::Pause { paused: true }),
    ]
}

pub fn inputs() -> Vec<(&'static str, InputEvent)> {
    vec![
        ("Key", InputEvent::Key { code: 30, down: true }),
        ("Button", InputEvent::Button { button: 3, down: false }),
        ("Abs", InputEvent::Abs { stream: 0, x: 1919, y: 0 }),
        ("Wheel", InputEvent::Wheel { dx: 0, dy: -120 }),
        ("Text", InputEvent::Text { text: "héllo 漢字".into() }),
        ("ReleaseAll", InputEvent::ReleaseAll),
        (
            "Pen",
            InputEvent::Pen {
                stream: 0,
                x: 16 * 100 + 8,
                y: 16 * 200,
                pressure: 40000,
                tilt_x: -30,
                tilt_y: 45,
                flags: pen::IN_RANGE | pen::CONTACT,
            },
        ),
    ]
}

pub fn rvs() -> Vec<(&'static str, RvMsg)> {
    vec![
        ("Challenge", RvMsg::Challenge { nonce: [1; 32] }),
        (
            "Register",
            RvMsg::Register {
                host_id: "qr-7f3a-91c2-0d4e".into(),
                cert_der: vec![0x30, 0x82, 0x01],
                signature: vec![0x30, 0x45],
                locals: vec!["192.168.1.20:9450".into()],
            },
        ),
        ("Registered", RvMsg::Registered { observed: "203.0.113.7:51000".into() }),
        ("Lookup", RvMsg::Lookup { host_id: "qr-7f3a-91c2-0d4e".into(), locals: vec![] }),
        (
            "Introduce",
            RvMsg::Introduce {
                token: [9; 16],
                peer_observed: "198.51.100.4:40001".into(),
                peer_locals: vec!["10.0.0.5:9450".into(), "[fd00::5]:9450".into()],
                punch_in_ms: 300,
                initiator: true,
            },
        ),
        ("RelayRequest", RvMsg::RelayRequest { token: [9; 16] }),
        ("RelayAllocated", RvMsg::RelayAllocated { token: [9; 16], relay_addr: "203.0.113.1:40007".into() }),
        ("RvError", RvMsg::Error { code: 1, text: "unknown host".into() }),
    ]
}

pub fn video_header() -> VideoShardHeader {
    VideoShardHeader { stream: 0, flags: 1, frame: 4242, block: 1, blocks: 3, shard: 33, k: 32, m: 4 }
}

pub fn frame_meta() -> FrameMeta {
    FrameMeta {
        codec: Codec::H264 as u8,
        flags: crate::datagram::fflags::KEYFRAME,
        width: 1920,
        height: 1080,
        len: 4,
        capture_us: 1_000_000,
        encode_start_us: 1_000_400,
        encode_end_us: 1_003_500,
        send_us: 1_003_600,
    }
}

pub fn all() -> Vec<Sample> {
    let mut v = Vec::new();
    for (n, m) in msgs() {
        v.push(s(n, "msg", m.encode()));
    }
    for (n, m) in inputs() {
        v.push(s(n, "input", m.encode()));
    }
    for (n, m) in rvs() {
        v.push(s(n, "rv", m.encode()));
    }
    let mut w = Writer::new();
    video_header().write(&mut w);
    w.raw(&[0xde, 0xad, 0xbe, 0xef]);
    v.push(s("VideoShard", "video", w.buf));
    let mut w = Writer::new();
    frame_meta().write(&mut w);
    w.raw(&[0, 0, 0, 1]);
    v.push(s("FrameMeta", "meta", w.buf));
    v.push(s(
        "Audio",
        "audio",
        AudioPacket { mic: false, seq: 10, ts_us: 5_000, data: vec![0xfc, 0xff, 0xfe] }.encode(),
    ));
    v.push(s("Mic", "audio", AudioPacket { mic: true, seq: 1, ts_us: 7, data: vec![1] }.encode()));
    v.push(s(
        "Motion",
        "motion",
        Motion {
            seq: 12,
            rel_x: -5000,
            rel_y: 1 << 40,
            has_abs: true,
            stream: 0,
            abs_x: 640,
            abs_y: 360,
            t_client_us: 99,
        }
        .encode(),
    ));
    v.push(s(
        "Pad",
        "pad",
        PadState { idx: 1, seq: 8, buttons: 0b101, axes: [-32768, 32767, 0, 100, 0, 32767] }.encode(),
    ));
    v.push(s("Probe", "probe", Probe { seq: 1, ts_us: 2, pad_len: 3 }.encode()));
    v
}
