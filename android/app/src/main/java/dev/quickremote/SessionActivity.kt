package dev.quickremote

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.view.Gravity
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.TextView
import org.json.JSONObject

/** A running session: the picture fills the screen; touch drives the mouse. */
class SessionActivity : Activity(), SurfaceHolder.Callback {
    private var h = 0L
    private val stats = Stats()
    private var video: VideoDecoder? = null
    private var audio: AudioPlayer? = null
    private lateinit var surface: SurfaceView
    private lateinit var overlay: TextView
    private var streamW = 1920
    private var streamH = 1080
    private var trackpad = true
    private var lastX = 0f
    private var lastY = 0f
    private var downAt = 0L
    private var moved = false
    private val ui = Handler(Looper.getMainLooper())
    private val pads = HashMap<Int, Int>() // device id → pad index

    override fun onCreate(b: Bundle?) {
        super.onCreate(b)
        h = intent.getLongExtra("handle", 0)
        surface = SurfaceView(this).apply { holder.addCallback(this@SessionActivity); isFocusable = true; isFocusableInTouchMode = true }
        overlay = TextView(this).apply { setTextColor(0xFFE6E9EE.toInt()); setBackgroundColor(0xA010141A.toInt()); setPadding(16, 8, 16, 8); textSize = 12f; visibility = View.GONE }
        val root = FrameLayout(this)
        root.addView(surface, FrameLayout.LayoutParams(-1, -1, Gravity.CENTER))
        root.addView(overlay, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START))
        setContentView(root)
        window.insetsController?.let {
            it.hide(WindowInsets.Type.systemBars())
            it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        pollEvents()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        video = VideoDecoder(h, holder.surface, stats).also { it.start() }
        audio = AudioPlayer(h).also { it.start() }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        video?.running = false
        video?.join(500)
        video = null
    }

    override fun onDestroy() {
        audio?.running = false
        Native.close(h)
        super.onDestroy()
    }

    private fun pollEvents() {
        while (true) {
            val e = Native.pollEvent(h) ?: break
            val j = JSONObject(e)
            when (j.getString("t")) {
                "stream" -> { streamW = j.getInt("width"); streamH = j.getInt("height"); fitSurface() }
                "rumble" -> rumble(j.getInt("idx"), j.getInt("strong"), j.getInt("weak"), j.getInt("ms"))
                "clipboard" -> (getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager)
                    .setPrimaryClip(ClipData.newPlainText("QuickRemote", j.getString("text")))
                "error", "ended" -> { overlay.visibility = View.VISIBLE; overlay.text = j.optString("text"); ui.postDelayed({ finish() }, 2500) }
            }
        }
        if (overlay.visibility == View.VISIBLE && !isFinishing) {
            overlay.text = "%.0f fps · %s · decode %.1f ms · capture→shown %.1f ms · %s".format(
                stats.fps, stats.codec, stats.decodeUs / 1000f, stats.latencyMs, if (trackpad) "trackpad" else "direct touch")
        }
        if (!isFinishing) ui.postDelayed({ pollEvents() }, 100)
    }

    private fun fitSurface() {
        val v = window.decorView
        val s = minOf(v.width.toFloat() / streamW, v.height.toFloat() / streamH)
        surface.layoutParams = FrameLayout.LayoutParams((streamW * s).toInt(), (streamH * s).toInt(), Gravity.CENTER)
    }

    private fun toStream(x: Float, y: Float): Pair<Int, Int> {
        val sx = x / surface.width * streamW
        val sy = y / surface.height * streamH
        return Pair(sx.toInt().coerceIn(0, streamW - 1), sy.toInt().coerceIn(0, streamH - 1))
    }

    override fun onTouchEvent(ev: MotionEvent): Boolean = handleTouch(ev)

    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        val loc = IntArray(2)
        surface.getLocationOnScreen(loc)
        val e = MotionEvent.obtain(ev)
        e.offsetLocation(-loc[0].toFloat(), -loc[1].toFloat())
        val r = handleTouch(e)
        e.recycle()
        return r
    }

    private fun handleTouch(ev: MotionEvent): Boolean {
        if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_STYLUS || ev.getToolType(0) == MotionEvent.TOOL_TYPE_ERASER) {
            val (x, y) = toStream(ev.x, ev.y)
            var flags = 1
            if (ev.action != MotionEvent.ACTION_UP && ev.action != MotionEvent.ACTION_HOVER_EXIT && ev.pressure > 0) flags = flags or 2
            if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_ERASER) flags = flags or 4
            if (ev.buttonState and MotionEvent.BUTTON_STYLUS_PRIMARY != 0) flags = flags or 8
            val tilt = Math.toDegrees(ev.getAxisValue(MotionEvent.AXIS_TILT).toDouble())
            val orient = ev.getAxisValue(MotionEvent.AXIS_ORIENTATION).toDouble()
            Native.pen(h, x * 16, y * 16, (ev.pressure * 65535).toInt(), (tilt * Math.sin(orient)).toInt(), (-tilt * Math.cos(orient)).toInt(), flags)
            return true
        }
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> { lastX = ev.x; lastY = ev.y; downAt = ev.eventTime; moved = false
                if (!trackpad) { val (x, y) = toStream(ev.x, ev.y); Native.abs(h, x, y); Native.button(h, 1, true) } }
            MotionEvent.ACTION_MOVE -> {
                if (ev.pointerCount >= 2) {
                    // Two-finger drag scrolls.
                    Native.wheel(h, 0, ((ev.y - lastY) * 4).toInt()); lastY = ev.y; moved = true
                } else if (trackpad) {
                    val dx = ev.x - lastX; val dy = ev.y - lastY
                    if (Math.abs(dx) + Math.abs(dy) > 0.5f) { Native.rel(h, dx * 1.6f, dy * 1.6f); moved = true }
                    lastX = ev.x; lastY = ev.y
                } else { val (x, y) = toStream(ev.x, ev.y); Native.abs(h, x, y) }
            }
            MotionEvent.ACTION_UP -> {
                if (trackpad) {
                    if (!moved && ev.eventTime - downAt < 250) { Native.button(h, 1, true); Native.button(h, 1, false) }
                } else Native.button(h, 1, false)
            }
            MotionEvent.ACTION_POINTER_DOWN -> if (ev.pointerCount == 3) toggleKeyboard()
            MotionEvent.ACTION_POINTER_UP -> if (ev.pointerCount == 2 && !moved && trackpad) { Native.button(h, 3, true); Native.button(h, 3, false) }
        }
        return true
    }

    private fun toggleKeyboard() {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        surface.requestFocus()
        imm.showSoftInput(surface, InputMethodManager.SHOW_FORCED)
    }

    // Hardware keyboards, TV remotes and gamepad buttons.
    override fun dispatchKeyEvent(e: KeyEvent): Boolean {
        val src = e.source
        if (src and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD || src and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK) {
            padButton(e); return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_BACK && e.action == KeyEvent.ACTION_UP && e.isLongPress.not() && src and InputDevice.SOURCE_KEYBOARD == 0) {
            finish(); return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_F1 && e.isCtrlPressed && e.isAltPressed && e.action == KeyEvent.ACTION_DOWN) {
            overlay.visibility = if (overlay.visibility == View.VISIBLE) View.GONE else View.VISIBLE; return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_F2 && e.isCtrlPressed && e.isAltPressed && e.action == KeyEvent.ACTION_DOWN) {
            trackpad = !trackpad; return true
        }
        val code = Keys.evdev(e.keyCode)
        if (code != null && e.action != KeyEvent.ACTION_MULTIPLE) {
            Native.key(h, code, e.action == KeyEvent.ACTION_DOWN); return true
        }
        // Soft keyboards deliver composed text as ACTION_MULTIPLE characters.
        if (e.action == KeyEvent.ACTION_MULTIPLE && e.characters != null) { Native.text(h, e.characters); return true }
        val ch = e.unicodeChar
        if (ch != 0 && e.action == KeyEvent.ACTION_DOWN) { Native.text(h, String(Character.toChars(ch))); return true }
        return super.dispatchKeyEvent(e)
    }

    private val padState = HashMap<Int, Pair<Int, ShortArray>>()

    private fun padIndex(deviceId: Int): Int = pads.getOrPut(deviceId) { (0..3).first { it !in pads.values } }

    private fun padButton(e: KeyEvent) {
        val idx = padIndex(e.deviceId)
        val bit = when (e.keyCode) {
            KeyEvent.KEYCODE_BUTTON_A -> 0; KeyEvent.KEYCODE_BUTTON_B -> 1; KeyEvent.KEYCODE_BUTTON_X -> 2; KeyEvent.KEYCODE_BUTTON_Y -> 3
            KeyEvent.KEYCODE_BUTTON_L1 -> 4; KeyEvent.KEYCODE_BUTTON_R1 -> 5; KeyEvent.KEYCODE_BUTTON_SELECT -> 6; KeyEvent.KEYCODE_BUTTON_START -> 7
            KeyEvent.KEYCODE_BUTTON_MODE -> 8; KeyEvent.KEYCODE_BUTTON_THUMBL -> 9; KeyEvent.KEYCODE_BUTTON_THUMBR -> 10
            KeyEvent.KEYCODE_DPAD_UP -> 11; KeyEvent.KEYCODE_DPAD_DOWN -> 12; KeyEvent.KEYCODE_DPAD_LEFT -> 13; KeyEvent.KEYCODE_DPAD_RIGHT -> 14
            else -> return
        }
        val (b, axes) = padState[idx] ?: Pair(0, ShortArray(6))
        val nb = if (e.action == KeyEvent.ACTION_DOWN) b or (1 shl bit) else b and (1 shl bit).inv()
        padState[idx] = Pair(nb, axes)
        Native.pad(h, idx, nb, axes)
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        if (e.source and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK && e.action == MotionEvent.ACTION_MOVE) {
            val idx = padIndex(e.deviceId)
            val s = { v: Float -> (v * 32767).toInt().coerceIn(-32768, 32767).toShort() }
            val axes = shortArrayOf(
                s(e.getAxisValue(MotionEvent.AXIS_X)), s(e.getAxisValue(MotionEvent.AXIS_Y)),
                s(e.getAxisValue(MotionEvent.AXIS_Z)), s(e.getAxisValue(MotionEvent.AXIS_RZ)),
                s(maxOf(e.getAxisValue(MotionEvent.AXIS_LTRIGGER), e.getAxisValue(MotionEvent.AXIS_BRAKE))),
                s(maxOf(e.getAxisValue(MotionEvent.AXIS_RTRIGGER), e.getAxisValue(MotionEvent.AXIS_GAS))))
            var b = padState[idx]?.first ?: 0
            // Hat as d-pad bits.
            val hx = e.getAxisValue(MotionEvent.AXIS_HAT_X); val hy = e.getAxisValue(MotionEvent.AXIS_HAT_Y)
            b = b and (0xF shl 11).inv()
            if (hy < -0.5f) b = b or (1 shl 11); if (hy > 0.5f) b = b or (1 shl 12)
            if (hx < -0.5f) b = b or (1 shl 13); if (hx > 0.5f) b = b or (1 shl 14)
            padState[idx] = Pair(b, axes)
            Native.pad(h, idx, b, axes)
            return true
        }
        if (e.source and InputDevice.SOURCE_MOUSE == InputDevice.SOURCE_MOUSE) {
            when (e.actionMasked) {
                MotionEvent.ACTION_HOVER_MOVE, MotionEvent.ACTION_MOVE -> { val (x, y) = toStream(e.x, e.y); Native.abs(h, x, y) }
                MotionEvent.ACTION_SCROLL -> Native.wheel(h, (e.getAxisValue(MotionEvent.AXIS_HSCROLL) * 120).toInt(), (e.getAxisValue(MotionEvent.AXIS_VSCROLL) * 120).toInt())
                MotionEvent.ACTION_BUTTON_PRESS, MotionEvent.ACTION_BUTTON_RELEASE -> {
                    val b = when (e.actionButton) { MotionEvent.BUTTON_SECONDARY -> 3; MotionEvent.BUTTON_TERTIARY -> 2; MotionEvent.BUTTON_BACK -> 4; MotionEvent.BUTTON_FORWARD -> 5; else -> 1 }
                    Native.button(h, b, e.actionMasked == MotionEvent.ACTION_BUTTON_PRESS)
                }
            }
            return true
        }
        return super.onGenericMotionEvent(e)
    }

    private fun rumble(idx: Int, strong: Int, weak: Int, ms: Int) {
        val dev = pads.entries.firstOrNull { it.value == idx }?.key ?: return
        val vm = InputDevice.getDevice(dev)?.vibratorManager ?: return
        val amp = (maxOf(strong, weak) / 257).coerceIn(1, 255)
        vm.defaultVibrator.vibrate(VibrationEffect.createOneShot(ms.toLong().coerceAtLeast(1), amp))
    }
}
