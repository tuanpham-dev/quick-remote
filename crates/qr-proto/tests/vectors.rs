//! Every sample round-trips, and `conformance/vectors.json` matches the
//! current encoder byte for byte (regenerate with `gen-vectors` on purpose).

use qr_proto::datagram::{AudioPacket, FrameMeta, Motion, PadState, Probe, VideoShardHeader};
use qr_proto::rendezvous::RvMsg;
use qr_proto::{InputEvent, Msg, Writer, samples};

fn reencode(kind: &str, b: &[u8]) -> Vec<u8> {
    match kind {
        "msg" => Msg::decode(b).unwrap().encode(),
        "input" => InputEvent::decode(b).unwrap().encode(),
        "rv" => RvMsg::decode(b).unwrap().encode(),
        "video" => {
            let (h, p) = VideoShardHeader::parse(b).unwrap();
            let mut w = Writer::new();
            h.write(&mut w);
            w.raw(p);
            w.buf
        }
        "meta" => {
            let (m, au) = FrameMeta::parse(b).unwrap();
            let mut w = Writer::new();
            m.write(&mut w);
            w.raw(au);
            w.buf
        }
        "audio" => AudioPacket::decode(b).unwrap().encode(),
        "motion" => Motion::decode(b).unwrap().encode(),
        "pad" => PadState::decode(b).unwrap().encode(),
        "probe" => Probe::decode(b).unwrap().encode(),
        k => panic!("unknown kind {k}"),
    }
}

#[test]
fn samples_round_trip() {
    for (_, m) in samples::msgs() {
        assert_eq!(Msg::decode(&m.encode()).unwrap(), m);
    }
    for (_, m) in samples::inputs() {
        assert_eq!(InputEvent::decode(&m.encode()).unwrap(), m);
    }
    for (_, m) in samples::rvs() {
        assert_eq!(RvMsg::decode(&m.encode()).unwrap(), m);
    }
    for s in samples::all() {
        assert_eq!(reencode(s.kind, &s.bytes), s.bytes, "{}", s.name);
    }
}

#[test]
fn truncation_never_panics() {
    for s in samples::all() {
        for cut in 0..s.bytes.len() {
            let b = &s.bytes[..cut];
            let _ = match s.kind {
                "msg" => Msg::decode(b).is_ok(),
                "input" => InputEvent::decode(b).is_ok(),
                "rv" => RvMsg::decode(b).is_ok(),
                "video" => VideoShardHeader::parse(b).is_ok(),
                "meta" => FrameMeta::parse(b).is_ok(),
                "audio" => AudioPacket::decode(b).is_ok(),
                "motion" => Motion::decode(b).is_ok(),
                "pad" => PadState::decode(b).is_ok(),
                _ => Probe::decode(b).is_ok(),
            };
        }
    }
}

#[test]
fn vectors_file_matches_encoder() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../conformance/vectors.json"
    );
    let text = std::fs::read_to_string(path).expect("conformance/vectors.json exists");
    for s in samples::all() {
        let hex: String = s.bytes.iter().map(|x| format!("{x:02x}")).collect();
        let needle = format!(
            "\"name\": \"{}\", \"kind\": \"{}\", \"hex\": \"{hex}\"",
            s.name, s.kind
        );
        assert!(
            text.contains(&needle),
            "vector {} is stale; run gen-vectors",
            s.name
        );
    }
}
