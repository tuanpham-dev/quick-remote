// QuickRemote web client.

import { authenticate, CertChanged, PinNeeded, wasm } from "./auth.ts";
import { EVDEV } from "./keymap.ts";
import {
  ClipKind, Codec, DisplayKind, DisplayTarget, Kind, Mode, PROTO_MAJOR, PROTO_MINOR, decodeAudio, decodeMsg, encodeInput,
  encodeMotion, encodeMsg, encodePad, type CodecCap, type InputEvent, type Msg,
} from "./proto.ts";
import * as cfg from "./settings.ts";
import { attachTouch } from "./touch.ts";
import { View } from "./view.ts";
import { FramedReader, FramedWriter } from "./transport.ts";
import { WebReassembler } from "./wasm/qr_wasm.js";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const canvas = $<HTMLCanvasElement>("v");
const ctx2d = canvas.getContext("2d", { desynchronized: true, alpha: false })!;
const overlay = $<HTMLDivElement>("overlay");
const status = $<HTMLDivElement>("status");
const pinBox = $<HTMLFormElement>("pin");
const pinInput = $<HTMLInputElement>("pin-input");
const startBtn = $<HTMLButtonElement>("start");
const fileInput = $<HTMLInputElement>("file");
const stage = $<HTMLDivElement>("stage");
const banner = $<HTMLDivElement>("banner");
// The picture ends above the bar (the bar would hide and swallow clicks on
// the desktop's bottom strip); its height changes as it wraps.
const bar = $<HTMLDivElement>("bar");
new ResizeObserver(() => { stage.style.bottom = `${bar.offsetHeight}px`; }).observe(bar);

// What size and look the user wants (kept in this browser), and how the
// picture sits in the page.
let settings = cfg.load();
const view = new View(stage, canvas);
view.mode = settings.view;
view.ratio = cfg.effectiveRatio(settings);

const nowUs = () => Math.round(performance.now() * 1000);
const say = (s: string) => { status.textContent = s; };

// Per-page diagnostics for automated tests.
const stats = {
  fps: 0, kbps: 0, decoded: 0, lost: 0, repaired: 0, decoder: "", codec: "", path: "", rttMs: 0,
  decodeMs: 0, totalMs: 0, audioPackets: 0, width: 0, height: 0, connected: false, error: "",
  displayKind: -1, displayNote: "", zoom: 1, reconnects: 0,
};
(window as unknown as { qrStats: typeof stats }).qrStats = stats;

let session: Session | null = null;

class Session {
  wt: WebTransport;
  control!: FramedWriter;
  input!: FramedWriter;
  reasm = new WebReassembler(34);
  decoder: VideoDecoder | null = null;
  codecString = "";
  needKey = true;
  offsetUs = 0; // host clock − page clock
  bestRtt = Infinity;
  pending = new Map<number, { capture: number; send: number; pushed: number }>();
  win = { frames: 0, bytes: 0, decodeUs: 0, totalUs: 0, n: 0 };
  fb = { bytes: 0, shardsRx: 0, shardsLost: 0, repaired: 0, framesRx: 0, framesLost: 0, delays: [] as [number, number][] };
  moved = { seq: 0, relX: 0n, relY: 0n };
  relative = false;
  sharp = false;
  audio: AudioPipe | null = null;
  pads = new Map<number, { seq: number; last: string; at: number }>();
  files = new Map<number, File>();
  nextFileId = 1;
  closed = false;
  /** The host's protocol minor version: display requests and pause need 1+. */
  hostMinor = 0;
  /** The user quit (no reconnect). */
  quit = false;
  /** One writer for all outgoing datagrams (a stream allows only one). */
  dgw: WritableStreamDefaultWriter<Uint8Array>;

  constructor(wt: WebTransport) {
    this.wt = wt;
    this.dgw = wt.datagrams.writable.getWriter();
    // A video frame arrives as a burst of datagrams (hundreds for a busy
    // 1080p picture) while this thread may be decoding or drawing; Chrome's
    // default queue holds only a few and drops the rest, which reads as
    // network loss and drives the host's bitrate and frame rate down.
    const dg = wt.datagrams as WebTransportDatagramDuplexStream & { incomingHighWaterMark?: number; incomingMaxAge?: number | null };
    dg.incomingHighWaterMark = 16384;
    dg.incomingMaxAge = null;
  }

  async start(): Promise<void> {
    const s = await this.wt.createBidirectionalStream();
    this.control = new FramedWriter(s.writable);
    const reader = new FramedReader(s.readable);
    const uni = await this.wt.createUnidirectionalStream();
    const iw = uni.getWriter();
    await iw.write(new Uint8Array([1]));
    iw.releaseLock();
    this.input = new FramedWriter(uni);
    await this.send({
      t: "Hello", major: PROTO_MAJOR, minor: PROTO_MINOR, name: "browser", os: navigator.platform || "web",
      wantWidth: 0, wantHeight: 0, wantRefresh: 0, decoders: await decoderCaps(),
    });
    void this.controlLoop(reader);
    void this.datagramLoop();
    void this.uniLoop();
    this.timers();
  }

  send(m: Msg): Promise<void> { return this.control.send(encodeMsg(m)); }
  sendInput(e: InputEvent): void { void this.input.send(encodeInput(e)); }

  async controlLoop(r: FramedReader): Promise<void> {
    try {
      for (;;) {
        const b = await r.next();
        if (!b) break;
        this.onMsg(decodeMsg(b));
      }
    } catch (e) {
      stats.error = String(e);
    }
    this.end("The host closed the session.");
  }

  onMsg(m: Msg): void {
    switch (m.t) {
      case "HelloAck":
        say(`Connected to ${m.name}`);
        this.hostMinor = m.minor;
        break;
      case "DisplayInfo": onDisplayInfo(m); break;
      case "SessionStart":
        stats.path = ["", "LAN", "Direct", "Relayed"][m.path] ?? "";
        stats.connected = true;
        break;
      case "StreamStarted":
        stats.codec = ["", "h264", "h265", "av1"][m.codec] ?? "";
        canvas.width = m.width;
        canvas.height = m.height;
        stats.width = m.width;
        stats.height = m.height;
        view.layout();
        this.reasm.reset();
        this.needKey = true;
        this.decoder?.close();
        this.decoder = null;
        this.codecString = "";
        break;
      case "Pong": {
        const t = nowUs();
        const rtt = t - Number(m.tClient);
        stats.rttMs = rtt / 1000;
        if (rtt <= this.bestRtt * 1.2) {
          this.bestRtt = Math.min(this.bestRtt, rtt);
          this.offsetUs = Number(m.tHost) - (Number(m.tClient) + rtt / 2);
        }
        break;
      }
      case "Clipboard":
        if (m.kind === ClipKind.TEXT) navigator.clipboard?.writeText(new TextDecoder().decode(m.data)).catch(() => {});
        break;
      case "FileOffer": void this.send({ t: "FileAccept", id: m.id }); break;
      case "FileAccept": void this.upload(m.id); break;
      case "FileDone": say(m.ok ? "File sent." : "The host's copy of the file didn't match."); break;
      case "Rumble": {
        const pad = navigator.getGamepads()[m.idx];
        const act = (pad as unknown as { vibrationActuator?: { playEffect: (t: string, o: object) => Promise<unknown> } })?.vibrationActuator;
        act?.playEffect("dual-rumble", { duration: m.ms, strongMagnitude: m.strong / 65535, weakMagnitude: m.weak / 65535 }).catch(() => {});
        break;
      }
      case "Error": stats.error = m.text; say(m.text); break;
      default: break;
    }
  }

  async datagramLoop(): Promise<void> {
    const r = this.wt.datagrams.readable.getReader();
    try {
      for (;;) {
        const { value, done } = await r.read();
        if (done) break;
        switch (value[0]) {
          case Kind.VIDEO:
            this.fb.bytes += value.length;
            if (this.reasm.push(value, performance.now()) > 0) this.drain();
            break;
          case Kind.AUDIO:
            stats.audioPackets++;
            this.audio?.push(decodeAudio(value));
            break;
          default: break;
        }
      }
    } catch (e) {
      stats.error = String(e);
    }
  }

  drain(): void {
    for (let f = this.reasm.next(); f; f = this.reasm.next()) {
      if (f.lost) {
        this.needKey = true;
        stats.lost++;
        void this.send({ t: "FrameLost", stream: 0, frame: f.id });
        f.free();
        continue;
      }
      if (this.needKey && !f.keyframe) { f.free(); continue; }
      const au = f.au;
      if (f.keyframe) {
        this.needKey = false;
        this.configure(f.codec, au);
      }
      const t = nowUs();
      this.fb.delays.push([t, t - f.send_us]);
      this.pending.set(f.id, { capture: f.capture_us, send: f.send_us, pushed: t });
      if (this.pending.size > 64) this.pending.clear();
      this.win.bytes += au.length;
      try {
        this.decoder?.decode(new EncodedVideoChunk({ type: f.keyframe ? "key" : "delta", timestamp: f.id, data: au }));
      } catch (e) {
        stats.error = String(e);
        this.needKey = true;
      }
      f.free();
    }
  }

  configure(codec: number, keyframe: Uint8Array): void {
    const s = codecString(codec, keyframe);
    if (this.decoder && s === this.codecString && this.decoder.state === "configured") return;
    this.decoder?.close();
    this.codecString = s;
    this.decoder = new VideoDecoder({
      output: (frame) => this.present(frame),
      error: (e) => { stats.error = String(e); this.needKey = true; void this.send({ t: "KeyframeReq", stream: 0 }); },
    });
    this.decoder.configure({ codec: s, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
    stats.decoder = s;
  }

  present(frame: VideoFrame): void {
    const id = frame.timestamp;
    ctx2d.drawImage(frame, 0, 0, canvas.width, canvas.height);
    frame.close();
    const t = nowUs();
    const p = this.pending.get(id);
    this.pending.delete(id);
    stats.decoded++;
    this.win.frames++;
    if (p && this.bestRtt < Infinity) {
      this.win.decodeUs += t - p.pushed;
      this.win.totalUs += t - (p.capture - this.offsetUs);
      this.win.n++;
    }
  }

  async uniLoop(): Promise<void> {
    const r = this.wt.incomingUnidirectionalStreams.getReader();
    for (;;) {
      const { value, done } = await r.read().catch(() => ({ value: undefined, done: true }));
      if (done || !value) return;
      void this.download(value);
    }
  }

  async download(stream: ReadableStream<Uint8Array>): Promise<void> {
    const r = new FramedReader(stream);
    const head = await r.raw(5);
    if (!head || head[0] !== 2) return;
    const id = new DataView(head.buffer, head.byteOffset).getUint32(1, true);
    const parts: Uint8Array[] = [];
    for await (const c of r.rest()) parts.push(c);
    const blob = new Blob(parts as BlobPart[]);
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `quickremote-${id}`;
    a.click();
    void this.send({ t: "FileDone", id, ok: true });
  }

  async offer(file: File): Promise<void> {
    const id = this.nextFileId;
    this.nextFileId += 2;
    this.files.set(id, file);
    const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", await file.arrayBuffer()));
    void this.send({ t: "FileOffer", id, name: file.name, size: BigInt(file.size), sha256: digest });
  }

  async upload(id: number): Promise<void> {
    const file = this.files.get(id);
    if (!file) return;
    const s = await this.wt.createUnidirectionalStream();
    const w = s.getWriter();
    const head = new Uint8Array(5);
    head[0] = 2;
    new DataView(head.buffer).setUint32(1, id, true);
    await w.write(head);
    const rd = file.stream().getReader();
    for (;;) {
      const { value, done } = await rd.read();
      if (done) break;
      await w.write(value);
    }
    await w.close();
    say(`Sent ${file.name}.`);
  }

  timers(): void {
    let seq = 0;
    let last = performance.now();
    const tick = setInterval(() => {
      if (this.closed) { clearInterval(tick); return; }
      seq++;
      if (this.reasm.poll(performance.now()) > 0) this.drain();
      const s = this.reasm.take_stats();
      this.fb.shardsRx += s[1]; this.fb.shardsLost += s[2]; this.fb.repaired += s[3];
      this.fb.framesRx += s[4]; this.fb.framesLost += s[5];
      stats.repaired += s[3];
      if (seq % 4 === 0) void this.send({ t: "Ping", seq, tClient: BigInt(nowUs()) });
      const d = this.fb.delays;
      void this.send({
        t: "Feedback", seq, windowMs: 50, bytesRx: Math.min(this.fb.bytes, 0xffffffff), shardsRx: this.fb.shardsRx,
        shardsLost: this.fb.shardsLost, shardsRepaired: this.fb.repaired, framesRx: this.fb.framesRx,
        framesLost: this.fb.framesLost,
        delayMinUs: d.length ? Math.round(Math.min(...d.map((x) => x[1])) % 2 ** 31) : 0,
        delayMaxUs: d.length ? Math.round(Math.max(...d.map((x) => x[1])) % 2 ** 31) : 0,
        delaySlope: slope(d), decodeUs: 0, presentUs: 0,
      });
      this.fb = { bytes: 0, shardsRx: 0, shardsLost: 0, repaired: 0, framesRx: 0, framesLost: 0, delays: [] };
      this.pollPads();
      const now = performance.now();
      if (now - last >= 1000) {
        const secs = (now - last) / 1000;
        last = now;
        stats.fps = this.win.frames / secs;
        stats.kbps = Math.round((this.win.bytes * 8) / secs / 1000);
        if (this.win.n) {
          stats.decodeMs = this.win.decodeUs / this.win.n / 1000;
          stats.totalMs = this.win.totalUs / this.win.n / 1000;
        }
        this.win = { frames: 0, bytes: 0, decodeUs: 0, totalUs: 0, n: 0 };
        overlay.textContent =
          `${stats.width}×${stats.height} · ${stats.fps.toFixed(0)} fps · ${stats.codec} (${stats.decoder})\n` +
          `${stats.kbps} kbit/s · lost ${stats.lost} · FEC ${stats.repaired} · ${stats.path} · RTT ${stats.rttMs.toFixed(1)} ms\n` +
          `decode ${stats.decodeMs.toFixed(1)} ms · capture→shown ${stats.totalMs.toFixed(1)} ms · WebTransport`;
      }
    }, 50);
  }

  pollPads(): void {
    const pads = navigator.getGamepads?.() ?? [];
    for (let i = 0; i < 4; i++) {
      const p = pads[i];
      const known = this.pads.get(i);
      if (!p && known) {
        this.pads.delete(i);
        void this.send({ t: "PadDetach", idx: i });
        continue;
      }
      if (!p) continue;
      if (!known) {
        this.pads.set(i, { seq: 0, last: "", at: 0 });
        void this.send({ t: "PadAttach", idx: i, kind: 1 });
      }
      const st = this.pads.get(i)!;
      const b = (n: number) => (p.buttons[n]?.pressed ? 1 : 0);
      // Standard mapping → protocol bits (A B X Y LB RB Back Start Guide LS RS Up Down Left Right).
      const order = [0, 1, 2, 3, 4, 5, 8, 9, 16, 10, 11, 12, 13, 14, 15];
      let buttons = 0;
      order.forEach((n, bit) => { if (b(n)) buttons |= 1 << bit; });
      const ax = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v * 32767)));
      const axes = [ax(p.axes[0] ?? 0), ax(p.axes[1] ?? 0), ax(p.axes[2] ?? 0), ax(p.axes[3] ?? 0),
        Math.round((p.buttons[6]?.value ?? 0) * 32767), Math.round((p.buttons[7]?.value ?? 0) * 32767)];
      const key = `${buttons}:${axes.join(",")}`;
      const t = performance.now();
      if (key !== st.last || t - st.at > 100) {
        st.seq++;
        st.last = key;
        st.at = t;
        this.dgw.write(encodePad({ idx: i, seq: st.seq, buttons, axes })).catch(() => {});
      }
    }
  }

  motion(dx: number, dy: number, abs: [number, number] | null): void {
    const m = this.moved;
    m.seq++;
    m.relX += BigInt(Math.trunc(dx));
    m.relY += BigInt(Math.trunc(dy));
    const d = encodeMotion({
      seq: m.seq, relX: m.relX, relY: m.relY, hasAbs: !!abs, stream: 0, absX: abs?.[0] ?? 0, absY: abs?.[1] ?? 0,
      tClientUs: BigInt(nowUs()),
    });
    this.dgw.write(d).catch(() => {});
  }

  end(why: string): void {
    if (this.closed) return;
    this.closed = true;
    stats.connected = false;
    say(why);
    this.decoder?.close();
    this.audio?.close();
    try { this.wt.close(); } catch { /* already closed */ }
  }
}

function slope(d: [number, number][]): number {
  if (d.length < 3) return 0;
  const t0 = d[0][0];
  let sx = 0, sy = 0, sxx = 0, sxy = 0;
  for (const [t, y] of d) { const x = (t - t0) / 1e6; sx += x; sy += y; sxx += x * x; sxy += x * y; }
  const n = d.length, den = n * sxx - sx * sx;
  return den === 0 ? 0 : Math.max(-(2 ** 31), Math.min(2 ** 31 - 1, Math.round((n * sxy - sx * sy) / den)));
}

/** The WebCodecs codec string, from the keyframe's parameter sets. */
function codecString(codec: number, au: Uint8Array): string {
  if (codec === Codec.H264) {
    for (let i = 0; i + 7 < au.length; i++) {
      if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1 && (au[i + 3] & 0x1f) === 7) {
        const h = (x: number) => x.toString(16).padStart(2, "0");
        return `avc1.${h(au[i + 4])}${h(au[i + 5])}${h(au[i + 6])}`;
      }
    }
    return "avc1.640033";
  }
  if (codec === Codec.H265) return "hev1.1.6.L153.B0";
  return "av01.0.08M.08";
}

async function decoderCaps(): Promise<CodecCap[]> {
  const out: CodecCap[] = [];
  const tries: [number, string][] = [[Codec.AV1, "av01.0.08M.08"], [Codec.H265, "hev1.1.6.L153.B0"], [Codec.H264, "avc1.640033"]];
  for (const [codec, s] of tries) {
    for (const hw of [true, false]) {
      try {
        const r = await VideoDecoder.isConfigSupported({ codec: s, hardwareAcceleration: hw ? "prefer-hardware" : "prefer-software" });
        if (r.supported) { out.push({ codec, hw, maxWidth: 4096, maxHeight: 2304, chroma444: false }); break; }
      } catch { /* unsupported */ }
    }
  }
  return out;
}

/** Opus host audio: decoded and scheduled 20 ms ahead. */
class AudioPipe {
  ctx = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
  dec: AudioDecoder;
  next = 0;
  constructor() {
    this.dec = new AudioDecoder({
      output: (d) => {
        const buf = this.ctx.createBuffer(d.numberOfChannels, d.numberOfFrames, d.sampleRate);
        for (let c = 0; c < d.numberOfChannels; c++) {
          const ch = new Float32Array(d.numberOfFrames);
          d.copyTo(ch, { planeIndex: c, format: "f32-planar" });
          buf.copyToChannel(ch, c);
        }
        d.close();
        const src = this.ctx.createBufferSource();
        src.buffer = buf;
        src.connect(this.ctx.destination);
        const t = this.ctx.currentTime;
        if (this.next < t + 0.005 || this.next > t + 0.1) this.next = t + 0.02;
        src.start(this.next);
        this.next += buf.duration;
      },
      error: () => {},
    });
    this.dec.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  }
  push(a: { seq: number; data: Uint8Array }): void {
    if (this.dec.state !== "configured") return;
    this.dec.decode(new EncodedAudioChunk({ type: "key", timestamp: a.seq * 5000, data: a.data }));
  }
  close(): void { try { this.dec.close(); void this.ctx.close(); } catch { /* closed */ } }
}

// Input.
const HOTKEY = (e: KeyboardEvent) => e.ctrlKey && e.altKey && e.shiftKey;
const held = new Set<number>();

let escTimer: number | undefined;
window.addEventListener("keydown", (e) => {
  const s = session;
  if (e.target === kbd) return;
  // Fullscreen locks the keyboard (Escape goes to the desktop): holding
  // Escape for a second leaves it.
  if (e.code === "Escape" && document.fullscreenElement && !e.repeat) {
    escTimer = window.setTimeout(() => { void document.exitFullscreen(); }, 1000);
  }
  if (!s || s.closed) return;
  if (HOTKEY(e)) {
    const handled = { KeyM: () => toggleLock(), KeyS: () => overlay.classList.toggle("hidden"),
      KeyF: () => toggleFullscreen(),
      KeyT: () => { s.sharp = !s.sharp; void s.send({ t: "SetMode", modes: s.sharp ? Mode.SHARP_TEXT : 0 }); },
      KeyQ: () => { s.quit = true; void s.send({ t: "Bye" }); s.end("Disconnected."); } }[e.code];
    if (handled) { handled(); e.preventDefault(); return; }
  }
  const code = EVDEV[e.code];
  if (code === undefined) return;
  e.preventDefault();
  held.add(code);
  s.sendInput({ t: "Key", code, down: true });
});
window.addEventListener("keyup", (e) => {
  if (e.code === "Escape" && escTimer !== undefined) { clearTimeout(escTimer); escTimer = undefined; }
  if (e.target === kbd) return;
  const code = EVDEV[e.code];
  if (code === undefined || !session) return;
  e.preventDefault();
  if (!held.delete(code)) return;
  session.sendInput({ t: "Key", code, down: false });
});
window.addEventListener("blur", () => {
  held.clear();
  session?.sendInput({ t: "ReleaseAll" });
});

function streamPos(e: MouseEvent): [number, number] | null {
  return view.toRemote(e.clientX, e.clientY);
}

canvas.addEventListener("mousemove", (e) => {
  const s = session;
  edgePan(e);
  if (!s) return;
  if (document.pointerLockElement === canvas) s.motion(e.movementX, e.movementY, null);
  else { const p = streamPos(e); if (p) s.motion(0, 0, p); }
});

// While zoomed in, the picture follows the pointer near the stage's edges.
let panVel = [0, 0];
let panning = false;
function edgePan(e: MouseEvent): void {
  if (!view.pans() || document.pointerLockElement === canvas) { panVel = [0, 0]; return; }
  const r = stage.getBoundingClientRect();
  const edge = 32, speed = 12;
  const v = (p: number, lo: number, hi: number) => (p < lo + edge ? speed : p > hi - edge ? -speed : 0);
  panVel = [v(e.clientX, r.left, r.right), v(e.clientY, r.top, r.bottom)];
  if ((panVel[0] || panVel[1]) && !panning) {
    panning = true;
    const step = () => {
      if (!panVel[0] && !panVel[1]) { panning = false; return; }
      view.panBy(panVel[0], panVel[1]);
      requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }
}
canvas.addEventListener("mouseleave", () => { panVel = [0, 0]; });
const BUTTON = [1, 2, 3, 4, 5];
canvas.addEventListener("mousedown", (e) => {
  if ((e as PointerEvent).pointerType === "touch") return;
  session?.sendInput({ t: "Button", button: BUTTON[e.button] ?? 1, down: true });
  e.preventDefault();
});
canvas.addEventListener("mouseup", (e) => {
  if ((e as PointerEvent).pointerType === "touch") return;
  session?.sendInput({ t: "Button", button: BUTTON[e.button] ?? 1, down: false });
  e.preventDefault();
});
canvas.addEventListener("contextmenu", (e) => e.preventDefault());
stage.addEventListener("wheel", (e) => {
  e.preventDefault();
  // Ctrl+wheel (and a trackpad pinch, which arrives as one): view zoom.
  if (e.ctrlKey) {
    view.setZoom(view.zoom * Math.exp(-e.deltaY * (e.deltaMode ? 0.1 : 0.002)), [e.clientX, e.clientY]);
    return;
  }
  if (!streamPos(e)) return;
  const k = e.deltaMode === 1 ? 120 : e.deltaMode === 2 ? 1200 : 1.2;
  session?.sendInput({ t: "Wheel", dx: Math.round(-e.deltaX * k), dy: Math.round(-e.deltaY * k) });
}, { passive: false });

function toggleLock(): void {
  if (document.pointerLockElement === canvas) document.exitPointerLock();
  else (canvas.requestPointerLock as (o?: object) => Promise<void>)({ unadjustedMovement: true })?.catch?.(() => canvas.requestPointerLock());
}

// Clipboard: send the page's clipboard when focus returns.
window.addEventListener("focus", () => {
  navigator.clipboard?.readText().then((t) => {
    if (t && session && t !== lastClip) {
      lastClip = t;
      void session.send({ t: "Clipboard", kind: ClipKind.TEXT, data: new TextEncoder().encode(t) });
    }
  }).catch(() => {});
});
let lastClip = "";

// Files.
canvas.addEventListener("dragover", (e) => e.preventDefault());
canvas.addEventListener("drop", (e) => {
  e.preventDefault();
  for (const f of Array.from(e.dataTransfer?.files ?? [])) void session?.offer(f);
});
fileInput.addEventListener("change", () => {
  for (const f of Array.from(fileInput.files ?? [])) void session?.offer(f);
  fileInput.value = "";
});

// Connection.
function unb64url(v: string): Uint8Array {
  const s = v.replace(/-/g, "+").replace(/_/g, "/");
  return Uint8Array.from(atob(s + "=".repeat((4 - (s.length % 4)) % 4)), (c) => c.charCodeAt(0));
}

/** The host's current WebTransport certificate hash: it rotates, so ask
 *  the host (same origin); the link's `#c=` is the fallback. */
async function certHash(): Promise<Uint8Array | null> {
  try {
    const r = await fetch("/wt-hash", { cache: "no-store" });
    if (r.ok) return unb64url((await r.json()).hash);
  } catch {
    // An older host, or the page opened from elsewhere.
  }
  const m = location.hash.match(/c=([A-Za-z0-9_-]+)/);
  return m ? unb64url(m[1]) : null;
}

async function connect(pin: string | null, retried = false): Promise<void> {
  const hash = await certHash();
  if (!hash) { say("This link is missing the host's certificate hash (#c=…). Use the link the host printed."); return; }
  if (typeof WebTransport === "undefined") { say("This browser has no WebTransport. Use a current Chrome, Edge or Firefox."); return; }
  say("Connecting…");
  const url = `https://${location.host}/`;
  const wt = new WebTransport(url, { serverCertificateHashes: [{ algorithm: "sha-256", value: hash }], congestionControl: "low-latency" } as WebTransportOptions);
  await wt.ready;
  try {
    await authenticate(wt, pin, hash);
  } catch (e) {
    wt.close();
    if (e instanceof CertChanged && !retried) return connect(pin, true);
    if (e instanceof PinNeeded) { pinBox.hidden = false; say(e.message); pinInput.focus(); return; }
    throw e;
  }
  pinBox.hidden = true;
  const s = new Session(wt);
  session = s;
  lastRequest = "";
  infoSeen = false;
  attempt = 0;
  banner.hidden = true;
  if (startedAudio) s.audio = new AudioPipe();
  await s.start();
  wt.closed.then(
    (info) => {
      s.end("Connection closed.");
      // Another viewer took over: leave it to them.
      if ((info as WebTransportCloseInfo)?.reason === "replaced") { say("Another viewer took over this desktop."); return; }
      if (!s.quit) reconnect();
    },
    () => { s.end("Connection lost."); if (!s.quit) reconnect(); },
  );
}

// Reconnecting: keep the last picture, try again after 1, 2, 4, 8, then
// every 10 seconds (a paired browser needs no PIN).
let attempt = 0;
let reconnectTimer: number | undefined;
function reconnect(): void {
  if (reconnectTimer !== undefined) return;
  const delay = [1, 2, 4, 8][attempt] ?? 10;
  attempt++;
  stats.reconnects++;
  banner.textContent = `Reconnecting… (in ${delay} s)`;
  banner.hidden = false;
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = undefined;
    banner.textContent = "Reconnecting…";
    connect(null).catch(() => reconnect());
  }, delay * 1000);
}

let startedAudio = false;
startBtn.addEventListener("click", () => {
  startedAudio = true;
  startBtn.hidden = true;
  if (session && !session.audio) session.audio = new AudioPipe();
  canvas.focus();
});

pinBox.addEventListener("submit", (e) => {
  e.preventDefault();
  const pin = pinInput.value.trim();
  if (!/^\d{6}$/.test(pin)) { say("The PIN is 6 digits."); return; }
  pinInput.value = "";
  connect(pin).catch((err) => { stats.error = String(err); say(String(err.message ?? err)); });
});

// Display: what the host can do (DisplayInfo) and what we ask for.
let lastRequest = "";
let requestTimer: number | undefined;
let infoSeen = false;

function onDisplayInfo(m: Extract<Msg, { t: "DisplayInfo" }>): void {
  if (m.stream !== 0) return;
  stats.displayKind = m.kind;
  stats.displayNote = m.note;
  const fixed = m.kind === DisplayKind.FIXED;
  $<HTMLFieldSetElement>("res-set").disabled = fixed;
  $<HTMLFieldSetElement>("target-set").hidden = !(m.kind === DisplayKind.KDE || m.kind === DisplayKind.GNOME);
  $<HTMLDivElement>("display-note").textContent = m.note;
  if (m.note) say(m.note);
  if (!infoSeen) {
    infoSeen = true;
    requestDisplay(true);
  }
}

/** The size, refresh, scale and target to ask the host for. */
function wanted(): { width: number; height: number; refresh: number; scalePct: number; target: number } {
  const ratio = cfg.effectiveRatio(settings);
  const [sw, sh] = view.stageSize();
  const [width, height] = settings.mode === "fixed" ? cfg.clampSize(settings.fixedW, settings.fixedH) : cfg.clampSize(sw * ratio, sh * ratio);
  return {
    width, height, refresh: settings.hz, scalePct: Math.round(ratio * 100),
    target: settings.target === "real" ? DisplayTarget.REAL : DisplayTarget.VIRTUAL,
  };
}

/** Asks the host for the display we want, 300 ms after the last change. */
function requestDisplay(now = false): void {
  if (requestTimer !== undefined) clearTimeout(requestTimer);
  requestTimer = window.setTimeout(() => {
    requestTimer = undefined;
    const s = session;
    if (!s || s.closed || s.hostMinor < 1 || stats.displayKind < 0 || stats.displayKind === DisplayKind.FIXED) return;
    const w = wanted();
    const key = JSON.stringify(w);
    if (key === lastRequest) return;
    lastRequest = key;
    void s.send({ t: "DisplayRequest", stream: 0, ...w });
  }, now ? 0 : 300);
}

new ResizeObserver(() => { if (settings.mode === "follow") requestDisplay(); }).observe(stage);
window.matchMedia?.(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener?.("change", () => {
  view.ratio = cfg.effectiveRatio(settings);
  view.layout();
  requestDisplay();
});

// The Display popover.
const displayForm = $<HTMLFormElement>("display");
const displayBtn = $<HTMLButtonElement>("display-btn");
const preset = $<HTMLSelectElement>("preset");
const hzSel = $<HTMLSelectElement>("hz");
for (const [w, h] of cfg.PRESETS) preset.add(new Option(`${w}×${h}`, `${w}x${h}`));
preset.add(new Option("Custom", "custom"));
for (const r of cfg.RATES) hzSel.add(new Option(`${r} Hz`, String(r)));

function showSettings(): void {
  const set = (name: string, v: string) => {
    for (const el of displayForm.querySelectorAll<HTMLInputElement>(`input[name=${name}]`)) el.checked = el.value === v;
  };
  set("mode", settings.mode);
  set("target", settings.target);
  set("view", settings.view);
  const key = `${settings.fixedW}x${settings.fixedH}`;
  preset.value = cfg.PRESETS.some(([w, h]) => `${w}x${h}` === key) ? key : "custom";
  hzSel.value = String(settings.hz);
  $<HTMLInputElement>("cw").value = String(settings.fixedW);
  $<HTMLInputElement>("ch").value = String(settings.fixedH);
  for (const b of displayForm.querySelectorAll<HTMLButtonElement>("[data-ratio]")) {
    b.setAttribute("aria-pressed", String(b.dataset.ratio === String(settings.ratio)));
  }
  $<HTMLButtonElement>("ratio-auto").textContent = `Auto (${cfg.deviceRatio()}×)`;
}

function applySettings(next: Partial<cfg.Settings>): void {
  settings = { ...settings, ...next };
  cfg.save(settings);
  view.mode = settings.view;
  view.ratio = cfg.effectiveRatio(settings);
  view.layout();
  showSettings();
  requestDisplay(true);
}

displayBtn.addEventListener("click", () => {
  displayForm.hidden = !displayForm.hidden;
  displayBtn.setAttribute("aria-expanded", String(!displayForm.hidden));
  showSettings();
});
displayForm.addEventListener("change", (e) => {
  const t = e.target as HTMLInputElement;
  if (t.name === "mode") applySettings({ mode: t.value as cfg.Settings["mode"] });
  else if (t.name === "target") applySettings({ target: t.value as cfg.Settings["target"] });
  else if (t.name === "view") applySettings({ view: t.value as cfg.ViewMode });
  else if (t === preset as unknown as HTMLInputElement && preset.value !== "custom") {
    const [w, h] = preset.value.split("x").map(Number);
    applySettings({ mode: "fixed", fixedW: w, fixedH: h });
  } else if (t === hzSel as unknown as HTMLInputElement) applySettings({ hz: Number(hzSel.value) });
});
$<HTMLButtonElement>("custom-apply").addEventListener("click", () => {
  const [w, h] = cfg.clampSize(Number($<HTMLInputElement>("cw").value), Number($<HTMLInputElement>("ch").value));
  applySettings({ mode: "fixed", fixedW: w, fixedH: h });
});
for (const b of displayForm.querySelectorAll<HTMLButtonElement>("[data-ratio]")) {
  b.addEventListener("click", () => {
    const r = b.dataset.ratio === "auto" ? "auto" : (Number(b.dataset.ratio) as 1 | 1.5 | 2);
    applySettings({ ratio: r });
  });
}
// Close on Escape or a click outside (that click isn't passed on).
document.addEventListener("pointerdown", (e) => {
  if (!displayForm.hidden && !displayForm.contains(e.target as Node) && e.target !== displayBtn && e.target !== $("t-display")) {
    displayForm.hidden = true;
    displayBtn.setAttribute("aria-expanded", "false");
    e.stopPropagation();
    e.preventDefault();
  }
}, true);
displayForm.addEventListener("keydown", (e) => { if (e.key === "Escape") { displayForm.hidden = true; e.stopPropagation(); } });

// Zoom.
const zoomPct = $<HTMLSpanElement>("zoom-pct");
const tZoom = $<HTMLButtonElement>("t-zoom");
view.onChange = () => {
  stats.zoom = view.zoom;
  const t = `${Math.round(view.zoom * 100)}%`;
  zoomPct.textContent = t;
  tZoom.textContent = t;
  canvas.classList.toggle("pixelated", view.scale * cfg.effectiveRatio(settings) >= 2);
};
$<HTMLButtonElement>("zoom-in").addEventListener("click", () => view.setZoom(view.zoom * 1.25));
$<HTMLButtonElement>("zoom-out").addEventListener("click", () => view.setZoom(view.zoom / 1.25));
zoomPct.addEventListener("click", () => view.setZoom(1));
tZoom.addEventListener("click", () => view.setZoom(1));
(window as unknown as { qrView: View }).qrView = view;

// Pause while hidden: no video while nobody can see it.
document.addEventListener("visibilitychange", () => {
  const s = session;
  if (!s || s.closed || s.hostMinor < 1) return;
  void s.send({ t: "Pause", paused: document.hidden });
});

// Fullscreen, with the keyboard locked so browser shortcuts reach the
// desktop (Chromium); hold Escape to leave.
async function toggleFullscreen(): Promise<void> {
  if (document.fullscreenElement) { await document.exitFullscreen(); return; }
  await document.documentElement.requestFullscreen().catch(() => {});
  const kb = (navigator as unknown as { keyboard?: { lock?: () => Promise<void> } }).keyboard;
  await kb?.lock?.().catch(() => {});
}
$<HTMLButtonElement>("fs").addEventListener("click", () => void toggleFullscreen());
document.addEventListener("fullscreenchange", () => {
  if (!document.fullscreenElement) {
    (navigator as unknown as { keyboard?: { unlock?: () => void } }).keyboard?.unlock?.();
    say("Left fullscreen.");
  } else {
    say("Fullscreen: hold Escape to leave.");
  }
});

// Touch: gestures on the picture, a toolbar, and the phone keyboard with a
// strip of extra keys.
const coarse = window.matchMedia?.("(pointer: coarse)").matches ?? false;
const touchbar = $<HTMLDivElement>("touchbar");
const keys = $<HTMLDivElement>("keys");
const kbd = $<HTMLTextAreaElement>("kbd");
touchbar.hidden = !coarse;
const sticky = new Set<number>();
let lockedMods = new Set<number>();
let lastModTap = { code: 0, at: 0 };

function tapKey(code: number): void {
  const s = session;
  if (!s) return;
  const mods = [...sticky];
  for (const m of mods) s.sendInput({ t: "Key", code: m, down: true });
  s.sendInput({ t: "Key", code, down: true });
  s.sendInput({ t: "Key", code, down: false });
  for (const m of mods.reverse()) s.sendInput({ t: "Key", code: m, down: false });
  // One-shot modifiers end after a key; locked ones stay.
  for (const m of [...sticky]) if (!lockedMods.has(m)) sticky.delete(m);
  showMods();
}
function showMods(): void {
  for (const b of keys.querySelectorAll<HTMLButtonElement>("[data-mod]")) {
    const m = Number(b.dataset.mod);
    b.setAttribute("aria-pressed", String(sticky.has(m)));
    b.style.fontWeight = lockedMods.has(m) ? "700" : "";
  }
}
for (const b of keys.querySelectorAll<HTMLButtonElement>("button")) {
  b.addEventListener("pointerdown", (e) => e.preventDefault()); // keep the keyboard up
  b.addEventListener("click", () => {
    if (b.dataset.key) { tapKey(Number(b.dataset.key)); return; }
    const m = Number(b.dataset.mod);
    const now = performance.now();
    if (lastModTap.code === m && now - lastModTap.at < 350) {
      // Double tap: lock until tapped again.
      lockedMods.add(m);
      sticky.add(m);
    } else if (sticky.has(m)) {
      sticky.delete(m);
      lockedMods.delete(m);
    } else {
      sticky.add(m);
    }
    lastModTap = { code: m, at: now };
    showMods();
  });
}

// Letters and digits typed with a sticky modifier go as keys (Ctrl+C);
// everything else as text, whatever the desktop's layout.
const CHAR_KEY: Record<string, number> = {};
for (const ch of "abcdefghijklmnopqrstuvwxyz") CHAR_KEY[ch] = EVDEV[`Key${ch.toUpperCase()}`];
for (const ch of "0123456789") CHAR_KEY[ch] = EVDEV[`Digit${ch}`];
kbd.addEventListener("beforeinput", (e) => {
  const s = session;
  e.preventDefault();
  if (!s) return;
  const ie = e;
  if (ie.inputType === "insertText" || ie.inputType === "insertCompositionText" || ie.inputType === "insertReplacementText") {
    const text = ie.data ?? "";
    const key = CHAR_KEY[text.toLowerCase()];
    if (sticky.size && text.length === 1 && key !== undefined) tapKey(key);
    else if (text) s.sendInput({ t: "Text", text });
  } else if (ie.inputType === "deleteContentBackward") tapKey(EVDEV.Backspace);
  else if (ie.inputType === "deleteContentForward") tapKey(EVDEV.Delete);
  else if (ie.inputType === "insertLineBreak" || ie.inputType === "insertParagraph") tapKey(EVDEV.Enter);
});
function toggleKeyboard(): void {
  if (document.activeElement === kbd) {
    kbd.blur();
    keys.hidden = true;
  } else {
    keys.hidden = false;
    kbd.focus();
  }
}
kbd.addEventListener("blur", () => { keys.hidden = true; });
$<HTMLButtonElement>("t-kbd").addEventListener("click", () => toggleKeyboard());
$<HTMLButtonElement>("t-display").addEventListener("click", () => displayBtn.click());
attachTouch(stage, view, {
  move: (p) => session?.motion(0, 0, p),
  button: (button, down) => session?.sendInput({ t: "Button", button, down }),
  wheel: (dx, dy) => session?.sendInput({ t: "Wheel", dx, dy }),
  toggleKeyboard,
});

showSettings();
void wasm();
const urlPin = new URLSearchParams(location.hash.slice(1)).get("pin");
connect(urlPin).catch((err) => { stats.error = String(err); say(String(err.message ?? err)); });
