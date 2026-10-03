// The TypeScript codec decodes every conformance vector and re-encodes it
// byte for byte (same check as crates/qr-proto/tests/vectors.rs).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { decodeAudio, decodeInput, decodeMotion, decodeMsg, decodePad, encodeAudio, encodeInput, encodeMotion, encodeMsg, encodePad } from "../src/proto.ts";

const vectors: { name: string; kind: string; hex: string }[] = JSON.parse(
  readFileSync(new URL("../../conformance/vectors.json", import.meta.url), "utf8"),
);
const fromHex = (h: string) => Uint8Array.from(h.match(/../g)!.map((x) => parseInt(x, 16)));
const toHex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

test("every vector round-trips", () => {
  let n = 0;
  for (const v of vectors) {
    const b = fromHex(v.hex);
    let out: Uint8Array | undefined;
    switch (v.kind) {
      case "msg": out = encodeMsg(decodeMsg(b)); break;
      case "input": out = encodeInput(decodeInput(b)); break;
      case "motion": out = encodeMotion(decodeMotion(b)); break;
      case "pad": out = encodePad(decodePad(b)); break;
      case "audio": out = encodeAudio(decodeAudio(b)); break;
      default: continue; // video/meta/rv/probe: parsed in WASM or not used by the web client
    }
    assert.equal(toHex(out), v.hex, v.name);
    n++;
  }
  const expected = vectors.filter((v) => ["msg", "input", "motion", "pad", "audio"].includes(v.kind)).length;
  assert.equal(n, expected);
  assert.ok(n > 30);
});

test("truncated messages throw instead of misparsing", () => {
  for (const v of vectors.filter((v) => v.kind === "msg")) {
    const b = fromHex(v.hex);
    for (let cut = 0; cut < b.length; cut++) {
      assert.throws(() => decodeMsg(b.subarray(0, cut)), v.name);
    }
  }
});
