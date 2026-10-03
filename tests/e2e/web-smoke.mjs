// Opens the host's web link in headless Chrome, pairs with the PIN, and
// reports the page's stats after a few seconds.
import { launch } from "./cdp.mjs";
const url = process.argv[2];
const page = await launch({ port: Number(process.env.CDP_PORT || 9333) });
try {
  await page.goto(url);
  await page.waitFor("window.qrStats && window.qrStats.connected", 20000);
  await page.waitFor("window.qrStats.decoded > 30", 20000);
  await new Promise((r) => setTimeout(r, 2500));
  console.log(JSON.stringify(await page.eval("window.qrStats")));
} catch (e) {
  console.log("FAILED", e.message);
  console.log(JSON.stringify(await page.eval("window.qrStats").catch(() => null)));
  console.log(page.logs.slice(-10).join("\n"));
  process.exitCode = 1;
} finally {
  await page.close();
}
