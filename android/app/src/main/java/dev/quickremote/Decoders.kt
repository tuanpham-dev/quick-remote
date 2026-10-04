package dev.quickremote

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Which codecs this device decodes in hardware: bit 0 H.264, 1 H.265, 2 AV1. */
fun hardwareCodecs(): Int {
    var bits = 0
    val list = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { !it.isEncoder }
    fun has(mime: String) = list.any { info -> info.supportedTypes.any { it.equals(mime, true) } }
    if (has(MediaFormat.MIMETYPE_VIDEO_AVC)) bits = bits or 1
    if (has(MediaFormat.MIMETYPE_VIDEO_HEVC)) bits = bits or 2
    if (has(MediaFormat.MIMETYPE_VIDEO_AV1)) bits = bits or 4
    return bits
}

/**
 * Pulls frames from the Rust core and decodes them straight to the surface,
 * rendering each picture as soon as it's out (no presentation-time wait).
 */
class VideoDecoder(private val h: Long, private val surface: Surface, private val stats: Stats) : Thread("qr-video") {
    @Volatile var running = true
    private var codec: MediaCodec? = null
    private var mime = ""
    private val pushed = HashMap<Long, Long>()

    override fun run() {
        val buf = ByteBuffer.allocateDirect(8 * 1024 * 1024)
        val meta = LongArray(6)
        val info = MediaCodec.BufferInfo()
        while (running) {
            val n = Native.nextVideo(h, buf, meta, 20)
            if (n < 0) break
            if (n > 0) {
                val keyframe = meta[1] == 1L
                val m = when (meta[2].toInt()) {
                    2 -> MediaFormat.MIMETYPE_VIDEO_HEVC
                    3 -> MediaFormat.MIMETYPE_VIDEO_AV1
                    else -> MediaFormat.MIMETYPE_VIDEO_AVC
                }
                if (keyframe && (codec == null || m != mime)) configure(m, meta[3].toInt(), meta[4].toInt())
                val c = codec ?: continue
                val idx = c.dequeueInputBuffer(10_000)
                if (idx >= 0) {
                    val ib = c.getInputBuffer(idx)!!
                    ib.clear()
                    buf.limit(n).position(0)
                    ib.put(buf)
                    buf.clear()
                    pushed[meta[0]] = System.nanoTime() / 1000
                    c.queueInputBuffer(idx, 0, n, meta[0], if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
                    stats.captureUs[meta[0]] = meta[5]
                }
            }
            drain(info)
        }
        codec?.run { stop(); release() }
    }

    private fun drain(info: MediaCodec.BufferInfo) {
        val c = codec ?: return
        while (true) {
            val out = c.dequeueOutputBuffer(info, 0)
            if (out < 0) return
            c.releaseOutputBuffer(out, true)
            val now = System.nanoTime() / 1000
            val id = info.presentationTimeUs
            pushed.remove(id)?.let { Native.decoded(h, id, now - it) ; stats.decodeUs = now - it }
            stats.frame(id, now)
        }
    }

    private fun configure(m: String, w: Int, hgt: Int) {
        codec?.run { stop(); release() }
        val f = MediaFormat.createVideoFormat(m, w, hgt)
        f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        f.setInteger(MediaFormat.KEY_PRIORITY, 0)
        f.setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
        val c = MediaCodec.createDecoderByType(m)
        c.configure(f, surface, null, 0)
        c.start()
        codec = c
        mime = m
        stats.codec = c.name
    }
}

/** Opus host audio through MediaCodec into a low-latency AudioTrack. */
class AudioPlayer(private val h: Long) : Thread("qr-audio") {
    @Volatile var running = true

    override fun run() {
        val f = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, 48000, 2)
        // OpusHead, pre-skip and seek pre-roll (in ns), as MediaCodec wants them.
        val head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
        head.put("OpusHead".toByteArray()).put(1).put(2).putShort(312).putInt(48000).putShort(0).put(0)
        head.flip()
        f.setByteBuffer("csd-0", head)
        // Codec delay (312 samples = 6.5 ms) and seek pre-roll (80 ms), in ns.
        f.setByteBuffer("csd-1", ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putLong(6_500_000).flip() as ByteBuffer)
        f.setByteBuffer("csd-2", ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putLong(80_000_000).flip() as ByteBuffer)
        val dec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
        dec.configure(f, null, null, 0)
        dec.start()
        val track = AudioTrack.Builder()
            .setAudioAttributes(AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_GAME).setContentType(AudioAttributes.CONTENT_TYPE_MOVIE).build())
            .setAudioFormat(AudioFormat.Builder().setSampleRate(48000).setChannelMask(AudioFormat.CHANNEL_OUT_STEREO).setEncoding(AudioFormat.ENCODING_PCM_16BIT).build())
            .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
            .setBufferSizeInBytes(AudioTrack.getMinBufferSize(48000, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT))
            .build()
        track.play()
        val buf = ByteBuffer.allocateDirect(4096)
        val info = MediaCodec.BufferInfo()
        var pts = 0L
        while (running) {
            val n = Native.nextAudio(h, buf, 20)
            if (n < 0) break
            if (n > 0) {
                val idx = dec.dequeueInputBuffer(5_000)
                if (idx >= 0) {
                    val ib = dec.getInputBuffer(idx)!!
                    ib.clear()
                    buf.limit(n).position(0)
                    ib.put(buf)
                    buf.clear()
                    dec.queueInputBuffer(idx, 0, n, pts, 0)
                    pts += 5_000
                }
            }
            while (true) {
                val out = dec.dequeueOutputBuffer(info, 0)
                if (out < 0) break
                val ob = dec.getOutputBuffer(out)!!
                track.write(ob, info.size, AudioTrack.WRITE_NON_BLOCKING)
                dec.releaseOutputBuffer(out, false)
            }
        }
        track.stop(); track.release(); dec.stop(); dec.release()
    }
}

/** Frame rate and latency for the overlay. */
class Stats {
    @Volatile var codec = ""
    @Volatile var decodeUs = 0L
    val captureUs = java.util.concurrent.ConcurrentHashMap<Long, Long>()
    private var frames = 0
    private var latSum = 0L
    private var latN = 0
    private var windowStart = System.nanoTime()
    @Volatile var fps = 0f
    @Volatile var latencyMs = 0f

    @Synchronized fun frame(id: Long, nowUs: Long) {
        frames++
        captureUs.remove(id)?.let { if (it > 0) { latSum += nowUs - it; latN++ } }
        val t = System.nanoTime()
        if (t - windowStart >= 1_000_000_000L) {
            val secs = (t - windowStart) / 1e9f
            fps = frames / secs
            latencyMs = if (latN > 0) latSum / latN / 1000f else 0f
            frames = 0; latSum = 0; latN = 0; windowStart = t
        }
    }
}
