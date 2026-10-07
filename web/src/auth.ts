// Browser identity and authentication with the host's web gateway.
//
// The browser keeps a non-extractable ECDSA P-256 key in IndexedDB. First
// time: PIN pairing (SPAKE2 in WebAssembly, bound to the browser key and
// the host's certificate) enrols it. Later: it signs the host's nonce.

import init, { WebPairing, sha256 } from "./wasm/qr_wasm.js";
import { FramedReader, FramedWriter, b64, dec, enc, unb64 } from "./transport.ts";

const DB = "quickremote";

function idb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore("keys");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

async function idbGet<T>(key: string): Promise<T | undefined> {
  const db = await idb();
  return new Promise((resolve, reject) => {
    const r = db.transaction("keys").objectStore("keys").get(key);
    r.onsuccess = () => resolve(r.result as T | undefined);
    r.onerror = () => reject(r.error);
  });
}

async function idbPut(key: string, value: unknown): Promise<void> {
  const db = await idb();
  return new Promise((resolve, reject) => {
    const t = db.transaction("keys", "readwrite");
    t.objectStore("keys").put(value, key);
    t.oncomplete = () => resolve();
    t.onerror = () => reject(t.error);
  });
}

/** This browser's key pair, created on first use. */
export async function browserKey(): Promise<CryptoKeyPair> {
  let k = await idbGet<CryptoKeyPair>("identity");
  if (!k) {
    k = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
    await idbPut("identity", k);
  }
  return k;
}

/** Hosts this browser paired with, by certificate-independent host name. */
export async function pairedHosts(): Promise<string[]> {
  return (await idbGet<string[]>("paired")) ?? [];
}

let wasmReady: Promise<unknown> | null = null;
export function wasm(): Promise<unknown> {
  wasmReady ??= init();
  return wasmReady;
}

export class PinNeeded extends Error {}

/**
 * Authenticates on the connection's first bidirectional stream. With a
 * `pin`, pairs; otherwise proves an earlier pairing.
 */
/** The host now presents another certificate than the one pinned (it
 *  rotated): fetch the current hash and connect again. */
export class CertChanged extends Error {}

/** `pinned`: the certificate hash this connection was made with. The
 *  authentication is bound to it, so a relay with another certificate
 *  can't pass the host's answers through. */
export async function authenticate(wt: WebTransport, pin: string | null, pinned: Uint8Array): Promise<void> {
  await wasm();
  const key = await browserKey();
  const spki = new Uint8Array(await crypto.subtle.exportKey("spki", key.publicKey));
  const s = await wt.createBidirectionalStream();
  const w = new FramedWriter(s.writable);
  const r = new FramedReader(s.readable);
  const read = async () => {
    const b = await r.next();
    if (!b) throw new Error("the host closed the connection");
    const m = dec(b);
    if (m.t === "error") {
      if (m.code === "unpaired") throw new PinNeeded(m.text);
      throw new Error(m.text);
    }
    return m;
  };
  const hello = await read();
  const nonce = unb64(hello.nonce);
  const hostFp = unb64(hello.hostFp);
  if (hostFp.length !== pinned.length || hostFp.some((b, i) => b !== pinned[i])) {
    throw new CertChanged("the host's certificate changed");
  }
  if (pin) {
    const p = new WebPairing(pin);
    await w.send(enc({ t: "pair", spki: b64(spki), msg: b64(p.message()), name: navigator.userAgent.slice(0, 60) }));
    const m = await read();
    const mac = p.finish(unb64(m.msg), sha256(spki), hostFp);
    await w.send(enc({ t: "confirm", mac: b64(mac) }));
    const c = await read();
    if (!p.verify(unb64(c.mac))) throw new Error("the host failed pairing confirmation; the connection may be intercepted");
  } else {
    const data = new Uint8Array(nonce.length + hostFp.length);
    data.set(nonce);
    data.set(hostFp, nonce.length);
    const sig = new Uint8Array(await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key.privateKey, data));
    await w.send(enc({ t: "key", spki: b64(spki), sig: b64(sig) }));
  }
  const ok = await read();
  if (ok.t !== "ok") throw new Error("unexpected answer from the host");
  w.close();
}
