// WebTransport helpers: length-prefixed frames on streams.

import { frame } from "./proto.ts";

/** Reads u32-length-prefixed frames from a WebTransport receive stream. */
export class FramedReader {
  private reader: ReadableStreamDefaultReader<Uint8Array>;
  private buf = new Uint8Array(0);

  constructor(stream: ReadableStream<Uint8Array>) {
    this.reader = stream.getReader();
  }

  private async fill(n: number): Promise<boolean> {
    while (this.buf.length < n) {
      const { value, done } = await this.reader.read();
      if (done) return false;
      const b = new Uint8Array(this.buf.length + value.length);
      b.set(this.buf);
      b.set(value, this.buf.length);
      this.buf = b;
    }
    return true;
  }

  /** The next frame, or null at end of stream. */
  async next(): Promise<Uint8Array | null> {
    if (!(await this.fill(4))) return null;
    const n = new DataView(this.buf.buffer, this.buf.byteOffset, 4).getUint32(0, true);
    if (n > 17 * 1024 * 1024) throw new Error("frame too large");
    if (!(await this.fill(4 + n))) return null;
    const body = this.buf.slice(4, 4 + n);
    this.buf = this.buf.slice(4 + n);
    return body;
  }

  /** Raw bytes (after any buffered data), for file bodies. */
  async raw(n: number): Promise<Uint8Array | null> {
    if (!(await this.fill(n))) return null;
    const b = this.buf.slice(0, n);
    this.buf = this.buf.slice(n);
    return b;
  }

  /** Everything left until the stream ends, chunk by chunk. */
  async *rest(): AsyncGenerator<Uint8Array> {
    if (this.buf.length) {
      yield this.buf;
      this.buf = new Uint8Array(0);
    }
    for (;;) {
      const { value, done } = await this.reader.read();
      if (done) return;
      yield value;
    }
  }
}

/** Writes u32-length-prefixed frames, one writer at a time. */
export class FramedWriter {
  private w: WritableStreamDefaultWriter<Uint8Array>;
  private chain: Promise<void> = Promise.resolve();

  constructor(stream: WritableStream<Uint8Array>) {
    this.w = stream.getWriter();
  }

  send(body: Uint8Array): Promise<void> {
    this.chain = this.chain.then(() => this.w.write(frame(body))).catch(() => {});
    return this.chain;
  }

  close(): void {
    this.chain.then(() => this.w.close()).catch(() => {});
  }
}

export const enc = (v: unknown) => new TextEncoder().encode(JSON.stringify(v));
export const dec = (b: Uint8Array) => JSON.parse(new TextDecoder().decode(b));
export const b64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));
export const unb64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
