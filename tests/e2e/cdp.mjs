// Minimal Chrome DevTools Protocol driver (Node ≥ 22: global WebSocket).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function launch({ chrome = process.env.CHROME || "/usr/bin/google-chrome", port = 9333, args = [] } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "qr-chrome-"));
  const proc = spawn(chrome, [
    "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${dir}`, "--no-first-run",
    "--no-default-browser-check", "--autoplay-policy=no-user-gesture-required", "--use-angle=swiftshader",
    "--enable-unsafe-swiftshader", "--window-size=1280,800", ...args,
    ...(process.env.CHROME_ARGS ? process.env.CHROME_ARGS.split(" ") : []), "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  let ws;
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((t) => t.type === "page");
      if (page) { ws = new WebSocket(page.webSocketDebuggerUrl); break; }
    } catch { /* not up yet */ }
    await sleep(100);
  }
  if (!ws) throw new Error("Chrome did not start");
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const waiting = new Map();
  const handlers = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) {
      const { resolve, reject } = waiting.get(msg.id);
      waiting.delete(msg.id);
      msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    } else if (msg.method) {
      for (const h of handlers) h(msg);
    }
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const i = ++id;
    waiting.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  const page = {
    send,
    on: (h) => handlers.push(h),
    async eval(expr) {
      const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || "evaluation failed");
      return r.result.value;
    },
    async goto(url) { await send("Page.navigate", { url }); },
    async waitFor(expr, ms = 15000) {
      const end = Date.now() + ms;
      for (;;) {
        const v = await page.eval(expr).catch(() => undefined);
        if (v) return v;
        if (Date.now() > end) throw new Error(`timed out waiting for ${expr}`);
        await sleep(100);
      }
    },
    async key(code, key, down) {
      await send("Input.dispatchKeyEvent", { type: down ? "rawKeyDown" : "keyUp", code, key, windowsVirtualKeyCode: key.toUpperCase().charCodeAt(0) });
    },
    async close() {
      try { ws.close(); } catch {}
      proc.kill("SIGKILL");
      await new Promise((r) => { proc.once("exit", r); setTimeout(r, 2000); });
      // The throwaway profile (tens of MB each, in /tmp).
      try { rmSync(dir, { recursive: true, force: true }); } catch {}
    },
  };
  await send("Runtime.enable");
  await send("Page.enable");
  page.logs = [];
  page.on((m) => {
    if (m.method === "Runtime.consoleAPICalled") page.logs.push(m.params.args.map((a) => a.value ?? a.description).join(" "));
    if (m.method === "Runtime.exceptionThrown") page.logs.push("EXCEPTION " + (m.params.exceptionDetails.exception?.description ?? ""));
  });
  return page;
}
