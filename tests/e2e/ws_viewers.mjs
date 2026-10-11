// WebSocket viewers (`--ws-listen`) in headless Chrome: two pages share one
// session with the host through the gateway hub.
// A3 two viewers decode at the host's rate, the first drives, Take over
//    swaps the seats and the display follows the new driver, closing the
//    driver promotes the other, the host ran one session throughout.
// A4 a throttled viewer (a tenth of the stream's rate) costs the other nothing: it is skipped to the next
//    keyframe (at most 4 keyframe requests a second) while the other keeps
//    its frame rate.
//
// usage: sg render -c "node tests/e2e/ws_viewers.mjs"   (needs target/debug/qr-host)
// Without the render group the host runs on the test pattern instead of the
// virtual display, and the resize half of A3 is ENV-BLOCKED.
import { spawn } from "node:child_process";
import { accessSync, appendFileSync, constants, existsSync, mkdtempSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import { launch } from "./cdp.mjs";

const root = new URL("../..", import.meta.url).pathname;
const out = mkdtempSync(join(root, "out", "wsview-"));
const results = join(out, "results.txt");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const report = (status, id, what) => { const l = `${status} ${id} — ${what}`; console.log(l); appendFileSync(results, l + "\n"); };
const check = (id, what, ok) => report(ok ? "PASS" : "FAIL", id, what);
const env = { ...process.env, GST_REGISTRY: process.env.GST_REGISTRY || join(out, "gst.bin"), QR_GSO: "0" };
const procs = [];
const run = (cmd, args, log) => {
  const p = spawn(cmd, args, { env, stdio: ["ignore", "pipe", "pipe"] });
  procs.push(p);
  if (log) { p.stdout.on("data", (d) => appendFileSync(log, d)); p.stderr.on("data", (d) => appendFileSync(log, d)); }
  return p;
};
const hostLog = join(out, "host.log");
const WS = 14681;
let gpu = false;
try { accessSync("/dev/dri/renderD128", constants.W_OK); gpu = true; } catch { /* no render group */ }

function startHost() {
  const source = gpu ? ["--source", "vdisplay:weston-simple-egl", "--encoder", "vah264lpenc"] : ["--source", "test", "--encoder", "x264enc"];
  return run(join(root, "target/debug/qr-host"), ["--listen", "127.0.0.1:9492", "--ws-listen", `127.0.0.1:${WS}`,
    "--config-dir", join(out, "h"), "--no-mdns", ...source, "--size", "1600x900", "--fps", "60", "--audio", "none", "--input", "none"], hostLog);
}
const status = async () => (await fetch(`http://127.0.0.1:${WS}/status`)).json();
const hostLines = (re) => (existsSync(hostLog) ? readFileSync(hostLog, "utf8").split("\n").filter((l) => re.test(l)).length : 0);

// The viewer page: the extension's wire in a few lines (binary channels,
// Hello with this browser's decoders, feedback every 50 ms, WebCodecs).
const PAGE = `<!doctype html><title>ws viewer</title><canvas id=c></canvas><script type=module>
const enc = new TextEncoder();
const str = (s) => { const b = enc.encode(s); const o = new Uint8Array(4 + b.length); new DataView(o.buffer).setUint32(0, b.length, true); o.set(b, 4); return o; };
const cat = (...a) => { const n = a.reduce((s, x) => s + x.length, 0); const o = new Uint8Array(n); let p = 0; for (const x of a) { o.set(x, p); p += x.length; } return o; };
const u16 = (v) => { const o = new Uint8Array(2); new DataView(o.buffer).setUint16(0, v, true); return o; };
const u32 = (v) => { const o = new Uint8Array(4); new DataView(o.buffer).setUint32(0, v, true); return o; };
const i32 = (v) => { const o = new Uint8Array(4); new DataView(o.buffer).setInt32(0, v, true); return o; };
const st = { frames: 0, bytes: 0, decoded: 0, key: 0, keyTimes: [], times: [], seat: null, seats: 0, tags: [], width: 0, height: 0, codec: "", error: "", closed: false, viewerId: location.hash.slice(1) || "v" };
window.qrStats = st;
const ws = new WebSocket("ws://127.0.0.1:" + (new URLSearchParams(location.search).get("port") || ${WS}) + "/ws");
ws.binaryType = "arraybuffer";
window.qrSend = (b) => ws.send(b);
window.qrClaim = () => ws.send(cat(new Uint8Array([5, 2]), str(st.viewerId)));
window.qrDisplay = (w, h) => ws.send(cat(new Uint8Array([0, 28, 0]), u16(w), u16(h), u16(60), u16(100), new Uint8Array([0])));
let dec = null, codecString = "", needKey = true, seq = 0;
const delays = [];
function configure(au) {
  let s = "avc1.640033";
  for (let i = 0; i + 7 < au.length; i++) if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1 && (au[i + 3] & 0x1f) === 7) {
    const h = (x) => x.toString(16).padStart(2, "0"); s = "avc1." + h(au[i + 4]) + h(au[i + 5]) + h(au[i + 6]); break; }
  if (dec && s === codecString && dec.state === "configured") return;
  dec?.close();
  codecString = s;
  dec = new VideoDecoder({ output: (f) => { st.decoded++; st.times.push(performance.now()); f.close(); }, error: (e) => { st.error = String(e); needKey = true; } });
  dec.configure({ codec: s, optimizeForLatency: true, hardwareAcceleration: "no-preference" });
}
ws.onopen = async () => {
  const caps = [];
  for (const [codec, s] of [[1, "avc1.640033"]]) {
    const r = await VideoDecoder.isConfigSupported({ codec: s }).catch(() => ({ supported: false }));
    if (r.supported) caps.push(cat(new Uint8Array([codec, 0]), u16(4096), u16(2304), new Uint8Array([0])));
  }
  ws.send(cat(new Uint8Array([0, 1]), u16(1), u16(2), str("perch:" + st.viewerId), str("chrome"), u16(0), u16(0), u16(0), u16(caps.length), ...caps));
  setInterval(() => {
    seq++;
    const d = delays.splice(0);
    const mn = d.length ? Math.min(...d) : 0, mx = d.length ? Math.max(...d) : 0;
    ws.send(cat(new Uint8Array([0, 8]), u32(seq), u16(50), u32(0), u32(0), u32(0), u32(0), u16(d.length), u16(0), i32(mn), i32(mx), i32(0), u32(0), u32(0)));
  }, 50);
};
ws.onmessage = (e) => {
  const b = new Uint8Array(e.data);
  if (b[0] === 2) {
    st.frames++;
    st.bytes += b.length;
    const dv = new DataView(b.buffer);
    const key = (b[3] & 1) === 1, len = dv.getUint32(8, true), sendUs = Number(dv.getBigUint64(36, true));
    const au = b.subarray(44, 44 + len);
    delays.push(Math.round(performance.now() * 1000 - sendUs) | 0);
    if (key) { st.key++; st.keyTimes.push(performance.now()); needKey = false; configure(au); }
    if (needKey || !dec) return;
    try { dec.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: st.frames, data: au })); } catch (err) { st.error = String(err); needKey = true; }
  } else if (b[0] === 0) {
    st.tags.push(b[1]);
    const dv = new DataView(b.buffer);
    if (b[1] === 14) { st.codec = ["", "h264", "h265", "av1"][b[4]]; st.width = dv.getUint16(6, true); st.height = dv.getUint16(8, true); needKey = true; }
    if (b[1] === 26) st.error = "host error " + dv.getUint16(2, true);
  } else if (b[0] === 5 && b[1] === 1) {
    const dv = new DataView(b.buffer);
    st.seat = { driver: b[2] === 1, scalePct: dv.getUint16(3, true), driverRatioPct: dv.getUint16(5, true), viewers: dv.getUint16(7, true), cursorInPicture: b[9] === 1 };
    st.seats++;
  }
};
ws.onclose = () => { st.closed = true; };
</script>`;

const server = createServer((_req, res) => { res.setHeader("content-type", "text/html"); res.end(PAGE); });
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const pageUrl = (id, port = WS) => `http://127.0.0.1:${server.address().port}/?port=${port}#${id}`;

// A TCP relay that lets `bytesPerSec` through towards the browser (CDP's
// network throttling leaves WebSockets alone), so the hub's socket to this
// viewer backs up as a slow link would.
import { createServer as createTcpServer, connect as tcpConnect } from "node:net";
function slowRelay(bytesPerSec) {
  const srv = createTcpServer((client) => {
    const up = tcpConnect(WS, "127.0.0.1");
    client.pipe(up);
    let tokens = bytesPerSec / 10;
    const pending = [];
    const timer = setInterval(() => {
      tokens = Math.min(bytesPerSec / 10, tokens + bytesPerSec / 100);
      while (pending.length && tokens > 0) {
        const chunk = pending[0];
        const n = Math.min(chunk.length, Math.floor(tokens));
        if (n <= 0) break;
        client.write(chunk.subarray(0, n));
        tokens -= n;
        if (n === chunk.length) pending.shift(); else pending[0] = chunk.subarray(n);
      }
      if (pending.length < 8) up.resume();
    }, 10);
    up.on("data", (d) => { pending.push(d); if (pending.length >= 8) up.pause(); });
    const end = () => { clearInterval(timer); client.destroy(); up.destroy(); };
    up.on("close", end); client.on("close", end); up.on("error", end); client.on("error", end);
  });
  return new Promise((r) => srv.listen(0, "127.0.0.1", () => r({ port: srv.address().port, close: () => srv.close() })));
}

// Decoded frames per second over the last `secs` seconds, from the page.
const fps = async (page, secs) => {
  const before = await page.eval("window.qrStats.decoded");
  await sleep(secs * 1000);
  const after = await page.eval("window.qrStats.decoded");
  return (after - before) / secs;
};
const keyRate = (times, since) => times.filter((t) => t >= since).length;

const host = startHost();
const pages = [];
try {
  for (let i = 0; i < 100 && !existsSync(hostLog); i++) await sleep(100);
  for (let i = 0; i < 150; i++) { try { await status(); break; } catch { await sleep(100); } }
  const a = await launch({ port: 9341 });
  pages.push(a);
  await a.goto(pageUrl("A"));
  await a.waitFor("window.qrStats && window.qrStats.decoded > 10", 20000).catch(() => {});
  const b = await launch({ port: 9342 });
  pages.push(b);
  await b.goto(pageUrl("B"));
  await b.waitFor("window.qrStats && window.qrStats.decoded > 10", 20000).catch(() => {});
  const [fa, fb] = await Promise.all([fps(a, 3), fps(b, 3)]);
  const sa = await a.eval("window.qrStats.seat"), sb = await b.eval("window.qrStats.seat");
  const sizeA = await a.eval("[window.qrStats.width, window.qrStats.height, window.qrStats.codec]");
  const errs = [await a.eval("window.qrStats.error"), await b.eval("window.qrStats.error")];
  const ok1 = fa >= 45 && fb >= 45 && sa?.driver === true && sb?.driver === false && sa.viewers === 2 && sb.viewers === 2;
  check("A3a", `two viewers: A ${fa.toFixed(1)} fps (driver ${sa?.driver}), B ${fb.toFixed(1)} fps (driver ${sb?.driver}), ${sa?.viewers} viewers, stream ${sizeA.join("×")}${errs.some(Boolean) ? ", errors " + errs.join(" | ") : ""}`, ok1);

  // Take over, then a smaller display from the new driver.
  await b.eval("window.qrClaim()");
  await b.waitFor("window.qrStats.seat && window.qrStats.seat.driver", 3000).catch(() => {});
  // A's seat message rides behind its queued frames.
  await a.waitFor("window.qrStats.seat && !window.qrStats.seat.driver", 3000).catch(() => {});
  const swapped = [await a.eval("window.qrStats.seat.driver"), await b.eval("window.qrStats.seat.driver")];
  let resized = null, took = 0;
  if (gpu) {
    const t0 = Date.now();
    await b.eval("window.qrDisplay(1280, 720)");
    for (; Date.now() - t0 < 6000; await sleep(100)) { const s = await status(); if (s.width === 1280 && s.height === 720) { resized = true; break; } }
    took = Date.now() - t0;
    if (resized === null) resized = false;
  }
  check("A3b", `Take over: A driver ${swapped[0]}, B driver ${swapped[1]}; ` + (gpu ? `B's 1280×720 request applied in ${took} ms: ${resized}` : "resize check skipped (test source)"),
    swapped[0] === false && swapped[1] === true && (!gpu || resized));
  if (!gpu) report("ENV-BLOCKED", "A3b-resize", "the display follows the new driver needs the virtual display (render group)");

  // The driver (B) leaves: A takes the wheel; one session all along.
  await b.close();
  pages.pop();
  await a.waitFor("window.qrStats.seat && window.qrStats.seat.driver && window.qrStats.seat.viewers === 1", 5000).catch(() => {});
  const promoted = await a.eval("window.qrStats.seat");
  const fa2 = await fps(a, 2);
  const sessions = hostLines(/session start/);
  check("A3c", `driver left: A driver ${promoted?.driver}, ${promoted?.viewers} viewer, ${fa2.toFixed(1)} fps; host sessions started: ${sessions}`,
    promoted?.driver === true && promoted?.viewers === 1 && fa2 >= 45 && sessions === 1);

  // A4: a follower behind a 1 Mbit/s link. The socket buffers hold a
  // second or two first; the measurement starts once they are full.
  const bytes0 = await a.eval("window.qrStats.bytes");
  const base = await fps(a, 3);
  const kbps = Math.round(((await a.eval("window.qrStats.bytes")) - bytes0) * 8 / 3000);
  // A tenth of the stream's own rate, at least 100 kbit/s.
  const linkBps = Math.max(12500, Math.round((kbps * 1000) / 8 / 10));
  const relay = await slowRelay(linkBps);
  const c = await launch({ port: 9343 });
  pages.push(c);
  await c.goto(pageUrl("C", relay.port));
  await c.waitFor("window.qrStats && window.qrStats.frames > 10", 20000).catch(() => {});
  await sleep(4000);
  const since = await c.eval("performance.now()");
  const framesBefore = await c.eval("window.qrStats.frames");
  const [during, cFps] = await Promise.all([fps(a, 5), fps(c, 5)]);
  const keys = await c.eval(`window.qrStats.keyTimes.filter((t) => t >= ${since}).length`);
  const cFrames = (await c.eval("window.qrStats.frames")) - framesBefore;
  relay.close();
  const keysPerSec = keys / 5;
  check("A4", `stream ${kbps} kbit/s; viewer C behind a ${Math.round(linkBps * 8 / 1000)} kbit/s link: A ${base.toFixed(1)} → ${during.toFixed(1)} fps (${((during / base) * 100).toFixed(0)}%), C ${cFps.toFixed(1)} fps with ${keys} keyframes in 5 s (${keysPerSec.toFixed(1)}/s), ${cFrames} frames received in that time`,
    during >= base * 0.9 && keysPerSec <= 4 && cFps < base * 0.5);
} catch (e) {
  report("FAIL", "WS", `script error: ${e.stack || e}`);
} finally {
  for (const p of pages) await p.close().catch(() => {});
  server.close();
  host.kill("SIGTERM");
  await sleep(500);
  for (const p of procs) p.kill("SIGKILL");
}
const lines = readFileSync(results, "utf8");
process.exit(/^FAIL/m.test(lines) ? 1 : 0);
