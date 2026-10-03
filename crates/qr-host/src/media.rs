//! Session audio (host → client) and incoming datagrams (microphone,
//! pointer motion, gamepads).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, bail};
use bytes::Bytes;
use qr_media::audio::{AudioCapture, AudioPlayer, AudioSink, AudioSource};
use qr_proto::datagram::{AudioPacket, kind};

use crate::HostCtx;

fn audio_source(ctx: &HostCtx) -> Option<AudioSource> {
    let a = ctx.args.audio.as_str();
    Some(match a.split_once(':').unwrap_or((a, "")) {
        ("none", _) => return None,
        ("auto", _) if ctx.args.source.starts_with("test") => AudioSource::Test { wave: "ticks".into() },
        ("auto", _) | ("default", _) => AudioSource::Default,
        ("test", w) => AudioSource::Test { wave: if w.is_empty() { "sine".into() } else { w.into() } },
        ("pipewire", t) => AudioSource::PipeWire { target: t.into() },
        ("pulse", d) => AudioSource::Pulse { device: (!d.is_empty()).then(|| d.into()) },
        _ => AudioSource::Default,
    })
}

/// Starts host audio capture, sending Opus packets as datagrams.
pub fn start_audio(ctx: &HostCtx, conn: &quinn::Connection) -> Result<Option<AudioCapture>> {
    let Some(src) = audio_source(ctx) else { return Ok(None) };
    let conn = conn.clone();
    let seq = AtomicU32::new(0);
    let clock = qr_media::ClockMap::new(qr_net::clock::now_us);
    let cap = AudioCapture::start(&src, 2, 128_000, clock, move |data, ts_us| {
        let p = AudioPacket { mic: false, seq: seq.fetch_add(1, Ordering::Relaxed), ts_us, data };
        let _ = conn.send_datagram(Bytes::from(p.encode()));
    })?;
    tracing::info!(?src, "audio");
    Ok(Some(cap))
}

fn mic_sink(ctx: &HostCtx) -> Result<Option<AudioSink>> {
    let m = ctx.args.mic_out.as_str();
    Ok(match m.split_once(':').unwrap_or((m, "")) {
        ("none", _) => None,
        ("file", p) => Some(AudioSink::File { path: p.into() }),
        ("pipewire", t) => Some(AudioSink::PipeWire { target: t.into() }),
        _ => bail!("--mic-out must be none, file:<wav> or pipewire:<target>"),
    })
}

/// Reads datagrams from the client until the connection ends.
pub async fn datagram_loop(ctx: Arc<HostCtx>, conn: quinn::Connection) {
    let mut mic: Option<AudioPlayer> = None;
    let sink = mic_sink(&ctx).unwrap_or_else(|e| {
        tracing::warn!("{e}");
        None
    });
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&kind::MIC) => {
                let Some(sink) = &sink else { continue };
                let Ok(p) = AudioPacket::decode(&d) else { continue };
                if mic.is_none() {
                    match AudioPlayer::start(sink, 1) {
                        Ok(m) => mic = Some(m),
                        Err(e) => {
                            tracing::warn!("microphone output failed: {e:#}");
                            continue;
                        }
                    }
                }
                if let Some(m) = &mic {
                    let _ = m.push(p.seq, &p.data);
                }
            }
            Some(&kind::MOTION) | Some(&kind::PAD) => {
                crate::input::datagram(&ctx, &d);
            }
            _ => {}
        }
    }
    if let Some(m) = mic {
        m.finish();
    }
}
