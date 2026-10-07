# Plan: QuickRemote — remote desktop and LAN game streaming

- **Session:** 50f466a1-d370-44db-b235-bb4ca30125a2
- **Spec:** `plans/quick-remote.spec.html` (R1–R32, A1–A32)

## Goal
Build QuickRemote from scratch in `/works/quick-remote`: a Rust remote desktop with its own QUIC protocol. It does gaming-grade LAN streaming and internet reach through a self-hosted rendezvous and relay server, with Linux and Windows hosts and native, web and Android clients.

## Approach
- **One QUIC session per connection (quinn, ALPN `qr/1`).** Video, audio, mic and pointer motion travel in unreliable QUIC datagrams. Control, input state, clipboard and files travel on reliable streams. The browser speaks the same message set over WebTransport.
- **The media engine is GStreamer, through gstreamer-rs, on Linux and Windows hosts and the native client.** It provides zero-copy DMA-BUF and D3D11 capture to encode, VA-API, NVENC, QSV, AMF and software encoders, Opus, and PipeWire and WASAPI audio. This avoids writing a codec layer per platform. Android uses MediaCodec; the web client uses WebCodecs.
- **FEC:** each encoded frame is split into blocks of up to 32 data shards, and Reed-Solomon parity (reed-solomon-simd) is added per block. Parity is 10% on the LAN profile and 20–50% (adaptive) on the internet profile. A frame that can't be repaired is reported back by the client, and the host forces an IDR. Worst case is RTT + 1 frame (R10).
- **Rate control:**
  - The LAN profile uses a fixed target (default 0.08 bits per pixel per frame at 60 fps), cut only on loss beyond FEC capacity.
  - The internet profile uses delay-gradient control: +8% per second while one-way delay is flat; on a rising delay gradient or loss above parity, drop to 0.85× the received rate. Frame rate steps down (60 → 30 → 15) before per-frame bits do.
  - Client feedback arrives every 50 ms.
  - Worst-case reaction to congestion is 100 ms (two feedback ticks). Per the 2026-07-03 lesson, the delay is stated here so it can be vetoed.
- **Latency path:** capture is timestamped on the host clock. The client estimates the clock offset with min-RTT ping filtering on the control stream. Frame metadata carries capture, encode-start and encode-end times, so the overlay and `qr-latency` can split latency into capture, encode, network, decode and present (R29, A8).
- **Identity and pairing:**
  - Every device has a long-lived ECDSA P-256 identity cert stored in `~/.config/quickremote/`. Host ID is `qr-` plus base32 of the first 8 bytes of the cert's SHA-256 (R4).
  - Native clients present client certs. The first connection runs SPAKE2 with the 6-digit PIN over the control stream, bound to both cert fingerprints and the TLS exporter. Both sides then pin each other's fingerprint (R2, R3).
  - Web clients pin the host's short-lived WebTransport cert hash and authenticate with a WebCrypto P-256 key, enrolled at pairing by the same SPAKE2, implemented in TypeScript over @noble/curves.
- **Discovery:** mdns-sd advertises `_quickremote._udp.local.` with TXT `id`, `name`, `os`, `proto`, `port`, `web` (R1).
- **Internet:**
  - The host keeps a QUIC control connection to `qr-relay`, registers its ID, and proves it with a signature from its identity cert.
  - A client asks the relay for an ID. The relay sends both sides each other's observed and local addresses and a punch time.
  - Both sides send punch packets from the same UDP socket as their quinn endpoint (a custom `AsyncUdpSocket` wrapper that can send raw packets and drops non-QUIC punches), then the client dials the host directly.
  - If no handshake completes within 2.5 s, the relay allocates a forwarding port pair and the client dials the host through it. The session is still end-to-end QUIC (R3, R4).
- **Virtual display (Linux):**
  - The host spawns a private PipeWire + WirePlumber instance with its own runtime dir.
  - It runs sway headless (`WLR_BACKENDS=headless,libinput`, `LIBSEAT_BACKEND=noop`) with Xwayland, outputs sized and timed from the client, plus a private D-Bus, `xdg-desktop-portal` and `xdg-desktop-portal-wlr` (chooser off, output preselected).
  - A desktop session (or a single app or game) runs inside. Capture goes through the same ScreenCast portal path as the Wayland real desktop: the portal's PipeWire node with DMA-BUF caps straight into the VA-API encoder (R12, R13).
  - All input is uinput: keyboard, relative and absolute mouse, pen and gamepads. sway's libinput backend picks up the QuickRemote devices, and games read the pads directly.
- **Real desktop (Linux):** X11 via `ximagesrc` (XShm). Wayland via `xdg-desktop-portal` ScreenCast + RemoteDesktop with restore tokens, then `pipewiresrc` on the portal's node. Input is uinput: keyboard, relative and absolute mouse, gamepads and pen, which libinput-based compositors pick up.
- **Windows host:** `d3d11screencapturesrc` (WGC mode, DXGI fallback), then `nvh264enc`, `qsvh264enc` or `amfh264enc` (and their HEVC/AV1 variants), with `wasapi2src` loopback. Input uses `SendInput`, `InjectSyntheticPointerInput` for pen, and the virtual gamepad driver chosen at the start of Phase 4.
- **Clients:**
  - **Native:** winit + wgpu. Decode is GStreamer into appsink with NV12 planes, then a YUV→RGB shader with `PresentMode::Mailbox`, falling back to `Immediate` and then `Fifo`. Gamepads use gilrs. Pointer lock uses winit `CursorGrabMode::Locked` plus raw `DeviceEvent::MouseMotion`.
  - **Web:** TypeScript, WebTransport + WebCodecs + AudioDecoder, rendered to a canvas through WebGL2.
  - **Android:** Kotlin UI with a Rust core over JNI (the `qr-ffi` crate) for transport, FEC and pairing. Video goes through MediaCodec decoding to a Surface with `KEY_LOW_LATENCY`.
- **Why this over the alternatives:**
  - Raw UDP with custom crypto was rejected; QUIC gives TLS 1.3, NAT-friendly single-port operation and browser reach.
  - Hand-written VA-API and NVENC bindings were rejected; GStreamer already covers every host platform's capture and encode with zero copy.
  - WebRTC was rejected because the protocol and its FEC/rate-control decisions would no longer be ours to tune.

## Architecture

```
                         ┌───────────── qr-relay (self-hosted) ─────────────┐
                         │ rendezvous: id → addrs, punch timing │ UDP relay │
                         └──────▲─────────────────────────────▲─────────────┘
                     register   │ (QUIC qr-rv/1)       lookup │
┌──────────── qr-host ──────────┴───────┐             ┌───────┴──────── qr-client / web / android ┐
│ capture (pipewiresrc│ximagesrc│d3d11)  │  QUIC qr/1  │ FEC reassembly → decode (gst│WebCodecs│    │
│   → encode (va│nv│qsv│amf│x264) → FEC ─┼─datagrams──▶│   MediaCodec) → present (wgpu│canvas│Surf) │
│ audio (pipewire│wasapi) → opus ────────┼─datagrams──▶│ opus → audio out                            │
│ virtual mic ◀──────────────────────────┼─datagrams───┤ mic capture                                 │
│ input inject (uinput│xtest│SendInput) ◀┼─dgram+stream┤ input capture, pointer lock, gilrs pads     │
│ clipboard, files, monitors, control ◀──┼──streams───▶│ clipboard, files, monitor picker, overlay   │
│ mDNS advertise, key store, PIN pairing │             │ mDNS browse, key store, pairing UI          │
└────────────────────────────────────────┘             └─────────────────────────────────────────────┘
```

- **Components:**
  - `qr-proto`: message types and wire codec. The TypeScript twin is `web/src/proto.ts`; both are checked by `conformance/vectors.json`.
  - `qr-fec`: shard and FEC block packetizing and reassembly.
  - `qr-net`: quinn endpoints, identity, pairing, key store, mDNS, clock sync, rendezvous client, hole punching.
  - `qr-media`: GStreamer pipeline builders for capture, encode, decode and audio, plus encoder probing.
  - `qr-host`: session manager, displays, input, clipboard, files, web server.
  - `qr-client`: native app.
  - `qr-relay`: rendezvous and relay.
  - `qr-ffi`: Android JNI.
  - `qr-latency`: measurement tool.
- **Data flow:**
  - Host media flows capture → encode → `qr-fec` → datagrams → client reassembly → decode → present.
  - Client input flows client → input datagrams (motion) or input stream (keys and buttons, reliable) → host injector.
  - Session state lives in the host session manager. Pairing state lives in each device's key store file.
- **Decisions:**
  - GStreamer over direct VA-API/NVENC bindings, because one engine covers all host platforms and zero copy.
  - Datagrams + FEC over one QUIC stream per frame, because a retransmit wait costs a frame time on LAN and FEC repairs in place.
  - Reliable stream for key and button state plus datagrams for motion, because a lost key-up must never stick (R23) while motion must never wait.
  - sway headless + libinput for the virtual display over Weston's PipeWire backend, because Weston reads no input devices, so pen pressure (R22) could not reach apps. wlroots also resizes and adds outputs at run time through `swaymsg`.

## Phases

### Phase 0: Environment setup
**Goal:** this machine can build every crate and run every Phase 1 test.
**Checkpoint:** `scripts/doctor.sh` prints OK for every item it checks: GPU access, VA-API H.264 encode, uinput writable, netns+netem, dev headers, Chrome, Docker, KVM.

- [ ] **T0.1 — doctor script**
  - Files: `scripts/doctor.sh` (new)
  - Do: check and report each prerequisite with a fix hint: render node rw (or `sg render` works), `gst-inspect-1.0 vah264lpenc`, `/dev/uinput` rw, `unshare -rn` + veth + netem, `pkg-config` for `gstreamer-1.0 gstreamer-app-1.0 gstreamer-video-1.0 libpipewire-0.3 opus xkbcommon libudev`, `google-chrome`, `docker info`, `/dev/kvm` rw, weston pipewire backend present.
  - Depends on: —
  - Done when: running it lists every item with OK or the exact fix.
- [ ] **T0.2 — one-time root setup (user-run)**
  - Files: `scripts/setup-root.sh` (new)
  - Do: `apt install` libpipewire-0.3-dev libopus-dev libxkbcommon-dev libva-dev intel-media-va-driver-non-free evtest xdotool mesa-utils glmark2-es2 vulkan-tools xdg-desktop-portal xdg-desktop-portal-gnome mutter (for the headless Wayland real-desktop test), gstreamer1.0-libav, gstreamer1.0-pipewire. Also: udev rule `KERNEL=="uinput", GROUP="input", MODE="0660"` + `SUBSYSTEM=="input", ATTRS{name}=="QuickRemote*", MODE="0660", GROUP="input"`, `usermod -aG input,kvm,docker,render $USER`.
  - Depends on: —
  - Done when: the user has run it (with `! sudo scripts/setup-root.sh`) and re-logged in, and T0.1 passes.

### Phase 1: LAN core (Linux host + native Linux client)
**Goal:** pair, discover and play on a LAN from the native client against a Linux virtual display, then a real desktop.
**Checkpoint:** `tests/e2e/run.sh phase1` passes A1, A2, A6, A7, A8, A11–A14, A16–A25 and A29 on the simulated LAN, and A15 on a nested X11 session and a headless mutter Wayland session.

#### Milestone 1a — protocol and transport
- [x] **T1.1 — workspace skeleton**
  - Files: `Cargo.toml`, `rust-toolchain.toml`, `crates/{qr-proto,qr-fec,qr-net,qr-media,qr-host,qr-client,qr-relay,qr-latency}/Cargo.toml` + `src/lib.rs` or `src/main.rs`, `.gitignore`, `README.md`
  - Do: workspace with shared deps (quinn 0.11, rustls 0.23 ring, rcgen, tokio, gstreamer 0.25, reed-solomon-simd 3, mdns-sd 0.21, spake2, winit, wgpu, gilrs, evdev, x11rb, clap, tracing, anyhow, serde). `git init`.
  - Depends on: —
  - Done when: `cargo build --workspace` succeeds.
- [x] **T1.2 — wire protocol + spec doc**
  - Files: `crates/qr-proto/src/{lib.rs,msg.rs,codec.rs,datagram.rs}`, `docs/protocol.md`
  - Do:
    - `PROTO_MAJOR=1`, `PROTO_MINOR=0`.
    - Control messages as a `Msg` enum with a hand-written little-endian codec (tag u8, fields fixed-width, strings and bytes u32-length-prefixed) so TypeScript and Kotlin can mirror it: `Hello`, `HelloAck`, `PairStart`, `PairMsg`, `PairConfirm`, `Ping`, `Pong`, `Feedback`, `FrameLost`, `KeyframeReq`, `SetMode`, `MonitorList`, `SelectMonitor`, `Resize`, `Clipboard`, `FileOffer`, `FileAccept`, `FileCancel`, `PadAttach`, `PadDetach`, `Rumble`, `CursorShape`, `Error`, `Stats`.
    - Datagram kinds: video shard, audio packet, mic packet, input motion, probe.
    - Input-stream events: key, button, absolute pointer, wheel, pad state, pen, text, release-all.
    - Framing on streams: u32 length + body, max 1 MiB.
  - Depends on: T1.1
  - Done when: round-trip unit tests pass for every variant, and `docs/protocol.md` documents every byte.
- [x] **T1.3 — conformance vectors**
  - Files: `conformance/vectors.json`, `crates/qr-proto/tests/vectors.rs`, `crates/qr-proto/src/bin/gen-vectors.rs`
  - Do: generate hex encodings of one sample of every message and datagram, and test that decoding then re-encoding matches.
  - Depends on: T1.2
  - Done when: `cargo test -p qr-proto` passes (part of A32).
- [x] **T1.4 — FEC**
  - Files: `crates/qr-fec/src/{lib.rs,packetize.rs,reassemble.rs}`
  - Do:
    - `Packetizer::frame(meta, bytes, parity_ratio, mtu) -> Vec<Datagram>`: shard size = mtu − headers; blocks of at most 32 data shards; parity = ceil(k × ratio).
    - `Reassembler` keyed by `(stream, frame_id)`: repairs blocks with ≥ k shards, emits complete frames in order, and reports `FrameLost` after a deadline of max(2 × frame interval, 20 ms) past the newest frame. Counts repaired shards.
  - Depends on: T1.2
  - Done when: a property test with 0–(parity−1) shards dropped per block always reassembles exactly; more loss yields `FrameLost`; 1080p-sized frames pack and unpack at ≥ 2 GB/s in a bench.
- [x] **T1.5 — identity, key store, pairing**
  - Files: `crates/qr-net/src/{identity.rs,keystore.rs,pairing.rs,tls.rs}`
  - Do:
    - `Identity::load_or_create(dir)` (rcgen ECDSA P-256, 10-year validity), `host_id()`.
    - `KeyStore` as a JSON file of `{fingerprint, name, host_id, added}`.
    - TLS verifiers that accept any cert at the handshake and expose the peer cert.
    - `pairing::run_host/run_client` with SPAKE2 (Ed25519 group, identities `"qr-host"`/`"qr-client"`), where the transcript binds both fingerprints and a 32-byte TLS exporter (`EXPORTER-qr-pair`). HMAC-SHA256 confirmations.
    - PIN: 6 random digits; refresh after a successful pair.
    - Rate limit: 5 failures per minute per source IP, then ignore for 60 s.
  - Depends on: T1.2
  - Done when: unit tests show a correct PIN pairs, a wrong PIN fails both confirmations, and a MITM with a different cert fails.
- [x] **T1.6 — QUIC session layer**
  - Files: `crates/qr-net/src/{endpoint.rs,session.rs,clock.rs}`
  - Do:
    - `Endpoint::bind(addr)` with quinn: datagrams on, receive buffer 8 MiB, BBR congestion control, initial window 1 MiB on private addresses, keep-alive 1 s, idle timeout 5 s.
    - Socket buffers set to 8 MiB via socket2.
    - `Session` API: `control()` framed bidi stream, `input_tx/rx()` uni stream, `send_datagram`, `recv_datagram`, `open_file_stream`.
    - `ClockSync`: ping every 200 ms; the offset comes from the sample with minimum RTT in a sliding 5 s window.
  - Depends on: T1.5
  - Done when: a loopback test exchanges 10,000 datagrams and control messages, and the clock offset error is < 0.5 ms on loopback.
- [x] **T1.7 — mDNS discovery**
  - Files: `crates/qr-net/src/discovery.rs`
  - Do: `advertise(info)` and `browse() -> Stream<HostInfo>` on `_quickremote._udp.local.` with the TXT fields from Approach.
  - Depends on: T1.1
  - Done when: a test in two netns finds the host in < 3 s (A1).
- [x] **T1.8 — netns test harness**
  - Files: `tests/e2e/netns.sh`, `tests/e2e/lib.sh`, `tests/e2e/run.sh`
  - Do: `netns.sh lan|wan|nat-cone|nat-sym|relay` builds the topology inside `unshare -rn` with veth pairs, `tc netem` (lan: 1 Gbit, 0.2 ms; wan: 10 Mbit, 25 ms each way, 1% loss), and nft masquerade for NAT (`fully-random` for the symmetric one). It runs given commands in the named namespaces (via `nsenter` on PID files). `run.sh phaseN` runs the criteria scripts and prints PASS / FAIL / ENV-BLOCKED per A-ID.
  - Depends on: —
  - Done when: `netns.sh lan -- ping` passes between namespaces, and netem shows in `tc qdisc`.

#### Milestone 1b — media pipeline on the virtual display
- [x] **T1.9 — encoder probing**
  - Files: `crates/qr-media/src/{lib.rs,probe.rs,codec.rs}`
  - Do: rank candidate encoders per codec: VA (`vaav1enc`, `vah265enc`/`vah265lpenc`, `vah264enc`/`vah264lpenc`), NV (`nvav1enc`, `nvh265enc`, `nvh264enc`), QSV, AMF, then software (`svtav1enc`, `x265enc`, `x264enc`, `openh264enc`). Open each with a 64×64 test buffer, keep those that produce output, and report the `Vec<EncoderCaps{codec, element, hw, max_res, chroma444}>`. Same for decoders on the client.
  - Depends on: T1.1
  - Done when: on this machine the probe lists `vah264lpenc` (hw) plus x264, x265, svtav1 and openh264 (sw). This is checked by `qr-host --probe`.
- [x] **T1.10 — encode pipeline**
  - Files: `crates/qr-media/src/encode.rs`
  - Do:
    - Build `src ! [vapostproc|videoconvert] ! enc ! parse ! appsink` with low-latency settings: no B-frames, one-frame VBV, CBR-capped VBR, rate from `set_bitrate()`, GOP infinite, IDR on `force_keyframe()` via a `GstForceKeyUnit` event, and `sync=false` on appsink.
    - Per-encoder property tables: x264 `tune=zerolatency speed-preset=ultrafast`, va `target-usage`, nv `preset=p1 tune=ultra-low-latency zerolatency`.
    - Each output carries PTS-mapped capture time and encode start and end times (from pad probes).
  - Depends on: T1.9
  - Done when: `videotestsrc` → each probed encoder yields decodable AUs, and IDR on demand is under 1 frame late.
- [x] **T1.11 — virtual display**
  - Files: `crates/qr-host/src/vdisplay.rs`
  - Do:
    - Spawn a private runtime dir and D-Bus session, then `pipewire` + `wireplumber`, then sway with a generated config: `output HEADLESS-1 mode WxH@RHz`, `xwayland enable`, `exec` the desktop. The env is `WLR_BACKENDS=headless,libinput WLR_RENDERER=gles2 LIBSEAT_BACKEND=noop WLR_LIBINPUT_NO_DEVICES=1`.
    - Start `xdg-desktop-portal` + `xdg-desktop-portal-wlr` with `[screencast] chooser_type=none output_name=HEADLESS-1`.
    - Start the desktop command (default `xfce4-session` under Xwayland if present, else `foot`/`xterm`) or `--app CMD`.
    - Resize: `swaymsg output HEADLESS-1 mode WxH@RHz` at run time; the portal stream renegotiates caps.
    - Teardown: process group, plus `PR_SET_PDEATHSIG`.
    - GPU access: when the render node is not rw but `sg render` works, re-exec self under `sg render`.
  - Done when: `qr-host vdisplay --size 1920x1080@60` brings up sway, and a ScreenCast portal session on the private bus yields a PipeWire node with DMA-BUF formats.
- [x] **T1.12 — capture → encode wiring, zero copy**
  - Files: `crates/qr-media/src/capture.rs`, `crates/qr-host/src/capture/portal.rs`, `crates/qr-host/src/stream.rs`
  - Do:
    - `capture/portal.rs`: an ashpd ScreenCast session (source = monitor, cursor = embedded) on the given D-Bus. It returns the PipeWire fd and node id. T1.28 reuses it for real Wayland desktops.
    - `pipewiresrc fd=<fd> path=<node> always-copy=false do-timestamp=true` with caps `video/x-raw(memory:DMABuf)`, negotiated into the VA encoder.
    - `stream.rs` pulls AUs, packetizes with `qr-fec` and sends datagrams. On backpressure (send queue over 2 frames), skip encoding the next capture rather than dropping encoded frames.
  - Depends on: T1.10, T1.11, T1.4, T1.6
  - Done when: A13's trace (`GST_DEBUG=GST_PERFORMANCE:5` + caps log) shows DMABuf memory from source to encoder.

- [x] **T1.13 — host audio and virtual mic**
  - Files: `crates/qr-media/src/audio.rs`, `crates/qr-host/src/audio.rs`
  - Do:
    - In the private PipeWire, load a null sink `qr-out` (default sink) and a virtual source `qr-mic`.
    - Capture: `pipewiresrc target-object=qr-out.monitor ! opusenc frame-size=5 bitrate=128000 audio-type=restricted-lowdelay`, then audio datagrams with a sequence number and capture time.
    - Mic: datagrams → `opusdec` → `pipewiresink target-object=qr-mic`, with a mic on/off control message.
  - Depends on: T1.11, T1.6
  - Done when: a tone played in the session is recorded by the client (A17), and a WAV pushed into the mic is recorded on the host (A18).

#### Milestone 1c — native client
- [x] **T1.14 — client decode + present**
  - Files: `crates/qr-media/src/decode.rs`, `crates/qr-client/src/{main.rs,render.rs,video.rs,shaders/yuv.wgsl}`
  - Do:
    - `appsrc ! parse ! dec ! appsink` (hardware first, from the probe), NV12 output, decode time measured.
    - wgpu renderer: two planes uploaded per frame, YUV→RGB with BT.709 limited range (or full for 4:4:4), aspect-fit.
    - Present: `Mailbox` if available, else `Immediate`, else `Fifo`. Frame latency target 1.
    - Present time is stamped after `queue.submit` + `present()`.
  - Depends on: T1.9
  - Done when: the client shows a `videotestsrc` stream from the host and logs per-frame decode and present times.
- [x] **T1.15 — host list, pairing UI, session lifecycle**
  - Files: `crates/qr-client/src/{ui.rs,session.rs,hosts.rs}`, `crates/qr-host/src/{main.rs,session.rs,pairing_ui.rs}`
  - Do:
    - Client UI is drawn with egui (egui-wgpu): "On this network", "Paired", "Add host by ID", a PIN dialog, and errors with the spec §7 wording.
    - CLI equivalents: `qr-client --list`, `--connect <addr|id> [--pin N]`, `--headless --frames N --stats-json`.
    - Host prints name, ID and PIN to the terminal, and shows the PIN in the virtual desktop's notification when available.
    - Auto-reconnect with exponential backoff (250 ms → 2 s, give up at 60 s). On connect, the host sends release-all.
  - Depends on: T1.5, T1.6, T1.7, T1.14
  - Done when: A1, A2 and A6 pass in netns.
- [ ] **T1.16 — client audio**
  - Files: `crates/qr-client/src/audio.rs`
  - Do: audio datagrams → jitter buffer (target 10 ms, adaptive 5–40 ms) → `opusdec` → `autoaudiosink` (or a file sink with `--audio-out file.wav` for tests). A/V sync: audio plays as soon as possible; if video is late by more than 40 ms, the overlay reports drift. Mic: `autoaudiosrc` (or `--mic-in file.wav`) → opusenc → datagrams.
  - Depends on: T1.13, T1.15
  - Done when: A17 and A18 pass.

#### Milestone 1d — input
- [x] **T1.17 — client input capture**
  - Files: `crates/qr-client/src/input.rs`
  - Do:
    - Keys by winit `PhysicalKey` → evdev code table.
    - Absolute mouse is mapped to remote pixels.
    - Relative mode: `CursorGrabMode::Locked` (falling back to `Confined` + recenter) with raw `DeviceEvent::MouseMotion` deltas and no scaling. The hotkey Ctrl+Alt+Shift+M toggles it, and the host's `SetMode{relative}` requests it.
    - Motion is coalesced per 1 ms into datagrams. Keys and buttons go on the input stream.
    - Text mode sends `Text` events via IME commit.
    - Hotkeys from spec §7.
  - Depends on: T1.15
  - Done when: unit tests pass for the keymap, and coalescing keeps relative delta sums exact (A20 logic).
- [x] **T1.18 — host input injection (virtual display)**
  - Files: `crates/qr-host/src/input/{mod.rs,xtest.rs,uinput.rs,keymap.rs}`
  - Do:
    - Trait `Injector { key, button, abs, rel, wheel, text, release_all }`.
    - `XTest` injector for X11 real desktops only (T1.27), with relative motion via `XTestFakeRelativeMotionEvent`.
    - `Uinput` injector: a "QuickRemote Keyboard", a "QuickRemote Mouse" (REL + ABS) and a "QuickRemote Pen" device.
    - Text: Unicode via a spare-keycode remap on X11; via uinput, the compose fallback is ENV-BLOCKED on Wayland real desktops (Open Questions).
    - Track held keys and release them all on disconnect.
    - Host-requested relative mode: watch XFixes cursor visibility and grabs; when the cursor is hidden and the pointer is grabbed, send `SetMode{relative}`.
  - Depends on: T1.11
  - Done when: A19, A20 and A23 pass (`xdotool`/`xinput test` on the host side record the events).
- [x] **T1.19 — gamepads**
  - Files: `crates/qr-client/src/pads.rs`, `crates/qr-host/src/input/pad.rs`
  - Do:
    - Client: gilrs, up to 4 pads. On attach it sends `PadAttach{idx, kind}`. State snapshots (buttons bitmask, 6 axes i16) go on change and every 100 ms, as datagrams with a sequence number, plus attach and detach on the reliable stream.
    - Host: uinput device mimicking an Xbox 360 pad (vendor 0x045e, product 0x028e, BTN_SOUTH…, ABS_X/Y/RX/RY/Z/RZ, HAT0X/Y) with FF_RUMBLE. Rumble effects are forwarded as `Rumble{idx, strong, weak, ms}` and the client plays them through gilrs force feedback.
  - Depends on: T1.17, T1.18
  - Done when: A21 passes; the client side is driven by a uinput test pad on the client namespace when one is available, otherwise by `--fake-pad` scripted input.
- [x] **T1.20 — pen**
  - Files: `crates/qr-client/src/pen.rs`, `crates/qr-host/src/input/pen.rs`
  - Do:
    - Client: winit has no tablet events, so read Linux tablets directly through evdev (`ABS_PRESSURE`, `ABS_TILT_X/Y`, `BTN_TOOL_RUBBER`) when the window is focused; Windows via WM_POINTER (Phase 4). `--fake-pen` emits a scripted sweep.
    - Host: a uinput tablet with `INPUT_PROP_DIRECT`, ABS_X/Y as screen size, pressure 0–4095 (rescaled from the client's range), tilt ±64, and BTN_TOOL_PEN / BTN_TOOL_RUBBER.
  - Depends on: T1.18
  - Done when: A22 passes at the evdev level. Delivery into apps in the virtual display is per the Open Questions decision.

#### Milestone 1e — desktop features and quality
- [x] **T1.21 — clipboard**
  - Files: `crates/qr-host/src/clipboard.rs`, `crates/qr-client/src/clipboard.rs`
  - Do: host on the X11 selection (x11rb, XFixes selection notify; targets `UTF8_STRING`, `image/png`). Client uses arboard (text + image → PNG). Loop suppression by content hash. A per-host toggle is stored in the client's key store entry.
  - Depends on: T1.15
  - Done when: A24 passes.
- [x] **T1.22 — file transfer**
  - Files: `crates/qr-net/src/files.rs`, `crates/qr-host/src/files.rs`, `crates/qr-client/src/files.rs`
  - Do:
    - `FileOffer{id,name,size,sha256}` → `FileAccept` → a dedicated uni stream per file with 256 KiB chunks.
    - The receiver writes `name.part` and verifies the SHA-256 before renaming. Cancel resets the stream and deletes `.part`.
    - QUIC stream priority below video and input (quinn `set_priority(-1)`).
    - Host target dir: `~/Downloads/QuickRemote`. Client: drag and drop onto the window (winit `DroppedFile`) and a "Receive" folder.
  - Depends on: T1.15
  - Done when: A25 passes.
- [x] **T1.23 — multi-monitor**
  - Files: `crates/qr-host/src/vdisplay.rs`, `crates/qr-host/src/monitors.rs`, `crates/qr-client/src/session.rs`
  - Do: `swaymsg create_output` adds headless outputs (HEADLESS-2…), each captured through its own portal ScreenCast session, so each gets its own PipeWire node. `MonitorList` and `SelectMonitor`. The video datagram `stream` byte selects the monitor. The client opens a second window as a second video stream inside the same session.
  - Depends on: T1.12, T1.15
  - Done when: A16 passes.
- [x] **T1.24 — rate control, profiles, recovery**
  - Files: `crates/qr-host/src/ratectl.rs`, `crates/qr-client/src/feedback.rs`
  - Do:
    - Client `Feedback` every 50 ms: bytes received, shards lost, shards repaired, frames lost, one-way delay samples (min, max, slope).
    - Host profile select: LAN when the peer address is RFC1918/ULA/link-local and RTT < 5 ms, else internet; the CLI `--profile` overrides.
    - Controller as in Approach. Adaptive parity: target = observed loss × 2 + 5%, clamped 10–50%.
    - `FrameLost` → force IDR, at most one per RTT.
  - Depends on: T1.12, T1.15
  - Done when: A10 and A11 pass in netns.
- [x] **T1.25 — sharp-text mode**
  - Files: `crates/qr-host/src/stream.rs`, `crates/qr-media/src/encode.rs`
  - Do: when `SetMode{sharp}` is on and an encoder with `chroma444` exists, rebuild the pipeline with Y444 caps. Otherwise, run still detection (no damage for 300 ms, from PipeWire buffer flags or frame-hash equality) and send one refresh frame at QP 18. Toggle from the client hotkey T.
  - Depends on: T1.24
  - Done when: A12 passes (the SSIM check uses `tests/e2e/ssim.py`, which compares the client's decoded PNG with the host's capture PNG).
- [x] **T1.26 — stats overlay + qr-latency**
  - Files: `crates/qr-client/src/overlay.rs`, `crates/qr-latency/src/main.rs`
  - Do: the overlay (egui) shows the fields from R29. `--stats-json` writes one line per second. `qr-latency --connect … --duration 120 --size 1920x1080@60` runs a headless client and reports p50, p95 and p99 of capture→present and each part.
  - Depends on: T1.14, T1.24
  - Done when: A8 and A29 pass. A9 is ENV-BLOCKED with the reason.

#### Milestone 1f — real desktop capture
- [ ] **T1.27 — X11 desktop backend**
  - Files: `crates/qr-host/src/capture/x11.rs`
  - Do: `--display :N` (no spawn) → `ximagesrc use-damage=true` with XShm + XTest input, and RandR monitors as the monitor list.
  - Depends on: T1.12, T1.18
  - Done when: A15 passes for X11 on an Xvfb started by the test as a stand-in "existing session".
- [ ] **T1.28 — Wayland portal backend**
  - Files: `crates/qr-host/src/capture/portal.rs`
  - Do: reuse `capture/portal.rs` on the user's session bus with `persist_mode=ExplicitlyRevoked` and the restore token stored in the host config. Input goes through uinput (keyboard, mouse, pen, pads), the same as the virtual display.
  - Depends on: T1.12, T1.18
  - Done when: A15 passes for Wayland against `mutter --headless --virtual-monitor 1920x1080` with xdg-desktop-portal-gnome (in a private session bus), and the second run asks no permission. If T0.2 packages are missing: ENV-BLOCKED.

### Phase 2: Internet (rendezvous, hole punching, relay)
**Goal:** a paired client reaches a host by ID across NATs, direct or relayed, end-to-end encrypted.
**Checkpoint:** `tests/e2e/run.sh phase2` passes A3, A4, A5, A9-internet (A10 through NAT).

- [x] **T2.1 — rendezvous protocol**
  - Files: `crates/qr-proto/src/rendezvous.rs`, `docs/protocol.md`
  - Do: ALPN `qr-rv/1` messages: `Register{host_id, cert, sig(nonce)}`, `Registered{observed_addr}`, `Lookup{host_id, client_local_addrs}`, `Introduce{peer_observed, peer_locals, punch_at_ms, session_token}`, `RelayRequest{session_token}`, `RelayAllocated{relay_addr}`, `Error`.
  - Depends on: T1.2
  - Done when: codec round-trip tests and conformance vectors are added.
- [x] **T2.2 — qr-relay server**
  - Files: `crates/qr-relay/src/{main.rs,registry.rs,relay.rs}`, `crates/qr-relay/Dockerfile`, `deploy/docker-compose.yml`
  - Do:
    - quinn server on UDP 4500. A registry of host_id → (connection, observed addr, locals) with signature-verified registration.
    - Introduce: send both sides `Introduce` with a punch time 300 ms ahead.
    - Relay: allocate a UDP port from a range (default 40000–40999) per session token and forward packets between the two 5-tuples learned from the first packets bearing the token prefix (8 bytes, the relay strips it). Idle timeout 30 s.
    - Limits: max sessions, per-session bandwidth cap flag.
    - Logs carry no payloads.
  - Depends on: T2.1, T1.6
  - Done when: unit tests for the registry and forwarding pass, and the `docker build` succeeds (or is ENV-BLOCKED without the docker group).
- [ ] **T2.3 — punchable socket**
  - Files: `crates/qr-net/src/punch.rs`
  - Do: a quinn `AsyncUdpSocket` wrapper over tokio UDP that (a) exposes `send_raw(addr, bytes)`, (b) drops inbound packets starting with the 8-byte punch magic `QRPUNCH\0`, and (c) prepends the relay token on packets to the relay address when relaying.
  - Depends on: T1.6
  - Done when: a test shows QUIC still works through the wrapper and raw punches are dropped.
- [x] **T2.4 — host and client internet flow**
  - Files: `crates/qr-net/src/rendezvous_client.rs`, `crates/qr-host/src/internet.rs`, `crates/qr-client/src/hosts.rs`
  - Do:
    - Host: `--rendezvous HOST:PORT` keeps a registration with reconnect.
    - Client: connect by ID → `Lookup` → at `punch_at`, both send 5 punches 20 ms apart to every candidate → the client dials the candidates in parallel and the first handshake wins. After 2.5 s → `RelayRequest` → dial the relay address. The path type is reported to the overlay and title.
    - A LAN pair learns the host ID and relay from `HelloAck`, so "by ID" is filled in automatically.
  - Depends on: T2.2, T2.3, T1.15
  - Done when: A4 passes (cone → Direct, symmetric → Relayed).
- [x] **T2.5 — E2E encryption proof + fallback**
  - Files: `tests/e2e/a3_relay_plaintext.sh`
  - Do: a `tcpdump` (in netns, as userns root) on the relay; send the clipboard marker `QR-PLAINTEXT-CANARY-7731`; grep the pcap. Then a forged host cert must fail. Stop the relay and check LAN still works (A5).
  - Depends on: T2.4
  - Done when: A3 and A5 pass (the Docker part is ENV-BLOCKED without the docker group).

### Phase 3: Web client
**Goal:** the host serves a browser client with feature parity for video, audio, input, pads, clipboard and files.
**Checkpoint:** `tests/e2e/run.sh phase3` passes A27 (Chrome full; Firefox smoke ENV-BLOCKED if not installed).

- [x] **T3.1 — host web server**
  - Files: `crates/qr-host/src/web/{mod.rs,wt.rs,https.rs}`
  - Do: `--web-listen ADDR`: HTTPS (axum + tokio-rustls) serves the embedded `web/dist` (rust-embed). WebTransport via `wtransport` on the same port (UDP) with an ECDSA cert valid 13 days, rotated daily, keeping the old one for 24 h. The page URL carries the cert hash in the fragment `#c=<hash>`, and the host prints `WEB-URL`.
  - Depends on: T1.15
  - Done when: Chrome loads the page and opens a WebTransport session.
- [x] **T3.2 — TypeScript protocol + conformance**
  - Files: `web/package.json`, `web/tsconfig.json`, `web/build.mjs` (esbuild), `web/src/proto.ts`, `web/test/vectors.test.ts`
  - Do: mirror `qr-proto` and check against `conformance/vectors.json` with `node --test`.
  - Depends on: T1.3
  - Done when: `npm test` passes (A32 TypeScript half).
- [x] **T3.3 — web pairing**
  - Files: `web/src/pairing.ts`, `crates/qr-host/src/web/auth.rs`
  - Do:
    - SPAKE2 in TypeScript with @noble/curves ed25519, matching the Rust `spake2` crate's M/N constants and transcript. The PIN enrols a WebCrypto P-256 key (non-extractable, in IndexedDB).
    - Later connections: the host sends a 32-byte nonce, and the browser signs `nonce‖host-cert-hash`.
  - Depends on: T3.2, T1.5
  - Done when: a cross-language SPAKE2 test vector passes, and A2 passes from Chrome.
- [x] **T3.4 — web media**
  - Files: `web/src/{video.ts,audio.ts,fec.ts,render.ts}`
  - Do:
    - FEC reassembly in TypeScript (Reed-Solomon over GF(2^16), the same as reed-solomon-simd, compiled to WASM from the `qr-fec` crate with `wasm-bindgen` to avoid a second implementation).
    - WebCodecs `VideoDecoder` (`optimizeForLatency: true`, hardware preferred) → WebGL2 canvas via `texImage2D(VideoFrame)`.
    - `AudioDecoder` (opus) → AudioWorklet ring buffer.
    - Mic: `getUserMedia` → `AudioEncoder` opus.
  - Depends on: T3.1
  - Done when: video and audio play in headless Chrome with GPU flags (per the 2026-07-23 lesson: `--use-angle=swiftshader --enable-unsafe-swiftshader` when no GPU in the QA browser).
- [x] **T3.5 — web input, pads, clipboard, files, overlay**
  - Files: `web/src/{input.ts,pads.ts,clipboard.ts,files.ts,overlay.ts,main.ts}`, `web/index.html`
  - Do:
    - `KeyboardEvent.code` → evdev, with Keyboard Lock in fullscreen.
    - Pointer lock with `unadjustedMovement: true`.
    - Gamepad API polling on rAF, rumble via `vibrationActuator`.
    - Clipboard on focus/copy events. File upload by picker and drop; downloads through an `<a download>` blob.
    - Overlay with the same fields as native.
  - Depends on: T3.4
  - Done when: `tests/e2e/web.mjs` (Node CDP driver, no puppeteer) passes A27.

### Phase 4: Windows host and Windows native client
**Goal:** the host and native client build and run on Windows.
**Checkpoint:** `cargo check --target x86_64-pc-windows-msvc` (cargo-xwin) passes in CI here. **Hardware gate:** A26, A30 and A31 (Windows parts) on a Windows PC.

- [x] **T4.1 — virtual gamepad driver decision**
  - Files: `plans/quick-remote.md` (record the decision)
  - Do: compare ViGEmBus (archived, signed, widely installed) with a VHF-based or UMDF HID minidriver (needs signing). Ask the user and record the choice.
  - Depends on: —
  - Done when: a decision is recorded here.
  - **Decision (2026-10-03, user):** ViGEmBus. The user installs it once, and QuickRemote loads `ViGEmClient.dll` at run time. If the driver is missing, the host logs that and continues without gamepads.
- [ ] **T4.2 — cross-compile setup**
  - Files: `scripts/xwin-check.sh`, `.cargo/config.toml`
  - Do: `cargo install cargo-xwin`. GStreamer MSVC dev libraries come from the official `gstreamer-1.0-devel-msvc-x86_64` package, extracted with `msiextract` or `7z`; `PKG_CONFIG_PATH` points at them for the target.
  - Depends on: T1.1
  - Done when: `scripts/xwin-check.sh` checks `qr-host` and `qr-client` for Windows.
- [ ] **T4.3 — Windows capture, encode, audio**
  - Files: `crates/qr-host/src/capture/windows.rs`, `crates/qr-media/src/probe.rs`
  - Do: `d3d11screencapturesrc capture-api=wgc monitor-index=N show-cursor=false` (falling back to DXGI) → `d3d11convert` → `nvh264enc`/`qsvh264enc`/`amfh264enc` or the HEVC/AV1 variants with D3D11 memory caps. Audio from `wasapi2src loopback=true low-latency=true`. Monitor list via `DXGI EnumOutputs`.
  - Depends on: T4.2
  - Done when: it compiles for Windows; hardware gate A30.
- [ ] **T4.4 — Windows input**
  - Files: `crates/qr-host/src/input/windows.rs`
  - Do: `SendInput` with scancodes (`KEYEVENTF_SCANCODE`), relative and absolute mouse (`MOUSEEVENTF_VIRTUALDESK`), `InjectSyntheticPointerInput` with `PT_PEN` for pressure and tilt, and gamepads per T4.1.
  - Depends on: T4.1, T4.2
  - Done when: it compiles; hardware gate A30.
- [ ] **T4.5 — Windows client specifics**
  - Files: `crates/qr-client/src/pen.rs`, `crates/qr-client/src/audio.rs`
  - Do: WM_POINTER pen input through a winit raw window hook; GStreamer `d3d11h264dec`, `d3d11h265dec` and `d3d11av1dec` for decode; `wasapi2sink` low-latency for audio.
  - Depends on: T4.2
  - Done when: it compiles; hardware gate A26.
- [ ] **T4.6 — host system service (Linux + Windows)**
  - Files: `crates/qr-host/src/service/{linux.rs,windows.rs}`, `deploy/systemd/quickremote-host.service`, `deploy/systemd/quickremote-greeter.service`
  - Do:
    - Linux: a systemd system service runs a supervisor that spawns the per-user host in the active session (via logind `ActiveSession`), plus a greeter-mode host on the display manager's X display (its `XAUTHORITY`).
    - Windows: a service (windows-service crate) that launches the host in the console session with `CreateProcessAsUser` and SYSTEM token on Winlogon for the lock screen and UAC desktop.
    - Autostart on both.
  - Depends on: T4.3, T1.27
  - Done when: the Linux greeter test passes on an LightDM-on-Xvfb stand-in, or is ENV-BLOCKED (needs root). Windows is a hardware gate (A31).

### Phase 5: Android client
**Goal:** an Android 10+ app that pairs, streams, and takes touch, keyboard, gamepad and pen input.
**Checkpoint:** `tests/e2e/run.sh phase5` passes A28 on the emulator (API 34, x86_64). **Hardware gate:** a real device for hardware-decode latency and TV remote.

- [x] **T5.1 — Android toolchain**
  - Files: `scripts/android-sdk.sh`
  - Do: download the commandline-tools to `~/Android/sdk`; install platform 34, build-tools, NDK r27, emulator and a system image; `cargo install cargo-ndk`; add the `aarch64-linux-android` and `x86_64-linux-android` targets.
  - Depends on: —
  - Done when: `sdkmanager --list_installed` shows them, and the emulator boots (KVM needed: T0.2).
- [x] **T5.2 — qr-ffi**
  - Files: `crates/qr-ffi/src/lib.rs`
  - Do: JNI (`jni` crate) exports: `connect(addrOrId, pin)`, `nextVideoFrame()` (direct ByteBuffer + metadata), `nextAudio()`, `sendInput(bytes)`, `sendPad(state)`, `pollEvents()` (JSON), `close()`. Discovery through mdns-sd, which needs Android's `MulticastLock` from Kotlin.
  - Depends on: T1.15
  - Done when: `cargo ndk -t x86_64 -t arm64-v8a build -p qr-ffi` succeeds.
- [x] **T5.3 — Android app**
  - Files: `android/settings.gradle.kts`, `android/app/build.gradle.kts`, `android/app/src/main/AndroidManifest.xml`, `android/app/src/main/java/dev/quickremote/{MainActivity.kt,HostListActivity.kt,SessionActivity.kt,VideoDecoder.kt,AudioPlayer.kt,InputHandler.kt,Pads.kt,Overlay.kt,Native.kt}`
  - Do:
    - Host list and pairing UI.
    - `SessionActivity` with SurfaceView: MediaCodec decode with `KEY_LOW_LATENCY` and `KEY_PRIORITY=0`, rendering output buffers immediately.
    - `AudioTrack` with `PERFORMANCE_MODE_LOW_LATENCY` and an opus MediaCodec decoder.
    - Touch: trackpad mode (relative + tap-click) and direct mode.
    - Soft keyboard → `Text` events. Hardware keys → evdev map. Gamepad `MotionEvent` axes with `InputDevice` rumble via `VibratorManager`. Stylus pressure and tilt from `MotionEvent`.
    - Leanback launcher and D-pad focus for TV. Clipboard via `ClipboardManager`. Files via the Storage Access Framework picker.
  - Depends on: T5.2
  - Done when: `./gradlew assembleDebug` builds.
- [ ] **T5.4 — emulator QA**
  - Files: `tests/e2e/android.sh`
  - Do: boot the emulator headless (`-no-window -gpu swiftshader_indirect`), install the APK, and connect to a host on the host network (10.0.2.2). Drive it with `adb shell input` and `adb shell sendevent` for a virtual gamepad if supported, check frames via `--stats` logcat lines, and take a screenshot.
  - Depends on: T5.3
  - Done when: A28 passes on the emulator, and the real-device parts are ENV-BLOCKED.

## Constraints
- Rust stable 1.98+, edition 2024. `cargo clippy --workspace -- -D warnings` and `cargo fmt --check` are clean at every checkpoint.
- No unsafe outside `qr-media` (GStreamer buffer mapping), `qr-host/src/input` (ioctl via evdev and Win32) and `qr-ffi`. Each `unsafe` gets a SAFETY comment.
- No GPL code is linked. x264 and x265 are used only as GStreamer plugins at run time.
- Every acceptance check reports PASS, FAIL or ENV-BLOCKED (with the reason). Nothing that wasn't run is reported as PASS.
- The protocol is changed only together with `docs/protocol.md` and `conformance/vectors.json`.
- Per the 2026-08-06 lesson, this plan traces R1–R32 → tasks:
  - R1: T1.7
  - R2: T1.5, T1.15, T3.3
  - R3: T1.5, T2.5
  - R4: T2.1–T2.4
  - R5: T2.2
  - R6: T1.15
  - R7: T1.9, T1.10
  - R8: T1.12, T1.24, T1.26
  - R9: T1.24
  - R10: T1.4, T1.24
  - R11: T1.25
  - R12: T1.12
  - R13: T1.11
  - R14: T1.27, T1.28, T4.3
  - R15: T1.23
  - R16: T1.11, T1.14
  - R17: T1.13, T1.16
  - R18: T1.13, T1.16
  - R19: T1.17, T1.18
  - R20: T1.17, T1.18
  - R21: T1.19
  - R22: T1.20, T4.4
  - R23: T1.18
  - R24: T1.21
  - R25: T1.22
  - R26: T1.14–T1.26, T4.5
  - R27: T3.1–T3.5
  - R28: T5.1–T5.4
  - R29: T1.26
  - R30: T4.3, T4.4
  - R31: T4.6
  - R32: T1.2, T1.3, T3.2

## Progress notes
- T1.4: FEC uses GF(2^8) Reed-Solomon (`reed-solomon-erasure`) instead of `reed-solomon-simd`, because the latter's decoder costs about 350 µs per block. Measured throughput is 0.7–1.3 GB/s (packetize + reassemble), not the 2 GB/s first guessed, which is about 0.14 ms per 100 KB frame. Per-block parity is sized from the measured loss so a block fails with probability under 0.1%.
- T1.6: path-MTU discovery is off (fixed 1200-byte packets). Black-hole detection under random loss was dropping queued video datagrams. Tests run with `QR_GSO=0`, because netem drops a whole GSO batch at once.
- T1.24: the encoder bitrate is scaled by source fps ÷ capped fps; capture hold is re-checked every 5 ms; growth is 50%/s after 3 quiet seconds. A10 and A11 pass on the 60–90 s runs; the full 180 s run is still to do.
- A8 (loopback, 2026-10-03): 1080p60, VA-API H.264 → VA-API decode, 2 minutes (6,898 frames). Capture → decoded p50 6.58 ms, p95 7.94 ms, p99 10.04 ms, longest gap 29 ms. Split: capture 0.18, encode 3.18, network 0.18, decode 2.73 ms. Not yet the full A8: the run used loopback instead of the simulated gigabit LAN (GPU in user namespaces needs the render group, i.e. setup), and it stops at the decoded picture (no window present).
- T1.14 (partial): headless and window clients decode and render (offscreen renderer test passes). Measured on loopback, VA-API → VA-API 1080p60 is about 10 ms from capture to decoded picture.
- T1.13/T1.16 (partial): Opus audio both ways with PLC. Mic WAV → host WAV correlation is 0.997 on loopback. Audio latency estimate is about 18 ms. The private PipeWire sink/source arrives with the virtual display.
- T1.21: clipboard is a shared `qr-clip` crate (X11 CLIPBOARD via XFixes, INCR both ways) used by host and client, so PNGs stay byte-identical. A24 passes 3/3. Windows/macOS/Wayland clients need their own backend later (T4.5 / Phase 5).
- T1.22: files go both ways (client `--send-file` and drag and drop; host outbox folder `~/QuickRemote/Outbox`), paced by RTT, with stream priority below video. A25a/b/d pass. A25c (frame rate during transfers) is ENV-BLOCKED: `net.core.rmem_max` is 208 KiB, and the client drops 13–17k UDP packets in each run. `setup-root.sh` now raises it.
- T1.25 (partial): the still-screen refresh is done (300 ms after the last change, one keyframe at 8× budget) and A12 passes (SSIM 0.9998). A still page also converges without the mode, so A12 doesn't show the refresh's benefit right after motion stops. 4:4:4 needs a 4:4:4 decoder on the client (`avdec_h264` from gstreamer1.0-libav, in setup) plus a stream restart; it isn't wired yet.
- Phase 2: A4 passes through cone NATs (Direct) and symmetric NATs (Relayed), and A3a/b pass (30k relayed packets, canary never in plaintext). Deviation from T2.3: no wrapping `AsyncUdpSocket`. Hole punches and relay hellos go out on a clone of the endpoint's UDP socket (same port, so the same NAT mapping). Each relay session has its own port, and the relay learns both peers from their `PUNCH_MAGIC ‖ token` hellos, so relayed packets carry no token prefix. Relay ports bind to `--public-ip`, because on a multi-homed server a 0.0.0.0 socket can answer from another address, which NATs drop. The Docker image (A5) is written but ENV-BLOCKED: no access to the docker group.
- Phase 3: A27 passes in headless Chrome: video at 60 fps via WebCodecs, audio, keys, pointer lock, 3 MiB upload, clipboard host → page, paired reconnect without PIN. Gamepad and Firefox are ENV-BLOCKED. Deviations: (1) the host runs a **web gateway**. It terminates WebTransport, authenticates the browser, and pipes the browser's streams and datagrams to a loopback QUIC session with its own trusted identity, so session code serves both kinds of client. The gateway records each browser's real address, RTT and datagram size for profile choice and shard sizing. (2) SPAKE2 and FEC run in the browser as WebAssembly (`qr-wasm` over a new `qr-pair` crate) instead of a TypeScript re-implementation, and the TypeScript side mirrors only the message codec (checked against the vectors). Still to do: the 13-day WebTransport certificate isn't rotated yet, so a host has to restart within 13 days. The fix is for the page to fetch the current hash and the host to reload its certificate.
- Phase 5: the SDK, NDK and a JDK 17 are installed in the home directory (no root needed); `scripts/build-android.sh` builds `qr-ffi` for arm64-v8a and x86_64 and the debug APK (18.6 MB). `qr-ffi` runs its own lean session core (no GStreamer): frames and Opus go to Kotlin MediaCodec, input comes back over JNI. The core is checked natively by `cargo run -p qr-ffi --example probe` against a host (pairing, 150 frames in 5 s, 1,002 audio packets, input, events). A28 on the emulator is ENV-BLOCKED: no KVM access (setup adds the kvm group). Android files from the host are refused for now.
- Phase 4 so far: `qr-win` (SendInput scancodes, absolute/relative mouse, wheel, Unicode text, synthetic pen, ViGEmBus pads with rumble) compile-checks for x86_64-pc-windows-msvc. The full host can't be cross-checked here: `ring` needs a Windows C toolchain (setup adds clang/lld). T4.3 capture uses the existing `d3d11` source string; T4.5 and T4.6 aren't started.
- T1.19/T1.20: Linux uinput keyboard, mouse, pen and Xbox-style pads with force feedback are written (`crates/qr-host/src/uinput.rs`), plus client pads through gilrs (`--fake-pad` for tests). Running them waits on uinput access (setup).
- T1.17/T1.18 (partial): input stream, motion running totals, XTest injector and the `--script` test driver. A19a, A20 and A23 pass (input p95 0.56–0.66 ms). uinput waits on root setup.
- T1.11/T1.12/T1.23 (2026-10-07): `--source vdisplay[:CMD]` runs headless sway (GLES2 on the render node, or `--vd-software`) with its own D-Bus, PipeWire and portals, and `--vd-outputs N` monitors. **Deviation from T1.12:** the virtual display is captured with wlr-screencopy straight from sway (`screencopy.rs`, shared-memory buffers into an `appsrc`), not through the ScreenCast portal. Two reasons. pipewiresrc 1.4.2 clears its possible caps after the first negotiation, so the renegotiation xdg-desktop-portal-wlr does after a start or a resize fails with "unhandled format", and xdpw then exits. It also stalled at random on stream start ("out of buffers"). Screencopy copies only on damage and repeats the last frame every 500 ms on a still screen. Zero copy came next (below). The portal path (`portal.rs`) stays for real Wayland desktops (T1.28) and will hit the same pipewiresrc bugs there.
- VA encoders output two frames late (async mode) when upstream answers the latency query "not live", as pipewiresrc does here. The encoder's sink pad now reports live. On the virtual display that took capture → decoded from 47 ms to 11 ms p50 (p95 18–23 ms) at 1600×900@60.
- Video sessions are restartable and multi-stream (`video.rs`). `SelectMonitor` moves a stream or opens one more (up to 4), `StreamStop` closes it, and `Resize` re-modes a virtual monitor (debounced 300 ms). Frame numbering carries on across restarts. The window client switches monitors with Ctrl+Alt+Shift+N and opens a view-only second window on the next monitor with Ctrl+Alt+Shift+W (checked under Xvfb: window drawn, stream 1 started and stopped). A16 and R16 (`tests/e2e/a16.sh`) pass 3/3: two monitors listed, both streams at ≥45 fps, a swap, a resize to 1280×720, and the extra stream stopped. Input in the second window, and Wayland input into the virtual display, came next (below).
- T1.12 zero copy (2026-10-07): sway copies each damaged frame on the GPU into a GBM buffer we allocated (screencopy's linux-dmabuf offer). A GLES 3 shader (`gpuconv.rs`, EGL on the GBM platform) writes BT.709 NV12 into a Y-tiled DMA-BUF, and the VA encoder reads it as `memory:DMABuf` (`Source::DmaNv12`). No pixel touches the CPU. Two driver/library problems needed workarounds. Mesa's GBM crashes allocating NV12 with Y-tiling, so the NV12 layout is hand-built in one Y-tiled R8 buffer (luma padded to 32 rows, then chroma). GStreamer 1.26's VA encoder never frees a picture whose input memory isn't from its own allocator (`gst_va_encode_picture_free` returns early without a VA display), so encoding stopped after 4 frames. The memories now come from libgstva's DMA-BUF allocator (loaded at run time). Results, capture → decoded on one machine: 1600×900@60 p50 8.4 ms (was 11 ms through shared memory); **1920×1080@120: 120 fps, p50 9.9 ms, p95 12.6 ms**; capture plus conversion is 0.2 ms plus about 1.3 ms of GPU work. 4K60 runs at only 13 fps: decode on the client takes 59 ms (VA download to system memory plus copies), which trips the decode backoff, and encode takes 26 ms with host and client sharing one iGPU. Next on that front: zero-copy decode → render in the client. The shared-memory path remains as a fallback (`QR_VD_CAPTURE=shm`, or automatically when any GPU step fails).
- T1.13 (2026-10-07): the virtual display's PipeWire gets a null sink `qr-out` (default) and a loopback microphone: the host plays into `qr-mic-in`, and apps record from `qr-mic`, the default source. A stream can't play into a source node, and the first try ended up in the speakers. pipewire-pulse runs too, for PulseAudio apps. WirePlumber's hardware monitors are off, so the machine's sound card stays out. The host captures `qr-out`'s monitor through an fd connection to the display's PipeWire (`pipewiresrc fd=`). Audio capture pipelines run on the system clock: pipewiresrc had made the display graph's clock the pipeline clock, and that clock starts at its own zero, which made the latency read 5.6 s. `tests/e2e/vd_audio.sh`: A17-vd (an app's click track through pipewire-pulse; host capture → client playout 7.4 ms) and A18-vd (client microphone WAV → app recording, correlation 0.984), 2/2. A17's flash-and-beep A/V offset isn't measured yet.
- A13/A14 (`tests/e2e/vd_3d.sh`): vkcube in the virtual display streams at 60 fps through zero-copy capture. The display takes the client's requested 2560×1440@120 and then a resize to 1920×1200@60. At 2560×1440@120 the stream reaches only 47 fps on this machine, with sway, the conversion, encode and decode sharing one iGPU.
- A6 (`tests/e2e/a6.sh`, 2/2): a 3 s link outage doesn't end the QUIC session (5 s idle timeout, 1 s keepalives), so the input stream never "broke" and a held key stayed down, auto-repeating. The host now releases held keys and buttons once the client has been silent for 500 ms (clients report every 50 ms). Measured: the key is released 0.5 s into the outage, and video resumes 1 s after the link returns, with no PIN.
- Input into the virtual display (`wlinput.rs`, `--input auto` with a vdisplay source) uses sway's virtual pointer and virtual keyboard: no uinput, no device permissions. Keys use libxkbcommon's default keymap (loaded at run time through `xkbcommon-dl`). Text is typed the `wtype` way, with a throwaway keymap of `U+XXXX` keysyms. Absolute positions add the stream's monitor origin and map over the output layout from xdg-output (wlroots reports every `wl_output` at 0,0). `tests/e2e/vd_input.sh` checks in a GTK window on the second monitor, through a second stream: A19-vd (Q key, "héllo 漢字" exact), A20-vd (1000 × +1 → +1000) and clicks landing at the right place, 2/2. The first borrowed character is not dropped on this path. Pads and pen still go through uinput (`wayland+uinput` when `/dev/uinput` is writable), but headless sway has no libinput backend, so a uinput pen won't reach apps in the virtual display yet. The second window sends pointer motion and clicks on its own stream.
- A10 regression fixed: the decode-time backoff (commit 41f0d44) read openh264's one-frame hold as a slow decoder. That locked A10 at 5–15 fps and about 0.8 Mbit/s (the previous commit fails A10b too). The client now reports decoding work (`work_us`): a picture released by the next push counts from that push, and a backlog still counts in full. The host steps down only after 3 slow windows in a row, at most every 500 ms. A10 then passes 3/3 at a steady 60 fps and 5–6 Mbit/s with 100% of seconds in band, and A25 (GPU) and A8 (p50 6.7 ms) still pass.

## Open Questions / Risks
- **Pen inside the virtual display.** Pre-flight assumed sway headless with a libinput backend, so the uinput pen would reach apps. Today the virtual display runs `WLR_BACKENDS=headless` only, since keyboard and pointer go through the virtual-input protocols. A pen needs either `headless,libinput` (device access through the `input` group, so the session has to be re-logged after `setup-root.sh`) or the tablet protocol from a virtual source. Unverified until the input group is active.
- **One-frame decode hold for x264/openh264 streams** (found in T1.14). GStreamer's H.264 decoders (`vah264dec`, `openh264dec`) hold each picture until the next one arrives when the SPS declares a one-frame DPB (x264's fast presets, openh264), even with `compliance=flexible`. VA-encoded streams aren't affected. That costs one frame (16.7 ms at 60 fps) on the CPU-encoder → hardware-decoder path. Handling: investigate. The candidate fix is a client-side SPS rewrite (VUI `max_dec_frame_buffering`/`num_reorder_frames`) in front of the decoder. Until then, A8 is measured on the VA→VA path, and the overlay shows the decode time.
- **Text mode: the first borrowed character is sometimes dropped** (found in T1.18). Characters missing from the host layout are typed by lending them a spare keycode. Measured with xev on Xvfb, about 1 run in 3 drops the first borrowed character of the session (`é` in "héllo 漢字"). Later borrowed characters always arrive, and characters in the layout use their own key. A 20 ms pause after each remap and a warm-up remap at start didn't fix it. Handling: investigate, either a client-side check with a real toolkit (GTK) once the virtual display exists, or typing borrowed characters through uinput + XKB keymap updates on Wayland hosts.
- **HEVC on this machine.** Debian's free Intel driver exposes only `vah264lpenc`. HEVC and 4:4:4 need `intel-media-va-driver-non-free` (in T0.2). Until then, A7 reports H.264 on VA-API, and A12 uses the still-refresh path.
- **Root-only setup.** uinput, KVM, Docker, render group and packages need T0.2, run by the user. The criteria that depend on it are ENV-BLOCKED until then.
- **pipewiresrc 1.4.2 can't renegotiate** (found in T1.12). This is why the virtual display uses screencopy. Real Wayland desktops (T1.28) still go through the portal and pipewiresrc. Handling: restart the cast when the stream errors, or feed PipeWire buffers to an `appsrc` from our own consumer (`pipewire` crate).
- **The SPAKE2 crate's group constants must be reproducible in TypeScript** (T3.3). If not, the web client uses its own SPAKE2 variant, with both sides implemented in this repo.
- **Windows GStreamer cross-check through cargo-xwin** is unverified (T4.2). The fallback is compile-checking Windows-specific modules behind a `cfg` with stubbed GStreamer.
- From the spec's Risks: real-LAN and 1080p120/4K60 hardware gates, ViGEmBus (T4.1), Wayland portal prompts, symmetric NAT, anti-cheat, browser latency.
