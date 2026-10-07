# QuickRemote protocol, version 1.0

All integers are little-endian. `str` and `bytes` are a `u32` length followed by
the bytes (UTF-8 for `str`), at most 16 MiB. `list<T>` is a `u16` count followed
by the items. `bool` is one byte, 0 or 1. Fixed arrays (`[u8; N]`) are raw.

Reference implementation: `crates/qr-proto`. Test vectors (hex of one sample of
every message): `conformance/vectors.json`, checked by `cargo test -p qr-proto`.

## Connection

One QUIC v1 connection per session, TLS 1.3, ALPN `qr/1`, QUIC datagrams
enabled. Both sides present their device certificate (self-signed ECDSA P-256).
Trust is decided after the handshake by the SHA-256 fingerprint of the peer's
certificate (DER): known from an earlier pairing, or established by PIN pairing.

| Channel | Opened by | Carries |
|---|---|---|
| Control stream: the first bidirectional stream | client | `u32` length + `Msg`, both ways |
| Input stream: unidirectional, first byte `0x01` | client | `u32` length + `InputEvent`, for the whole session |
| File stream: unidirectional, first byte `0x02` | sender | `u32` file id, then the file bytes until FIN |
| Datagrams | both | first byte is the kind; see below |

A length-prefixed frame is at most 17 MiB.

## Session start

```
client → Hello
host   → HelloAck            (need_pin = client unknown)
  [pairing, when need_pin]
client → PairSpake(A)        host → PairSpake(B)
client → PairConfirm(mac_c)  host → PairConfirm(mac_h)   (or Error PinWrong)
host   → SessionStart, MonitorList, StreamStarted(stream 0)
```

A host refuses a client with a different major version by sending `Error`
(code 1, `VersionMismatch`) and closing.

### PIN pairing

SPAKE2 over the Ed25519 group (the `spake2` crate's construction), password =
the 6 ASCII digits, identities `"qr-client"` (side A) and `"qr-host"` (side B).
Both sides compute

```
transcript = "quickremote pairing v1" ‖ client_fp[32] ‖ host_fp[32] ‖ exporter[32]
exporter   = TLS-Exporter(label "EXPORTER-qr-pair", context "", 32)
mac_c      = HMAC-SHA256(K, "client" ‖ transcript)
mac_h      = HMAC-SHA256(K, "host"   ‖ transcript)
```

where `K` is the SPAKE2 key. The client sends `mac_c` first; the host checks it
before answering with `mac_h`. Binding the fingerprints and the exporter means a
man in the middle fails confirmation even with the right PIN. After 5 failures
within a minute from one IP address, the host ignores that address for 60 s.

## Control messages (`Msg`)

`tag: u8`, then the fields in order.

| Tag | Name | Fields | Direction |
|---|---|---|---|
| 1 | Hello | major u16, minor u16, name str, os str, want_width u16, want_height u16, want_refresh u16, decoders list<CodecCap> | C→H |
| 2 | HelloAck | major u16, minor u16, name str, host_id str, need_pin bool, rendezvous str | H→C |
| 3 | PairSpake | data bytes | both |
| 4 | PairConfirm | mac [u8; 32] | both |
| 5 | SessionStart | profile u8 (1 LAN, 2 internet), path u8 (1 LAN, 2 direct, 3 relayed), modes u32 | H→C |
| 6 | Ping | seq u32, t_client u64 | C→H |
| 7 | Pong | seq u32, t_client u64, t_host u64 | H→C |
| 8 | Feedback | seq u32, window_ms u16, bytes_rx u32, shards_rx u32, shards_lost u32, shards_repaired u32, frames_rx u16, frames_lost u16, delay_min_us i32, delay_max_us i32, delay_slope i32 (µs/s), decode_us u32, present_us u32 | C→H |
| 9 | FrameLost | stream u8, frame u32 | C→H |
| 10 | KeyframeReq | stream u8 | C→H |
| 11 | SetMode | modes u32 | both |
| 12 | MonitorList | monitors list<Monitor> | H→C |
| 13 | SelectMonitor | stream u8, monitor u8 | C→H |
| 14 | StreamStarted | stream u8, monitor u8, codec u8, chroma444 bool, width u16, height u16, fps u16 | H→C |
| 15 | StreamStop | stream u8 | both |
| 16 | Resize | stream u8, width u16, height u16, refresh u16 | C→H |
| 17 | Clipboard | kind u8 (1 text, 2 PNG), data bytes | both |
| 18 | FileOffer | id u32, name str, size u64, sha256 [u8; 32] | both |
| 19 | FileAccept | id u32 | both |
| 20 | FileCancel | id u32, reason str | both |
| 21 | FileDone | id u32, ok bool | both |
| 22 | PadAttach | idx u8, kind u8 | C→H |
| 23 | PadDetach | idx u8 | C→H |
| 24 | Rumble | idx u8, strong u16, weak u16, ms u16 | H→C |
| 25 | HostStats | stream u8, bitrate_kbps u32, fps u16, parity_pct u8, profile u8, encoder str, capture_us u32, encode_us u32, dropped_by_backpressure u32 | H→C |
| 26 | Error | code u16, text str | both |
| 27 | Bye | — | C→H |

`CodecCap`: codec u8 (1 H.264, 2 H.265, 3 AV1), hw bool, max_width u16,
max_height u16, chroma444 bool.

`Monitor`: id u8, name str, width u16, height u16, refresh_mhz u32, x i32, y i32,
primary bool.

Modes (`SetMode`, `SessionStart`): bit 0 relative mouse, 1 sharp text,
2 microphone, 3 clipboard, 4 audio.

Streams: stream 0 starts with the session. `SelectMonitor` on a running
stream moves it to that monitor; on a stream id not running (1–3), it opens
one more stream (a second window). `StreamStop` from the client closes an
extra stream, and the host confirms with its own `StreamStop`. Stream 0
can't be stopped. Each (re)start is announced by `StreamStarted` and begins
with a keyframe. A restarted stream keeps its frame numbering, so the
client's reassembler and decoder carry on. Video datagrams carry the stream
id. `FrameLost`/`KeyframeReq` name a stream; `Feedback` describes the link
and applies to every stream. `Resize` changes a virtual display's monitor
mode; the host applies it once requests stop for 300 ms. Real desktops
ignore it, and the client scales the picture instead.

Error codes: 1 version mismatch, 2 PIN required, 3 wrong PIN, 4 rate limited,
5 no common codec, 6 busy, 7 capture failed, 8 internal, 9 not paired.

`Feedback` is sent every 50 ms. `delay_*` are frame transit times (client
arrival minus host `send_us`), measured on unsynchronized clocks: only their
differences and slope are meaningful.

## Input events (`InputEvent`, input stream)

| Tag | Name | Fields |
|---|---|---|
| 1 | Key | code u16 (Linux evdev `KEY_*`, by physical position), down bool |
| 2 | Button | button u8 (1 left, 2 middle, 3 right, 4 back, 5 forward), down bool |
| 3 | Abs | stream u8, x u16, y u16 (stream pixels) |
| 4 | Wheel | dx i16, dy i16 (1/120 notch, positive up/right) |
| 5 | Text | text str |
| 6 | ReleaseAll | — |
| 7 | Pen | stream u8, x u32, y u32 (stream pixels × 16), pressure u16, tilt_x i8, tilt_y i8, flags u8 (1 in range, 2 contact, 4 eraser, 8 barrel) |

## Datagrams

First byte = kind.

### 1 — Video shard

```
kind u8 = 1, stream u8, flags u8 (bit 0 keyframe), frame u32,
block u8, blocks u8, shard u8, k u8, m u8, payload
```

A frame's payload is `FrameMeta ‖ access unit`, cut into equal-size data shards
(the last zero-padded) and grouped into `blocks` blocks of at most 32 data
shards (`k`). Each block adds `m` parity shards: GF(2^8) Reed-Solomon with the
systematic Vandermonde-derived matrix of the Backblaze construction
(`reed-solomon-erasure`). Shards `0..k` are data and `k..k+m` parity. All shards
of a block have the same length. Any `k` shards of a block rebuild it.

`FrameMeta` (42 bytes): codec u8, flags u8 (1 keyframe, 2 4:4:4, 4 still
refresh), width u16, height u16, len u32 (access-unit length), capture_us u64,
encode_start_us u64, encode_end_us u64, send_us u64 (host monotonic clock).

The access unit is H.264/H.265 Annex B (with SPS/PPS/VPS on every keyframe) or
an AV1 temporal unit. There are no B-frames. Frame ids count up from 0 per
stream. A client that cannot complete a frame sends `FrameLost` and discards
frames until the next keyframe.

### 2 / 3 — Audio / microphone

`kind u8, seq u32, ts_us u64, opus packet` (48 kHz stereo for audio, mono for
the microphone).

### 4 — Motion

`kind u8 = 4, seq u32, rel_x i64, rel_y i64, has_abs bool, stream u8,
abs_x u16, abs_y u16, t_client_us u64`. `rel_*` are running totals since the
session started; the host applies the difference from the last total it
applied, so loss and reordering never corrupt relative motion. Datagrams older
than the newest `seq` seen are ignored.

### 5 — Pad state

`kind u8 = 5, idx u8, seq u32, buttons u32, axes [i16; 6]`, a full snapshot. The
host keeps the newest by `seq`. Buttons, Xbox layout: A, B, X, Y, LB, RB, Back,
Start, Guide, LS, RS, Up, Down, Left, Right (bits 0–14). Axes: LX, LY, RX, RY
(−32768..32767, +Y down), LT, RT (0..32767).

### 6 — Probe

`kind u8 = 6, seq u32, ts_us u64, pad_len u16, pad_len zero bytes`.

## Rendezvous (`qr-rv/1`)

A separate QUIC connection to `qr-relay`, one bidirectional stream with
length-prefixed `RvMsg`s:

| Tag | Name | Fields |
|---|---|---|
| 1 | Challenge | nonce [u8; 32] |
| 2 | Register | host_id str, cert_der bytes, signature bytes, locals list<str> |
| 3 | Registered | observed str |
| 4 | Lookup | host_id str, locals list<str> |
| 5 | Introduce | token [u8; 16], peer_observed str, peer_locals list<str>, punch_in_ms u16, initiator bool |
| 6 | RelayRequest | token [u8; 16] |
| 7 | RelayAllocated | token [u8; 16], relay_addr str |
| 8 | Error | code u16, text str |

`signature` is ECDSA P-256 / SHA-256 (ASN.1) by the host's identity key over
`"quickremote rendezvous register v1" ‖ nonce`. The relay checks that the
certificate's fingerprint gives `host_id` (`qr-` + base32 of its first 8 bytes,
grouped 4-4-5). Hole-punch packets start with `QRPUNCH\0` and are dropped by the
receiver. Relayed packets carry the 16-byte token in front, which the relay
strips.
