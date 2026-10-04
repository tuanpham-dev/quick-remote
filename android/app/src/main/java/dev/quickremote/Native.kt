package dev.quickremote

import java.nio.ByteBuffer

/** The Rust session core (crates/qr-ffi). */
object Native {
    init {
        System.loadLibrary("quickremote")
        init()
    }

    external fun init()
    external fun listHosts(ms: Int): String
    /** codecs: bit 0 H.264, 1 H.265, 2 AV1. Throws IllegalStateException with a readable message. */
    external fun connect(configDir: String, target: String, pin: String?, name: String, rendezvous: String?, codecs: Int, width: Int, height: Int): Long
    external fun nextVideo(h: Long, buf: ByteBuffer, meta: LongArray, timeoutMs: Int): Int
    external fun nextAudio(h: Long, buf: ByteBuffer, timeoutMs: Int): Int
    external fun decoded(h: Long, id: Long, decodeUs: Long)
    external fun pollEvent(h: Long): String?
    external fun key(h: Long, code: Int, down: Boolean)
    external fun button(h: Long, button: Int, down: Boolean)
    external fun abs(h: Long, x: Int, y: Int)
    external fun rel(h: Long, dx: Float, dy: Float)
    external fun wheel(h: Long, dx: Int, dy: Int)
    external fun text(h: Long, text: String)
    external fun pen(h: Long, x: Int, y: Int, pressure: Int, tiltX: Int, tiltY: Int, flags: Int)
    external fun pad(h: Long, idx: Int, buttons: Int, axes: ShortArray)
    external fun padDetach(h: Long, idx: Int)
    external fun clipboard(h: Long, text: String)
    external fun close(h: Long)
}
