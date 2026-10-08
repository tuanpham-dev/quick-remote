// Display controls in the web client, headless Chrome against a host on the
// sway virtual display (the probe app as the desktop):
// A1 follow window, A2 fixed resolution and view modes, A8 zoom/pan and
// pointer mapping, A9 settings kept, A10 reconnect, A11 pause while
// hidden, A12 touch gestures, A13 sticky Ctrl on the keys strip, A14
// fullscreen keyboard lock and hold-Escape, A21 a fixed-size (X11) desktop.
//
// usage: sg render -c "node tests/e2e/web_display.mjs"   (needs target/debug/qr-host)
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, appendFileSync } from "node:fs";
import { join } from "node:path";
import { launch } from "./cdp.mjs";

const root = new URL("../..", import.meta.url).pathname;
const here = new URL(".", import.meta.url).pathname;
const out = mkdtempSync(join(root, "out", "webdisp-"));
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
const probe = join(out, "probe.jsonl");
const events = () => existsSync(probe) ? readFileSync(probe, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [];
const hostLog = join(out, "host.log");

function startHost(source, extra = []) {
  return run(join(root, "target/debug/qr-host"), ["--listen", "127.0.0.1:9490", "--web-listen", "127.0.0.1:9491",
    "--config-dir", join(out, "h"), "--pin", "424242", "--no-mdns", "--encoder", "vah264lpenc", "--source", source,
    "--size", "1280x720", "--fps", "60", "--audio", "none", ...extra], hostLog);
}
async function hostUrl() {
  for (let i = 0; i < 150; i++) {
    await sleep(100);
    const m = existsSync(hostLog) ? [...readFileSync(hostLog, "utf8").matchAll(/^WEB-URL (.*)$/gm)].pop() : null;
    if (m) return m[1];
  }
  throw new Error("host did not print WEB-URL");
}

const desk = `env GDK_BACKEND=wayland python3 ${join(here, "wl_probe.py")} ${probe}`;
let host = startHost(`vdisplay:${desk}`);
let page;
try {
  const url = await hostUrl();
  page = await launch({ port: 9338 });
  const viewport = (width, height) => page.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
  const st = () => page.eval("window.qrStats");
  const waitSize = async (w, h, ms) => {
    const t0 = Date.now();
    await page.waitFor(`window.qrStats.width === ${w} && window.qrStats.height === ${h}`, ms).catch(() => {});
    return Date.now() - t0;
  };
  await viewport(1400, 900);
  await page.goto(url + "&pin=424242");
  await page.waitFor("window.qrStats && window.qrStats.connected && window.qrStats.decoded > 2", 20000).catch(() => {});

  // A1: follow the window: the desktop takes the picture area's size (the
  // window above the bar).
  const area = () => page.eval("(() => { const r = document.getElementById('stage').getBoundingClientRect(); return [Math.round(r.width) & ~1, Math.round(r.height) & ~1]; })()");
  let [aw, ah] = await area();
  await waitSize(aw, ah, 6000);
  let s = await st();
  const a1a = s.width === aw && s.height === ah;
  const first = `${aw}×${ah}`;
  await viewport(1000, 700);
  await sleep(100);
  [aw, ah] = await area();
  const took = await waitSize(aw, ah, 4000);
  s = await st();
  check("A1", `the desktop follows the window (1400×900 window → ${first}: ${a1a}; 1000×700 window → ${s.width}×${s.height} after ${took} ms, area ${aw}×${ah})`,
    a1a && s.width === aw && s.height === ah && took <= 1500);

  // A2: fixed resolution; Fit shrinks it, 100% shows it at native size
  // and pans, resizing the window leaves it alone.
  await page.eval(`(() => { document.querySelector('#display-btn').click();
    const p = document.querySelector('#preset'); p.value = '1920x1080'; p.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await waitSize(1920, 1080, 5000);
  const fitW = await page.eval("parseFloat(document.getElementById('v').style.width)");
  await page.eval(`(() => { const r = document.querySelector('input[name=view][value=native]'); r.checked = true;
    r.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await sleep(300);
  const natW = await page.eval("parseFloat(document.getElementById('v').style.width)");
  const pans = await page.eval("window.qrView.pans()");
  await viewport(1100, 700);
  await sleep(1500);
  s = await st();
  check("A2", `fixed 1920×1080 (stream ${s.width}×${s.height}); Fit shows it ${Math.round(fitW)} px wide, 100% ${Math.round(natW)} px and pans (${pans}); the window resize left it`,
    s.width === 1920 && s.height === 1080 && fitW < 1100 && Math.round(natW) === 1920 && pans);

  // A9: the settings survive a reload.
  await page.send("Page.reload");
  await page.waitFor("window.qrStats && window.qrStats.connected && window.qrStats.decoded > 2", 20000).catch(() => {});
  await waitSize(1920, 1080, 5000);
  const kept = await page.eval(`[document.querySelector('input[name=mode][value=fixed]').checked,
    document.querySelector('input[name=view][value=native]').checked, window.qrStats.width]`);
  check("A9", `settings survive a reload (fixed ${kept[0]}, 100% ${kept[1]}, stream width ${kept[2]})`, kept[0] && kept[1] && kept[2] === 1920);

  // A8: zoom with Ctrl+wheel, pan, and a click lands on the remote point.
  await page.eval(`(() => { const r = document.querySelector('input[name=view][value=fit]'); r.checked = true;
    r.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await sleep(300);
  await page.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: 500, y: 300, deltaX: 0, deltaY: -347, modifiers: 2 });
  await sleep(300);
  const zoom = await page.eval("window.qrView.zoom");
  await page.eval("window.qrView.panBy(-5000, -5000)");
  const clickAt = async (rx, ry) => {
    const [cx, cy] = await page.eval(`(() => { const v = window.qrView, r = document.getElementById('stage').getBoundingClientRect();
      return [r.left + v.x + (${rx} + 0.5) * v.scale, r.top + v.y + (${ry} + 0.5) * v.scale]; })()`);
    const before = events().filter((e) => e.ev === "button").length;
    await page.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy });
    await sleep(150);
    await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: cx, y: cy, button: "left", clickCount: 1 });
    await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: cx, y: cy, button: "left", clickCount: 1 });
    await sleep(500);
    return events().filter((e) => e.ev === "button").slice(before)[0];
  };
  const near = (e, x, y) => e && Math.abs(e.x - x) <= 2 && Math.abs(e.y - y) <= 2;
  const zoomed = await clickAt(1700, 900);
  await page.eval("window.qrView.setZoom(1)");
  await sleep(200);
  const fitted = await clickAt(400, 300);
  await page.eval(`(() => { const r = document.querySelector('input[name=view][value=fill]'); r.checked = true;
    r.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await sleep(300);
  const filled = await clickAt(1000, 500);
  check("A8", `Ctrl+wheel zooms to ${zoom.toFixed(2)}×; clicks land on the remote point zoomed+panned ${JSON.stringify(zoomed && [zoomed.x, zoomed.y])}→(1700,900), Fit ${JSON.stringify(fitted && [fitted.x, fitted.y])}→(400,300), Fill ${JSON.stringify(filled && [filled.x, filled.y])}→(1000,500)`,
    zoom > 1.9 && zoom < 2.1 && near(zoomed, 1700, 900) && near(fitted, 400, 300) && near(filled, 1000, 500));
  await page.eval(`(() => { const r = document.querySelector('input[name=view][value=fit]'); r.checked = true;
    r.dispatchEvent(new Event('change', { bubbles: true })); })()`);

  // A11: hidden → no video; visible → a keyframe within a second.
  await page.eval(`(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    document.dispatchEvent(new Event('visibilitychange')); })()`);
  await sleep(1500);
  const d0 = (await st()).decoded;
  await sleep(2000);
  const d1 = (await st()).decoded;
  await page.eval(`(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
    document.dispatchEvent(new Event('visibilitychange')); })()`);
  const t0 = Date.now();
  await page.waitFor(`window.qrStats.decoded > ${d1}`, 3000).catch(() => {});
  const back = Date.now() - t0;
  const paused = readFileSync(hostLog, "utf8").includes("client video pause");
  check("A11", `hidden: no frames for 2 s (${d1 - d0} decoded), the host paused (${paused}); visible: a frame after ${back} ms`,
    d1 - d0 === 0 && paused && back <= 1000);

  // A12: touch gestures (CDP touch events become pointer events).
  await page.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
  const pt = async (rx, ry) => page.eval(`(() => { const v = window.qrView, r = document.getElementById('stage').getBoundingClientRect();
      return { x: r.left + v.x + (${rx} + 0.5) * v.scale, y: r.top + v.y + (${ry} + 0.5) * v.scale }; })()`);
  const touch = (type, points) => page.send("Input.dispatchTouchEvent", { type, touchPoints: points.map((p, i) => ({ x: p.x, y: p.y, id: i + 1 })) });
  const lastButtons = (n) => events().filter((e) => e.ev === "button").slice(-n).map((e) => e.button);
  const nb = () => events().filter((e) => e.ev === "button").length;
  let b0 = nb();
  const p1 = await pt(600, 400);
  await touch("touchStart", [p1]); await touch("touchEnd", []);
  await sleep(500);
  const tap = nb() > b0 ? lastButtons(1)[0] : null;
  b0 = nb();
  await touch("touchStart", [p1]); await sleep(700); await touch("touchEnd", []);
  await sleep(500);
  const long = nb() > b0 ? lastButtons(1)[0] : null;
  const p2 = { x: p1.x + 60, y: p1.y }, p3 = { x: p1.x + 120, y: p1.y }, p4 = { x: p1.x + 180, y: p1.y };
  b0 = nb();
  await touch("touchStart", [p1, p2]); await touch("touchEnd", []);
  await sleep(500);
  const two = nb() > b0 ? lastButtons(1)[0] : null;
  b0 = nb();
  await touch("touchStart", [p1, p2, p3]); await touch("touchEnd", []);
  await sleep(500);
  const three = nb() > b0 ? lastButtons(1)[0] : null;
  const s0 = events().filter((e) => e.ev === "scroll").length;
  await touch("touchStart", [p1, p2]);
  for (let i = 1; i <= 10; i++) { await touch("touchMove", [{ x: p1.x, y: p1.y + i * 12 }, { x: p2.x, y: p2.y + i * 12 }]); await sleep(20); }
  await touch("touchEnd", []);
  await sleep(500);
  const scrolls = events().filter((e) => e.ev === "scroll").length - s0;
  const z0 = await page.eval("window.qrView.zoom");
  await touch("touchStart", [{ x: p1.x - 30, y: p1.y }, { x: p1.x + 30, y: p1.y }]);
  for (let i = 1; i <= 10; i++) { await touch("touchMove", [{ x: p1.x - 30 - i * 10, y: p1.y }, { x: p1.x + 30 + i * 10, y: p1.y }]); await sleep(20); }
  await touch("touchEnd", []);
  const z1 = await page.eval("window.qrView.zoom");
  await page.eval("window.qrView.setZoom(1)");
  await touch("touchStart", [p1, p2, p3, p4]); await touch("touchEnd", []);
  await sleep(300);
  const kbdUp = await page.eval("document.activeElement === document.getElementById('kbd')");
  check("A12", `touch: tap→${tap}, long press→${long}, two-finger tap→${two}, three-finger tap→${three} (GTK: 1 left, 3 right, 2 middle), two-finger drag→${scrolls} scroll events, pinch zoom ${z0}→${z1.toFixed(2)}, four-finger tap keyboard ${kbdUp}`,
    tap === 1 && long === 3 && two === 3 && three === 2 && scrolls > 0 && z1 > 1.3 && kbdUp);
  await page.send("Emulation.setTouchEmulationEnabled", { enabled: false });

  // A13: sticky Ctrl on the keys strip, then "c" from the keyboard.
  const k0 = events().filter((e) => e.ev === "key").length;
  await page.eval("document.querySelector('#keys [data-mod=\"29\"]').click(); document.getElementById('kbd').focus()");
  await page.send("Input.insertText", { text: "c" });
  await sleep(400);
  await page.send("Input.insertText", { text: "c" });
  await sleep(600);
  const ks = events().filter((e) => e.ev === "key").slice(k0).filter((e) => e.char === "c").map((e) => e.ctrl);
  check("A13", `sticky Ctrl then "c" sends Ctrl+C, the next "c" is plain (ctrl flags ${JSON.stringify(ks)})`, ks.length >= 2 && ks[0] === true && ks[ks.length - 1] === false);
  await page.eval("document.getElementById('kbd').blur(); document.getElementById('v').focus()");

  // A14: fullscreen with the keyboard locked; Ctrl+T reaches the desktop;
  // holding Escape leaves fullscreen.
  const fs = await page.eval("(() => { const r = document.getElementById('fs').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()");
  await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: fs[0], y: fs[1], button: "left", clickCount: 1 });
  await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: fs[0], y: fs[1], button: "left", clickCount: 1 });
  await sleep(800);
  const inFs = await page.eval("!!document.fullscreenElement");
  const kt = events().filter((e) => e.ev === "key").length;
  await page.send("Input.dispatchKeyEvent", { type: "rawKeyDown", code: "ControlLeft", key: "Control", modifiers: 2, windowsVirtualKeyCode: 17 });
  await page.send("Input.dispatchKeyEvent", { type: "rawKeyDown", code: "KeyT", key: "t", modifiers: 2, windowsVirtualKeyCode: 84 });
  await page.send("Input.dispatchKeyEvent", { type: "keyUp", code: "KeyT", key: "t", modifiers: 2, windowsVirtualKeyCode: 84 });
  await page.send("Input.dispatchKeyEvent", { type: "keyUp", code: "ControlLeft", key: "Control", windowsVirtualKeyCode: 17 });
  await sleep(500);
  const ctrlT = events().filter((e) => e.ev === "key").slice(kt).some((e) => e.char === "t" && e.ctrl);
  await page.send("Input.dispatchKeyEvent", { type: "rawKeyDown", code: "Escape", key: "Escape", windowsVirtualKeyCode: 27 });
  await sleep(1300);
  await page.send("Input.dispatchKeyEvent", { type: "keyUp", code: "Escape", key: "Escape", windowsVirtualKeyCode: 27 });
  await sleep(300);
  const leftFs = await page.eval("!document.fullscreenElement");
  check("A14", `fullscreen (${inFs}); Ctrl+T reaches the desktop (${ctrlT}); holding Escape leaves it (${leftFs})`, inFs && ctrlT && leftFs);

  // A10: the host restarts; the page reconnects without a PIN.
  const before = (await st()).decoded;
  host.kill("SIGTERM");
  await sleep(1500);
  host = startHost(`vdisplay:${desk}`);
  const r0 = Date.now();
  await page.waitFor(`window.qrStats.connected && window.qrStats.reconnects > 0 && window.qrStats.decoded > ${before} + 2`, 15000).catch(() => {});
  s = await st();
  const pinShown = await page.eval("!document.getElementById('pin').hidden");
  check("A10", `after a host restart the page reconnects in ${((Date.now() - r0) / 1000).toFixed(1)} s (${s.reconnects} attempts), no PIN asked (${!pinShown})`,
    s.connected && s.reconnects > 0 && !pinShown && Date.now() - r0 <= 12000);

  // A21: a desktop that keeps its own size (X11).
  host.kill("SIGTERM");
  await sleep(1000);
  run("Xvfb", [":84", "-screen", "0", "1280x720x24", "-nolisten", "tcp"]);
  await sleep(800);
  host = startHost("x11::84");
  await page.waitFor("window.qrStats.connected && window.qrStats.displayKind === 0", 15000).catch(() => {});
  const fixedUi = await page.eval("[document.getElementById('res-set').disabled, window.qrStats.displayNote]");
  check("A21", `an X11 desktop: resolution settings disabled (${fixedUi[0]}), note "${fixedUi[1]}"`, fixedUi[0] && /keeps its own size/.test(fixedUi[1]));
  if (page.logs.some((l) => l.startsWith("EXCEPTION"))) console.log(page.logs.filter((l) => l.startsWith("EXCEPTION")).join("\n"));
} finally {
  await page?.close();
  for (const p of procs) { try { p.kill("SIGKILL"); } catch { /* gone */ } }
  console.log(`logs in ${out}`);
}
