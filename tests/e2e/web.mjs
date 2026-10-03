// A27: the web client in headless Chrome against a host on an Xvfb display.
// Checks video, audio, keys, pointer lock, a file upload, clipboard
// host → page, a paired reconnect without PIN, and reports gamepads as
// ENV-BLOCKED (headless Chrome has no virtual gamepad).
//
// usage: node tests/e2e/web.mjs   (needs target/debug/qr-host, Xvfb, xev)
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, existsSync, appendFileSync } from "node:fs";
import { join } from "node:path";
import { launch } from "./cdp.mjs";

const root = new URL("../..", import.meta.url).pathname;
const out = mkdtempSync(join(root, "out", "web-"));
const results = join(out, "results.txt");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const report = (status, id, what) => { const l = `${status} ${id} — ${what}`; console.log(l); appendFileSync(results, l + "\n"); };
const check = (id, what, ok) => report(ok ? "PASS" : "FAIL", id, what);
const env = { ...process.env, GST_REGISTRY: process.env.GST_REGISTRY || join(out, "gst.bin"), SVT_LOG: "1" };

const D = ":83";
const procs = [];
const run = (cmd, args, log) => { const p = spawn(cmd, args, { env, stdio: ["ignore", "pipe", "pipe"] }); procs.push(p); if (log) { p.stdout.on("data", (d) => appendFileSync(log, d)); p.stderr.on("data", (d) => appendFileSync(log, d)); } return p; };

try {
  run("Xvfb", [D, "-screen", "0", "1280x720x24", "-nolisten", "tcp"]);
  await sleep(800);
  execFileSync("setxkbmap", ["-display", D, "us"]);
  run("xev", ["-display", D, "-geometry", "1270x710+0+0", "-event", "keyboard"], join(out, "xev.log"));
  await sleep(500);
  const hostLog = join(out, "host.log");
  run(join(root, "target/debug/qr-host"), ["--listen", "127.0.0.1:9480", "--web-listen", "127.0.0.1:9481", "--config-dir", join(out, "h"),
    "--pin", "424242", "--no-mdns", "--encoder", "x264enc", "--source", `x11:${D}`, "--audio", "test:sine", "--files-dir", join(out, "in")], hostLog);
  let url = "";
  for (let i = 0; i < 100 && !url; i++) { await sleep(100); url = existsSync(hostLog) ? (readFileSync(hostLog, "utf8").match(/^WEB-URL (.*)$/m) || [])[1] || "" : ""; }
  if (!url) throw new Error("host did not print WEB-URL");

  const page = await launch({ port: 9334 });
  await page.send("Browser.grantPermissions", { permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"], origin: new URL(url).origin }).catch(() => {});
  await page.goto(url + "&pin=424242");
  await page.waitFor("window.qrStats && window.qrStats.decoded > 30", 25000).catch(() => {});
  await sleep(2000);
  let st = await page.eval("window.qrStats");
  check("A27-video", `video decodes in the browser (${st.decoded} frames, ${st.fps.toFixed(0)} fps, ${st.decoder})`, st.decoded > 30 && st.fps > 20);
  check("A27-audio", `host audio arrives (${st.audioPackets} Opus packets)`, st.audioPackets > 100);

  // Keys: the page sends physical codes; xev on the host shows them.
  await page.send("Runtime.evaluate", { expression: "document.getElementById('v').focus()" });
  await page.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: 300, y: 300 });
  await sleep(300);
  for (const [code, key] of [["KeyQ", "q"], ["KeyW", "w"]]) { await page.key(code, key, true); await page.key(code, key, false); }
  await sleep(800);
  const xev = existsSync(join(out, "xev.log")) ? readFileSync(join(out, "xev.log"), "utf8") : "";
  check("A27-keys", "key presses reach the host by physical position (q, w in xev)", /keycode 24 \(keysym 0x71, q\)/.test(xev) && /keycode 25 \(keysym 0x77, w\)/.test(xev));

  // Pointer lock via the hotkey.
  await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: 300, y: 300, button: "left", clickCount: 1 });
  await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: 300, y: 300, button: "left", clickCount: 1 });
  await page.send("Input.dispatchKeyEvent", { type: "rawKeyDown", code: "KeyM", key: "M", modifiers: 2 | 1 | 8, windowsVirtualKeyCode: 77 });
  await sleep(500);
  const locked = await page.eval("document.pointerLockElement === document.getElementById('v')");
  check("A27-lock", "Ctrl+Alt+Shift+M locks the pointer", locked);

  // File upload through the picker.
  const file = join(out, "upload.bin");
  writeFileSync(file, Buffer.alloc(3 * 1024 * 1024, 7));
  const { root: dom } = await page.send("DOM.getDocument");
  const { nodeId } = await page.send("DOM.querySelector", { nodeId: dom.nodeId, selector: "#file" });
  await page.send("DOM.setFileInputFiles", { nodeId, files: [file] });
  let got = false;
  for (let i = 0; i < 50 && !got; i++) { await sleep(200); got = existsSync(join(out, "in", "upload.bin")); }
  check("A27-files", "a 3 MiB upload from the page arrives on the host", got && readFileSync(join(out, "in", "upload.bin")).equals(readFileSync(file)));

  // Clipboard host → page.
  const marker = `web-clip-${Date.now()}`;
  run(join(root, "target/debug/qr-clipctl"), [D, "set-text", marker]);
  await sleep(1500);
  const clip = await page.eval("navigator.clipboard.readText().catch(e => 'ERR ' + e)");
  check("A27-clipboard", `host clipboard reaches the page (${clip === marker ? "matches" : clip})`, clip === marker);

  // Reconnect: this browser is paired now, no PIN.
  await page.goto(url);
  await page.waitFor("window.qrStats && window.qrStats.decoded > 10", 20000).catch(() => {});
  st = await page.eval("window.qrStats");
  check("A27-repaired", "a paired browser reconnects without a PIN", st.decoded > 10);
  report("ENV-BLOCKED", "A27-gamepad", "headless Chrome has no virtual gamepad (the Gamepad API path needs a real browser and pad)");
  report("ENV-BLOCKED", "A27-firefox", "Firefox is not installed on this machine");
  await page.close();
} catch (e) {
  report("FAIL", "A27", `driver error: ${e.message}`);
} finally {
  for (const p of procs) p.kill("SIGKILL");
  console.log(`logs: ${out}`);
}
