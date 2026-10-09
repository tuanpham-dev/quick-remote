package dev.quickremote

import android.content.Context

/**
 * Display settings, kept per device: the remote desktop's size (follow the
 * screen or a fixed resolution), its pixel ratio, which monitor on
 * KDE/GNOME, how the picture is shown, and the touch mode.
 */
class Settings(ctx: Context) {
    private val p = ctx.getSharedPreferences("display", Context.MODE_PRIVATE)

    var follow: Boolean = p.getBoolean("follow", true)
    var fixedW: Int = p.getInt("fixedW", 1920)
    var fixedH: Int = p.getInt("fixedH", 1080)
    var hz: Int = p.getInt("hz", 60)
    /** Remote pixels per density-independent pixel; 0 = the screen's own (at most 2). */
    var ratio: Float = p.getFloat("ratio", 0f)
    /** True: the desktop's real monitor; false: a virtual monitor of its own. */
    var real: Boolean = p.getBoolean("real", false)
    var view: ViewMode = ViewMode.entries.getOrElse(p.getInt("view", 0)) { ViewMode.FIT }
    var trackpad: Boolean = p.getBoolean("trackpad", true)

    fun save() {
        p.edit().putBoolean("follow", follow).putInt("fixedW", fixedW).putInt("fixedH", fixedH).putInt("hz", hz)
            .putFloat("ratio", ratio).putBoolean("real", real).putInt("view", view.ordinal).putBoolean("trackpad", trackpad).apply()
    }

    /** The pixel ratio in effect on a screen of `density`. */
    fun ratio(density: Float): Float = if (ratio > 0f) ratio else density.coerceIn(1f, 2f)

    companion object {
        val PRESETS = listOf(1280 to 720, 1600 to 900, 1920 to 1080, 2560 to 1440, 3840 to 2160)
        val RATES = listOf(30, 60, 120)
        /** Below 1: the same desktop in fewer pixels (faster, softer). */
        val RATIOS = listOf(0.5f, 0.75f, 1f, 1.5f, 2f)

        /** An even size within what hosts take. */
        fun clampSize(w: Float, h: Float): Pair<Int, Int> =
            Pair(Math.round(w).coerceIn(320, 7680) and 1.inv(), Math.round(h).coerceIn(200, 4320) and 1.inv())
    }
}

enum class ViewMode { FIT, FILL, NATIVE }

/**
 * How the remote picture sits on the screen: a base scale from the view
 * mode (Fit, Fill, 100%), times the user's zoom, and a pan offset when the
 * picture is larger than the screen. All in screen pixels; the single
 * source for placing the picture and mapping touches.
 */
class ViewTransform {
    var mode = ViewMode.FIT
    /** Screen pixels per remote pixel at 100% (the density over the pixel ratio). */
    var natural = 1f
    var zoom = 1f
        private set
    var x = 0f
        private set
    var y = 0f
        private set
    var scale = 1f
        private set
    private var winW = 1f
    private var winH = 1f
    private var picW = 1f
    private var picH = 1f

    private fun base(): Float {
        val fit = minOf(winW / picW, winH / picH)
        return when (mode) {
            // Fit shrinks a large picture but never enlarges past 100%.
            ViewMode.FIT -> minOf(fit, natural)
            ViewMode.FILL -> fit
            ViewMode.NATIVE -> natural
        }
    }

    /** Recomputes for a screen and picture size, keeping the screen's centre on the same remote point. */
    fun layout(ww: Int, wh: Int, pw: Int, ph: Int) {
        val prev = scale
        val cx = (winW / 2 - x) / prev
        val cy = (winH / 2 - y) / prev
        winW = ww.coerceAtLeast(1).toFloat(); winH = wh.coerceAtLeast(1).toFloat()
        picW = pw.coerceAtLeast(1).toFloat(); picH = ph.coerceAtLeast(1).toFloat()
        scale = base() * zoom
        if (Math.abs(scale - prev) > 1e-6f) { x = winW / 2 - cx * scale; y = winH / 2 - cy * scale }
        clamp()
    }

    private fun clamp() {
        val dw = picW * scale; val dh = picH * scale
        x = if (dw <= winW) (winW - dw) / 2 else x.coerceIn(winW - dw, 0f)
        y = if (dh <= winH) (winH - dh) / 2 else y.coerceIn(winH - dh, 0f)
    }

    val width get() = picW * scale
    val height get() = picH * scale

    fun pans(): Boolean = picW * scale > winW + 0.5f || picH * scale > winH + 0.5f

    /** Zooms to `z`, keeping the remote point under (px, py) where it is. */
    fun setZoom(z: Float, px: Float = winW / 2, py: Float = winH / 2) {
        val rx = (px - x) / scale; val ry = (py - y) / scale
        zoom = z.coerceIn(ZOOM_MIN, ZOOM_MAX)
        scale = base() * zoom
        x = px - rx * scale; y = py - ry * scale
        clamp()
    }

    fun panBy(dx: Float, dy: Float) { x += dx; y += dy; clamp() }

    /** The remote pixel under a screen position, clamped to the picture. */
    fun toRemote(sx: Float, sy: Float): Pair<Int, Int> {
        val rx = ((sx - x) / scale).toInt().coerceIn(0, picW.toInt() - 1)
        val ry = ((sy - y) / scale).toInt().coerceIn(0, picH.toInt() - 1)
        return Pair(rx, ry)
    }

    companion object {
        const val ZOOM_MIN = 0.25f
        const val ZOOM_MAX = 4f
    }
}
