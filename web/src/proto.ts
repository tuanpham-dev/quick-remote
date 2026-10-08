// QuickRemote wire protocol v1 for the web client: the same byte layout as
// crates/qr-proto (see docs/protocol.md), checked against
// conformance/vectors.json by test/vectors.test.ts.

export const PROTO_MAJOR = 1;
export const PROTO_MINOR = 1;

export class Writer {
  private buf = new Uint8Array(64);
  private len = 0;
  private dv = new DataView(this.buf.buffer);

  private room(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let cap = this.buf.length * 2;
    while (cap < this.len + n) cap *= 2;
    const b = new Uint8Array(cap);
    b.set(this.buf.subarray(0, this.len));
    this.buf = b;
    this.dv = new DataView(b.buffer);
  }
  u8(v: number): this { this.room(1); this.dv.setUint8(this.len, v); this.len += 1; return this; }
  bool(v: boolean): this { return this.u8(v ? 1 : 0); }
  i8(v: number): this { this.room(1); this.dv.setInt8(this.len, v); this.len += 1; return this; }
  u16(v: number): this { this.room(2); this.dv.setUint16(this.len, v, true); this.len += 2; return this; }
  i16(v: number): this { this.room(2); this.dv.setInt16(this.len, v, true); this.len += 2; return this; }
  u32(v: number): this { this.room(4); this.dv.setUint32(this.len, v, true); this.len += 4; return this; }
  i32(v: number): this { this.room(4); this.dv.setInt32(this.len, v, true); this.len += 4; return this; }
  u64(v: bigint): this { this.room(8); this.dv.setBigUint64(this.len, v, true); this.len += 8; return this; }
  i64(v: bigint): this { this.room(8); this.dv.setBigInt64(this.len, v, true); this.len += 8; return this; }
  raw(v: Uint8Array): this { this.room(v.length); this.buf.set(v, this.len); this.len += v.length; return this; }
  bytes(v: Uint8Array): this { return this.u32(v.length).raw(v); }
  str(v: string): this { return this.bytes(new TextEncoder().encode(v)); }
  done(): Uint8Array { return this.buf.slice(0, this.len); }
}

export class Reader {
  private pos = 0;
  private buf: Uint8Array;
  private dv: DataView;
  constructor(buf: Uint8Array) {
    this.buf = buf;
    this.dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  }
  private need(n: number): void { if (this.pos + n > this.buf.length) throw new Error("message truncated"); }
  remaining(): number { return this.buf.length - this.pos; }
  finish(): void { if (this.remaining() !== 0) throw new Error(`${this.remaining()} trailing bytes`); }
  u8(): number { this.need(1); return this.dv.getUint8(this.pos++); }
  bool(): boolean { return this.u8() !== 0; }
  i8(): number { this.need(1); return this.dv.getInt8(this.pos++); }
  u16(): number { this.need(2); const v = this.dv.getUint16(this.pos, true); this.pos += 2; return v; }
  i16(): number { this.need(2); const v = this.dv.getInt16(this.pos, true); this.pos += 2; return v; }
  u32(): number { this.need(4); const v = this.dv.getUint32(this.pos, true); this.pos += 4; return v; }
  i32(): number { this.need(4); const v = this.dv.getInt32(this.pos, true); this.pos += 4; return v; }
  u64(): bigint { this.need(8); const v = this.dv.getBigUint64(this.pos, true); this.pos += 8; return v; }
  i64(): bigint { this.need(8); const v = this.dv.getBigInt64(this.pos, true); this.pos += 8; return v; }
  take(n: number): Uint8Array { this.need(n); const s = this.buf.subarray(this.pos, this.pos + n); this.pos += n; return s; }
  rest(): Uint8Array { return this.take(this.remaining()); }
  bytes(): Uint8Array { const n = this.u32(); if (n > 16 * 1024 * 1024) throw new Error("field too long"); return this.take(n).slice(); }
  str(): string { return new TextDecoder("utf-8", { fatal: true }).decode(this.bytes()); }
}

export const Codec = { H264: 1, H265: 2, AV1: 3 } as const;
export const Mode = { RELATIVE_MOUSE: 1, SHARP_TEXT: 2, MIC: 4, CLIPBOARD: 8, AUDIO: 16 } as const;
export const ClipKind = { TEXT: 1, PNG: 2 } as const;

export interface CodecCap { codec: number; hw: boolean; maxWidth: number; maxHeight: number; chroma444: boolean }
export interface Monitor { id: number; name: string; width: number; height: number; refreshMhz: number; x: number; y: number; primary: boolean }
export interface Feedback {
  seq: number; windowMs: number; bytesRx: number; shardsRx: number; shardsLost: number; shardsRepaired: number;
  framesRx: number; framesLost: number; delayMinUs: number; delayMaxUs: number; delaySlope: number; decodeUs: number; presentUs: number;
}
export interface HostStats {
  stream: number; bitrateKbps: number; fps: number; parityPct: number; profile: number; encoder: string;
  captureUs: number; encodeUs: number; droppedByBackpressure: number;
}

export type Msg =
  | { t: "Hello"; major: number; minor: number; name: string; os: string; wantWidth: number; wantHeight: number; wantRefresh: number; decoders: CodecCap[] }
  | { t: "HelloAck"; major: number; minor: number; name: string; hostId: string; needPin: boolean; rendezvous: string }
  | { t: "PairSpake"; data: Uint8Array }
  | { t: "PairConfirm"; mac: Uint8Array }
  | { t: "SessionStart"; profile: number; path: number; modes: number }
  | { t: "Ping"; seq: number; tClient: bigint }
  | { t: "Pong"; seq: number; tClient: bigint; tHost: bigint }
  | ({ t: "Feedback" } & Feedback)
  | { t: "FrameLost"; stream: number; frame: number }
  | { t: "KeyframeReq"; stream: number }
  | { t: "SetMode"; modes: number }
  | { t: "MonitorList"; monitors: Monitor[] }
  | { t: "SelectMonitor"; stream: number; monitor: number }
  | { t: "StreamStarted"; stream: number; monitor: number; codec: number; chroma444: boolean; width: number; height: number; fps: number }
  | { t: "StreamStop"; stream: number }
  | { t: "Resize"; stream: number; width: number; height: number; refresh: number }
  | { t: "Clipboard"; kind: number; data: Uint8Array }
  | { t: "FileOffer"; id: number; name: string; size: bigint; sha256: Uint8Array }
  | { t: "FileAccept"; id: number }
  | { t: "FileCancel"; id: number; reason: string }
  | { t: "FileDone"; id: number; ok: boolean }
  | { t: "PadAttach"; idx: number; kind: number }
  | { t: "PadDetach"; idx: number }
  | { t: "Rumble"; idx: number; strong: number; weak: number; ms: number }
  | ({ t: "HostStats" } & HostStats)
  | { t: "Error"; code: number; text: string }
  | { t: "Bye" }
  | { t: "DisplayRequest"; stream: number; width: number; height: number; refresh: number; scalePct: number; target: number }
  | { t: "DisplayInfo"; stream: number; kind: number; width: number; height: number; scalePct: number; note: string }
  | { t: "Pause"; paused: boolean }
  | { t: "Unknown"; tag: number };

/** `DisplayInfo.kind`. */
export const DisplayKind = { FIXED: 0, OWN: 1, KDE: 2, GNOME: 3 } as const;
/** `DisplayRequest.target`. */
export const DisplayTarget = { VIRTUAL: 0, REAL: 1 } as const;

const TAG: Record<Msg["t"], number> = {
  Hello: 1, HelloAck: 2, PairSpake: 3, PairConfirm: 4, SessionStart: 5, Ping: 6, Pong: 7, Feedback: 8,
  FrameLost: 9, KeyframeReq: 10, SetMode: 11, MonitorList: 12, SelectMonitor: 13, StreamStarted: 14,
  StreamStop: 15, Resize: 16, Clipboard: 17, FileOffer: 18, FileAccept: 19, FileCancel: 20, FileDone: 21,
  PadAttach: 22, PadDetach: 23, Rumble: 24, HostStats: 25, Error: 26, Bye: 27,
  DisplayRequest: 28, DisplayInfo: 29, Pause: 30, Unknown: 0,
};

export function encodeMsg(m: Msg): Uint8Array {
  const w = new Writer().u8(m.t === "Unknown" ? m.tag : TAG[m.t]);
  switch (m.t) {
    case "Hello":
      w.u16(m.major).u16(m.minor).str(m.name).str(m.os).u16(m.wantWidth).u16(m.wantHeight).u16(m.wantRefresh);
      w.u16(m.decoders.length);
      for (const c of m.decoders) w.u8(c.codec).bool(c.hw).u16(c.maxWidth).u16(c.maxHeight).bool(c.chroma444);
      break;
    case "HelloAck": w.u16(m.major).u16(m.minor).str(m.name).str(m.hostId).bool(m.needPin).str(m.rendezvous); break;
    case "PairSpake": w.bytes(m.data); break;
    case "PairConfirm": w.raw(m.mac); break;
    case "SessionStart": w.u8(m.profile).u8(m.path).u32(m.modes); break;
    case "Ping": w.u32(m.seq).u64(m.tClient); break;
    case "Pong": w.u32(m.seq).u64(m.tClient).u64(m.tHost); break;
    case "Feedback":
      w.u32(m.seq).u16(m.windowMs).u32(m.bytesRx).u32(m.shardsRx).u32(m.shardsLost).u32(m.shardsRepaired);
      w.u16(m.framesRx).u16(m.framesLost).i32(m.delayMinUs).i32(m.delayMaxUs).i32(m.delaySlope).u32(m.decodeUs).u32(m.presentUs);
      break;
    case "FrameLost": w.u8(m.stream).u32(m.frame); break;
    case "KeyframeReq": w.u8(m.stream); break;
    case "SetMode": w.u32(m.modes); break;
    case "MonitorList":
      w.u16(m.monitors.length);
      for (const x of m.monitors) w.u8(x.id).str(x.name).u16(x.width).u16(x.height).u32(x.refreshMhz).i32(x.x).i32(x.y).bool(x.primary);
      break;
    case "SelectMonitor": w.u8(m.stream).u8(m.monitor); break;
    case "StreamStarted": w.u8(m.stream).u8(m.monitor).u8(m.codec).bool(m.chroma444).u16(m.width).u16(m.height).u16(m.fps); break;
    case "StreamStop": w.u8(m.stream); break;
    case "Resize": w.u8(m.stream).u16(m.width).u16(m.height).u16(m.refresh); break;
    case "DisplayRequest": w.u8(m.stream).u16(m.width).u16(m.height).u16(m.refresh).u16(m.scalePct).u8(m.target); break;
    case "DisplayInfo": w.u8(m.stream).u8(m.kind).u16(m.width).u16(m.height).u16(m.scalePct).str(m.note); break;
    case "Pause": w.bool(m.paused); break;
    case "Unknown": break;
    case "Clipboard": w.u8(m.kind).bytes(m.data); break;
    case "FileOffer": w.u32(m.id).str(m.name).u64(m.size).raw(m.sha256); break;
    case "FileAccept": w.u32(m.id); break;
    case "FileCancel": w.u32(m.id).str(m.reason); break;
    case "FileDone": w.u32(m.id).bool(m.ok); break;
    case "PadAttach": w.u8(m.idx).u8(m.kind); break;
    case "PadDetach": w.u8(m.idx); break;
    case "Rumble": w.u8(m.idx).u16(m.strong).u16(m.weak).u16(m.ms); break;
    case "HostStats":
      w.u8(m.stream).u32(m.bitrateKbps).u16(m.fps).u8(m.parityPct).u8(m.profile).str(m.encoder);
      w.u32(m.captureUs).u32(m.encodeUs).u32(m.droppedByBackpressure);
      break;
    case "Error": w.u16(m.code).str(m.text); break;
    case "Bye": break;
  }
  return w.done();
}

export function decodeMsg(b: Uint8Array): Msg {
  const r = new Reader(b);
  const tag = r.u8();
  let m: Msg;
  switch (tag) {
    case 1: {
      const major = r.u16(), minor = r.u16(), name = r.str(), os = r.str();
      const wantWidth = r.u16(), wantHeight = r.u16(), wantRefresh = r.u16();
      const n = r.u16();
      const decoders: CodecCap[] = [];
      for (let i = 0; i < n; i++) decoders.push({ codec: r.u8(), hw: r.bool(), maxWidth: r.u16(), maxHeight: r.u16(), chroma444: r.bool() });
      m = { t: "Hello", major, minor, name, os, wantWidth, wantHeight, wantRefresh, decoders };
      break;
    }
    case 2: m = { t: "HelloAck", major: r.u16(), minor: r.u16(), name: r.str(), hostId: r.str(), needPin: r.bool(), rendezvous: r.str() }; break;
    case 3: m = { t: "PairSpake", data: r.bytes() }; break;
    case 4: m = { t: "PairConfirm", mac: r.take(32).slice() }; break;
    case 5: m = { t: "SessionStart", profile: r.u8(), path: r.u8(), modes: r.u32() }; break;
    case 6: m = { t: "Ping", seq: r.u32(), tClient: r.u64() }; break;
    case 7: m = { t: "Pong", seq: r.u32(), tClient: r.u64(), tHost: r.u64() }; break;
    case 8:
      m = {
        t: "Feedback", seq: r.u32(), windowMs: r.u16(), bytesRx: r.u32(), shardsRx: r.u32(), shardsLost: r.u32(),
        shardsRepaired: r.u32(), framesRx: r.u16(), framesLost: r.u16(), delayMinUs: r.i32(), delayMaxUs: r.i32(),
        delaySlope: r.i32(), decodeUs: r.u32(), presentUs: r.u32(),
      };
      break;
    case 9: m = { t: "FrameLost", stream: r.u8(), frame: r.u32() }; break;
    case 10: m = { t: "KeyframeReq", stream: r.u8() }; break;
    case 11: m = { t: "SetMode", modes: r.u32() }; break;
    case 12: {
      const n = r.u16();
      const monitors: Monitor[] = [];
      for (let i = 0; i < n; i++) monitors.push({ id: r.u8(), name: r.str(), width: r.u16(), height: r.u16(), refreshMhz: r.u32(), x: r.i32(), y: r.i32(), primary: r.bool() });
      m = { t: "MonitorList", monitors };
      break;
    }
    case 13: m = { t: "SelectMonitor", stream: r.u8(), monitor: r.u8() }; break;
    case 14: m = { t: "StreamStarted", stream: r.u8(), monitor: r.u8(), codec: r.u8(), chroma444: r.bool(), width: r.u16(), height: r.u16(), fps: r.u16() }; break;
    case 15: m = { t: "StreamStop", stream: r.u8() }; break;
    case 16: m = { t: "Resize", stream: r.u8(), width: r.u16(), height: r.u16(), refresh: r.u16() }; break;
    case 17: m = { t: "Clipboard", kind: r.u8(), data: r.bytes() }; break;
    case 18: m = { t: "FileOffer", id: r.u32(), name: r.str(), size: r.u64(), sha256: r.take(32).slice() }; break;
    case 19: m = { t: "FileAccept", id: r.u32() }; break;
    case 20: m = { t: "FileCancel", id: r.u32(), reason: r.str() }; break;
    case 21: m = { t: "FileDone", id: r.u32(), ok: r.bool() }; break;
    case 22: m = { t: "PadAttach", idx: r.u8(), kind: r.u8() }; break;
    case 23: m = { t: "PadDetach", idx: r.u8() }; break;
    case 24: m = { t: "Rumble", idx: r.u8(), strong: r.u16(), weak: r.u16(), ms: r.u16() }; break;
    case 25:
      m = {
        t: "HostStats", stream: r.u8(), bitrateKbps: r.u32(), fps: r.u16(), parityPct: r.u8(), profile: r.u8(),
        encoder: r.str(), captureUs: r.u32(), encodeUs: r.u32(), droppedByBackpressure: r.u32(),
      };
      break;
    case 26: m = { t: "Error", code: r.u16(), text: r.str() }; break;
    case 27: m = { t: "Bye" }; break;
    case 28: m = { t: "DisplayRequest", stream: r.u8(), width: r.u16(), height: r.u16(), refresh: r.u16(), scalePct: r.u16(), target: r.u8() }; break;
    case 29: m = { t: "DisplayInfo", stream: r.u8(), kind: r.u8(), width: r.u16(), height: r.u16(), scalePct: r.u16(), note: r.str() }; break;
    case 30: m = { t: "Pause", paused: r.bool() }; break;
    // From a newer host: skip it whole.
    default: r.rest(); return { t: "Unknown", tag };
  }
  r.finish();
  return m;
}

// Input-stream events.
export const Pen = { IN_RANGE: 1, CONTACT: 2, ERASER: 4, BARREL: 8 } as const;
export type InputEvent =
  | { t: "Key"; code: number; down: boolean }
  | { t: "Button"; button: number; down: boolean }
  | { t: "Abs"; stream: number; x: number; y: number }
  | { t: "Wheel"; dx: number; dy: number }
  | { t: "Text"; text: string }
  | { t: "ReleaseAll" }
  | { t: "Pen"; stream: number; x: number; y: number; pressure: number; tiltX: number; tiltY: number; flags: number };

export function encodeInput(e: InputEvent): Uint8Array {
  const w = new Writer();
  switch (e.t) {
    case "Key": w.u8(1).u16(e.code).bool(e.down); break;
    case "Button": w.u8(2).u8(e.button).bool(e.down); break;
    case "Abs": w.u8(3).u8(e.stream).u16(e.x).u16(e.y); break;
    case "Wheel": w.u8(4).i16(e.dx).i16(e.dy); break;
    case "Text": w.u8(5).str(e.text); break;
    case "ReleaseAll": w.u8(6); break;
    case "Pen": w.u8(7).u8(e.stream).u32(e.x).u32(e.y).u16(e.pressure).i8(e.tiltX).i8(e.tiltY).u8(e.flags); break;
  }
  return w.done();
}

export function decodeInput(b: Uint8Array): InputEvent {
  const r = new Reader(b);
  let e: InputEvent;
  const tag = r.u8();
  switch (tag) {
    case 1: e = { t: "Key", code: r.u16(), down: r.bool() }; break;
    case 2: e = { t: "Button", button: r.u8(), down: r.bool() }; break;
    case 3: e = { t: "Abs", stream: r.u8(), x: r.u16(), y: r.u16() }; break;
    case 4: e = { t: "Wheel", dx: r.i16(), dy: r.i16() }; break;
    case 5: e = { t: "Text", text: r.str() }; break;
    case 6: e = { t: "ReleaseAll" }; break;
    case 7: e = { t: "Pen", stream: r.u8(), x: r.u32(), y: r.u32(), pressure: r.u16(), tiltX: r.i8(), tiltY: r.i8(), flags: r.u8() }; break;
    default: throw new Error(`unknown input tag ${tag}`);
  }
  r.finish();
  return e;
}

// Datagrams.
export const Kind = { VIDEO: 1, AUDIO: 2, MIC: 3, MOTION: 4, PAD: 5, PROBE: 6 } as const;

export interface Motion { seq: number; relX: bigint; relY: bigint; hasAbs: boolean; stream: number; absX: number; absY: number; tClientUs: bigint }
export function encodeMotion(m: Motion): Uint8Array {
  return new Writer().u8(Kind.MOTION).u32(m.seq).i64(m.relX).i64(m.relY).bool(m.hasAbs).u8(m.stream).u16(m.absX).u16(m.absY).u64(m.tClientUs).done();
}
export function decodeMotion(b: Uint8Array): Motion {
  const r = new Reader(b);
  if (r.u8() !== Kind.MOTION) throw new Error("not motion");
  const m = { seq: r.u32(), relX: r.i64(), relY: r.i64(), hasAbs: r.bool(), stream: r.u8(), absX: r.u16(), absY: r.u16(), tClientUs: r.u64() };
  r.finish();
  return m;
}

export interface PadState { idx: number; seq: number; buttons: number; axes: number[] }
export function encodePad(p: PadState): Uint8Array {
  const w = new Writer().u8(Kind.PAD).u8(p.idx).u32(p.seq).u32(p.buttons);
  for (let i = 0; i < 6; i++) w.i16(p.axes[i] ?? 0);
  return w.done();
}
export function decodePad(b: Uint8Array): PadState {
  const r = new Reader(b);
  if (r.u8() !== Kind.PAD) throw new Error("not a pad");
  const idx = r.u8(), seq = r.u32(), buttons = r.u32();
  const axes: number[] = [];
  for (let i = 0; i < 6; i++) axes.push(r.i16());
  r.finish();
  return { idx, seq, buttons, axes };
}

export interface AudioPacket { mic: boolean; seq: number; tsUs: bigint; data: Uint8Array }
export function encodeAudio(a: AudioPacket): Uint8Array {
  return new Writer().u8(a.mic ? Kind.MIC : Kind.AUDIO).u32(a.seq).u64(a.tsUs).raw(a.data).done();
}
export function decodeAudio(b: Uint8Array): AudioPacket {
  const r = new Reader(b);
  const k = r.u8();
  if (k !== Kind.AUDIO && k !== Kind.MIC) throw new Error("not audio");
  return { mic: k === Kind.MIC, seq: r.u32(), tsUs: r.u64(), data: r.rest().slice() };
}

/// u32 little-endian length prefix, as on every stream.
export function frame(body: Uint8Array): Uint8Array {
  const out = new Uint8Array(4 + body.length);
  new DataView(out.buffer).setUint32(0, body.length, true);
  out.set(body, 4);
  return out;
}
