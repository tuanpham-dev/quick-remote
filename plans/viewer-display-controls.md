# Plan: QuickRemote Display Controls

- **Session:** 50f466a1-d370-44db-b235-bb4ca30125a2
- **Spec:** `plans/viewer-display-controls.spec.html` (published: https://claude.ai/artifact/DT8vXyar5WbaR98z7Wu2Rp)

## Goal
Clients choose the remote desktop's size (follow window / fixed), sharpness (pixel ratio + desktop scale) and view (fit, fill, 100%, zoom, pan); the host sizes its own display, a KDE/GNOME virtual monitor or the real monitor; plus touch, reconnect, pause-while-hidden, keyboard lock, and one adaptive rate controller (spec R1–R23).

## Approach
- **Protocol:** three new control messages next to the existing ones, so old clients keep working: `DisplayRequest` (client → host: stream, width, height, refresh, scale ×100, target), `DisplayInfo` (host → client: what this desktop can do, current size and scale, an optional note such as "not available on this monitor") and `Pause` (client → host: paused flag). `Resize` stays and is handled as a `DisplayRequest` with the current scale and target.
- **Host display manager** (new `display.rs`): owns "what is the stream's screen" per session. Kinds: own sway display (resize + `output scale`), KDE (KWin `stream_virtual_output` with width/height/scale, or real output mode/scale through `kscreen-doctor`), GNOME (mutter `RecordVirtual` in a replacement session, or real mode/scale through `DisplayConfig.ApplyMonitorsConfig`), fixed (X11, Windows: no resize, `DisplayInfo` says so). Virtual monitors outlive their session by 60 s, keyed by client identity so the reconnecting client gets the same one.
- **Primary:** a KDE virtual monitor becomes primary through `kscreen-doctor output.<name>.priority.1` when no non-virtual output exists; GNOME through the `primary` flag in `ApplyMonitorsConfig` on the same condition.
- **Rate control:** replace the LAN/Internet branches in `ratectl.rs` with one controller: delay gradient with a jitter-tolerant baseline (rising only when the minimum-delay envelope rises for several windows, not single spikes), loss beyond FEC that persists across 2 windows cuts to 85% of the received rate, clean windows grow 8%/s near the last cut and 50%/s after 3 s quiet, start at the old LAN target capped by `--max-kbps`. Pacing and the keyframe throttle (already written, uncommitted) are kept.
- **Clients** share one behavior model: settings object {resolution mode, fixed size + Hz, ratio, target, view mode}, persisted (localStorage / native config file / Android SharedPreferences); a 300 ms debounce turns window/settings changes into `DisplayRequest`; view transform (scale + pan) is separate from the remote size and is the single source for pointer mapping.
- Real-monitor mode change on KDE through `kscreen-doctor` (part of Plasma) rather than speaking `kde_output_management_v2` ourselves: one CLI call, and Plasma always ships it. Over the alternative: a full Wayland client for the output-management protocol.

## Architecture

```
 Web / Native / Android client
   settings ──300ms──▶ DisplayRequest{w,h,Hz,scale,target} ─┐      ┌── video (paced datagrams)
   view zoom/pan (local)          Pause{paused}  ───────────┤      │
   touch, keyboard strip ───────── input ──────────────────┤      │
   ◀──────────── DisplayInfo{kind,w,h,scale,note} ──────────┼──┐   │
                                                            ▼  │   │
 Host session ─────▶ display manager ──┬─ sway: set_mode + output scale
        │  feedback                    ├─ KWin: stream_virtual_output(w,h,scale) │ kscreen-doctor mode/scale/priority
        ▼                              ├─ mutter: replacement session + RecordVirtual │ ApplyMonitorsConfig
   adaptive ratectl ── bitrate/fps ──▶ encoder ◀── capture (restart on new node/size)
        └── keyframe ≤1/250ms                └──▶ paced sender ──▶ client
```

- **Components:** `display.rs` decides and applies the screen; `video.rs` starts capture from the node or socket `display.rs` hands it; `kwin.rs`/`mutter.rs`/`vdisplay.rs` gain the per-desktop operations; `ratectl.rs` one controller; clients gain settings, view transform, gestures, reconnect, pause.
- **Data flow:** the client's settings are the source of truth for the request; the host's `DisplayInfo` is the source of truth for what was applied; the view transform lives only in the client.
- **Decisions:**
  - New messages over changing `Resize`'s layout — because old native/Android builds keep working against a new host and vice versa.
  - Replacement mutter session for GNOME virtual monitors over adding streams to the running one — because mutter starts no stream recorded after `Start` (verified 2026-10-08 with `RecordMonitor`).
  - `kscreen-doctor` for KDE real-monitor changes over a native protocol client — Plasma ships it; far less code.

## Phases

### Phase 1: Rate control and delivery
**Goal:** one adaptive controller, pacing and keyframe throttle committed and tested.
**Checkpoint:** `cargo test -p qr-host ratectl` passes; `tests/e2e/run.sh wifi lan` passes A17 and A19; `tests/e2e/run.sh a10_a11 wan` passes; the forced-LAN wrapper is gone from `dist/quickremote-test/`.

- [x] **T1 — Single controller**
  - Files: `crates/qr-host/src/ratectl.rs`, `crates/qr-host/src/session.rs`, `crates/qr-host/src/main.rs`
  - Do: replace the `Profile::Lan`/`Profile::Internet` branches in `RateCtl::on_feedback` with one path (delay envelope over 8 windows; "rising" needs 3 consecutive rising windows above 8 ms queueing; loss beyond FEC in 2 consecutive windows → cut to 85% of received; growth 8%/s for 2.5 s after a cut, then 50%/s; start target = `Limits::lan_target()`); `RateCtl::new` drops its profile argument; `pick_profile` removed, `SessionStart.profile` sent as 1; `--profile` parsed, ignored, warns "the --profile option has no effect".
  - Depends on: —
  - Done when: unit tests rewritten for the single controller pass (clean link reaches the target, persistent loss cuts, single loss window doesn't, jitter-only windows don't cut, recovery from a squeeze within 5 s simulated).
- [x] **T2 — Wi-Fi simulation test**
  - Files: `tests/e2e/wifi.sh` (new), `tests/e2e/netns.sh` (a `wifi` topology), `tests/e2e/run.sh`
  - Do: topology with 30 Mbit/s, `delay 15ms 15ms distribution normal`, 0.5% loss, plus a loop applying 1% loss for 2 s every 10 s; host with `--source test:ball --size 1920x1080` and `x264enc`; client `--stats-json`; checks A17 (no 3 s run under 30 fps, median ≥ 50) and A19 (host-logged forced keyframes ≤ 4 in any second, via a new `debug` log line "forced keyframe").
  - Depends on: T1
  - Done when: `tests/e2e/run.sh wifi wifi` prints PASS for both checks.
- [x] **T3 — Commit delivery fixes, retire the wrapper**
  - Files: `crates/qr-host/src/stream.rs`, `crates/qr-host/src/session.rs`, `dist/quickremote-test/qr-host`
  - Do: run `a1_a2 lan`, `a10_a11 wan`, `a7 lan`; commit pacing + keyframe throttle + T1; replace the wrapper with the real binary.
  - Depends on: T1, T2
  - Done when: the three scripts pass and `git status` is clean for these files.

### Phase 2: Protocol and host display manager
**Goal:** the host applies display requests on sway, KDE and GNOME and reports what it did.
**Checkpoint:** headless tests `vd_display`, `kde_display`, `gnome_display` pass (A1-host part, A3–A7, A20, A21-host part).

- [x] **T4 — Messages**
  - Files: `crates/qr-proto/src/msg.rs`, `web/src/proto.ts`, `conformance/vectors.json`
  - Do: tags 28 `DisplayRequest {stream:u8, width:u16, height:u16, refresh:u16, scale_pct:u16, target:u8}`, 29 `DisplayInfo {stream:u8, kind:u8, width:u16, height:u16, scale_pct:u16, note:String}` (kind: 0 fixed, 1 own display, 2 KDE, 3 GNOME), 30 `Pause {paused:bool}`; encode/decode in Rust (`crates/qr-proto/src/msg.rs`) and TS (`web/src/proto.ts`); vectors for each (`crates/qr-proto/src/samples.rs`, regenerate `conformance/vectors.json` with `gen-vectors`); `PROTO_MINOR` 0 → 1 in both; decoders return `Msg::Unknown(tag)` / `{t:"Unknown"}` for an unknown tag instead of failing (callers ignore it); senders gate the new messages on the peer's minor ≥ 1 (host reads `Hello.minor`, clients read `HelloAck.minor`).
  - Depends on: —
  - Done when: `cargo test -p qr-proto` and `web/test/vectors.test.ts` pass with the new vectors; a decode test feeds tag 250 and gets Unknown.
- [x] **T5 — Display manager skeleton + sway**
  - Files: `crates/qr-host/src/display.rs` (new), `crates/qr-host/src/session.rs`, `crates/qr-host/src/video.rs`, `crates/qr-host/src/vdisplay.rs`, `crates/qr-host/src/nolinux.rs`
  - Do: `DisplayManager::apply(request) -> DisplayInfo`; session handles `DisplayRequest` and maps `Resize` onto it (current scale/target); sends `DisplayInfo` at session start and after each apply; sway: existing `set_mode` plus new `VirtualDisplay::set_scale(output, scale)` (`swaymsg output <name> scale <s>`); X11/Windows: kind 0, no change, note "This desktop keeps its own size".
  - Depends on: T4
  - Done when: `tests/e2e/vd_display.sh` (new, headless client with a script command `display W H HZ SCALE TARGET`) sees StreamStarted at the requested size and `swaymsg -t get_outputs` scale 2 after a 2× request.
- [x] **T6 — KDE virtual and real monitor**
  - Files: `crates/qr-host/src/kwin.rs`, `crates/qr-host/src/display.rs`
  - Do: `KwinSession::stream_virtual(name, w, h, scale) -> node` via `stream_virtual_output` (bind screencast v4 → v2+ request exists); one virtual monitor per client key, replaced on change, closed 60 s after the session ends (timer in `DisplayManager`, cancelled on reconnect with the same key); after creation, if `kscreen-doctor -j` lists no non-virtual output enabled, set `output.<name>.priority.1`; real target: `kscreen-doctor output.<name>.mode.<W>x<H>@<Hz>` when listed, `output.<name>.scale.<s>`, else note.
  - Depends on: T5
  - Done when: `tests/e2e/kde_display.sh` (new, headless KWin, checks permission path unchanged) passes A3, A4 (with 20 s / 70 s waits), A6, A7-KDE.
- [ ] **T7 — GNOME virtual and real monitor**
  - Files: `crates/qr-host/src/mutter.rs`, `crates/qr-host/src/display.rs`
  - Do: `MutterSession::open` gains an optional virtual request; a virtual target replaces the session with one that also has `RecordVirtual` (stream properties `size`, `is-platform: true`), monitors re-read; scale and primary through `ApplyMonitorsConfig` (method 1) on the virtual logical monitor; real target: mode switch when listed, else note.
  - Depends on: T5
  - Done when: `tests/e2e/gnome_display.sh` (new) passes A5 and real-mode note behavior on mutter's fixed virtual monitor.
- [x] **T8 — Pause**
  - Files: `crates/qr-host/src/session.rs`, `crates/qr-host/src/video.rs`, `crates/qr-host/src/stream.rs`
  - Do: `Pause{true}` holds all encoders (existing `encoder.hold`), `Pause{false}` releases and forces a keyframe.
  - Depends on: T4
  - Done when: script command `pause`/`resume` in the headless client shows no frames while paused and a keyframe within 1 s of resume.

### Phase 3: Web client
**Goal:** all web behaviors in the spec.
**Checkpoint:** `node tests/e2e/web_display.mjs` passes A1, A2, A8–A14, A21 (web part).

- [ ] **T9 — Settings model + Display popover**
  - Files: `web/src/settings.ts` (new), `web/src/main.ts`, `web/index.html`, `web/src/style.css` or inline styles in `index.html`
  - Do: settings {mode: follow|fixed, fixed {w,h,hz}, ratio: auto|1|1.5|2, target: virtual|real, view: fit|fill|native}, localStorage with try/catch; popover per the spec mock; `DisplayInfo` drives which rows show (kind 0 disables resolution with the note; kind 1 hides target) and shows notes.
  - Depends on: T4
  - Done when: the popover renders in headless Chrome, values survive a reload.
- [ ] **T10 — Requests, view transform, zoom/pan, pointer mapping**
  - Files: `web/src/main.ts`, `web/src/view.ts` (new)
  - Do: ResizeObserver + settings changes → 300 ms debounce → `DisplayRequest` (CSS size × ratio, even); `View` class holds base scale (fit/fill/native) × zoom (0.25–4) and pan; Ctrl+wheel and +/− buttons; drag-to-pan when larger than the canvas area with the middle of the view stable on zoom; all pointer events map through `View.toRemote(x, y)`.
  - Depends on: T9
  - Done when: A1, A2, A8 pass.
- [ ] **T11 — Reconnect + pause while hidden**
  - Files: `web/src/main.ts`
  - Do: on `wt.closed` keep canvas, overlay "Reconnecting…", retry 1/2/4/8/10 s with the stored browser key (no PIN), re-send settings; `visibilitychange` → `Pause`.
  - Depends on: T8, T10
  - Done when: A10, A11 pass.
- [ ] **T12 — Touch + keyboard strip + keyboard lock**
  - Files: `web/src/touch.ts` (new), `web/src/main.ts`, `web/index.html`
  - Do: gesture recognizer per R18 (tap, 500 ms long press with 10 px slop, 2-finger tap/drag, 3-finger tap, 4-finger tap, pinch → `View`); toolbar for touch devices (`pointer: coarse`); hidden textarea for the soft keyboard sending `Text` input; extra-keys strip with sticky modifiers (tap = once, double-tap = lock); fullscreen button calls `navigator.keyboard.lock()` when present; Escape held 1 s exits.
  - Depends on: T10
  - Done when: A12 (web), A13, A14 pass with CDP touch and key emulation.

### Phase 4: Native client
**Goal:** the same settings and view in the window client.
**Checkpoint:** `tests/e2e/native_display.sh` passes A15.

- [ ] **T13 — Settings, requests, view, reconnect, pause**
  - Files: `crates/qr-client/src/app.rs`, `crates/qr-client/src/session.rs`, `crates/qr-client/src/settings.rs` (new), `crates/qr-client/src/script.rs`
  - Do: settings stored in the client config dir (`display.json`); Display menu (egui window, hotkey Ctrl+Alt+Shift+V); window resize → 300 ms debounce → `DisplayRequest` (replaces the direct `Resize`); view transform with Ctrl+wheel and Ctrl+Alt+Shift+Plus/Minus/0, drag-pan with middle button when zoomed; pointer mapping through it; reconnect loop with backoff keeping the last frame; `Pause` on minimize; script commands `display` and `zoom` for tests.
  - Depends on: T4, T5, T8
  - Done when: `tests/e2e/native_display.sh` (headless client against the sway display, plus a windowed run under Xvfb for the menu) passes.

### Phase 5: Android
**Goal:** the same on Android.
**Checkpoint:** `tests/e2e/android.sh` extended passes A12 (Android), A16.

- [ ] **T14 — FFI**
  - Files: `crates/qr-ffi/src/core.rs`, `crates/qr-ffi/src/lib.rs`, `android/app/src/main/java/dev/quickremote/Native.kt`
  - Do: `displayRequest(w,h,hz,scalePct,target)`, `pause(bool)`, `DisplayInfo` forwarded as an event JSON `{"t":"display",...}`; reconnect support (`connect` again with the stored identity).
  - Depends on: T4
  - Done when: `scripts/build-android.sh` builds.
- [ ] **T15 — UI and gestures**
  - Files: `android/app/src/main/java/dev/quickremote/SessionActivity.kt`, `android/app/src/main/java/dev/quickremote/Settings.kt` (new)
  - Do: toolbar (Keyboard, Zoom %, Display dialog), SharedPreferences settings, follow-screen size with orientation, view transform (ScaleGestureDetector + pan), gestures per R18 (three-finger tap = middle, four-finger tap = keyboard, long press = right), extra-keys strip above the IME, reconnect overlay, `pause` in onPause/onResume.
  - Depends on: T14
  - Done when: the extended emulator test passes.

### Phase 6: Regression, docs, package
**Goal:** everything green and shipped to the test package.
**Checkpoint:** `tests/e2e/run.sh phase1` all PASS; `scripts/package.sh` builds; `docs/TESTING.md` updated.

- [ ] **T16 — Suite, docs, package**
  - Files: `tests/e2e/run.sh`, `docs/TESTING.md`, `plans/quick-remote.md`
  - Do: add the new scripts to phase1; describe the Display popover, gestures and keyboard strip in TESTING.md; plan notes; rebuild the package; restart the live test host from it.
  - Depends on: T1–T15
  - Done when: phase1 passes, package built.

## Constraints
- Old clients against a new host and new clients against an old host must still connect: today's decoders fail on an unknown tag (verified: Rust `BadTag("message")`, TS throws), so new messages are sent only to a peer whose handshake minor version is ≥ 1 (T4).
- No new runtime dependency on the host beyond `kscreen-doctor` on KDE (only used for real-monitor and primary changes; virtual monitors work without it).
- Tests run headless in the netns harness; the live test host is not restarted until Phase 6.

## Open Questions / Risks
- **Found in T6:** KWin's headless backend (`--virtual`) can't add virtual outputs (only the DRM backend implements `createVirtualOutput`), so KDE virtual monitors (A3, A4) can't be verified on this machine; the host detects a headless KWin, uses the real monitor and says so in `DisplayInfo` (tested as A3-fallback). `kscreen-doctor` must run with its own defaults (Qt's offscreen platform reads a fake screen).
- KWin `stream_virtual_output` scale and refresh handling, and mutter `RecordVirtual` + `ApplyMonitorsConfig` on a virtual monitor, are unverified: T6/T7 start by probing them on the headless desktops; any missing capability becomes a `DisplayInfo` note and a spec update.
