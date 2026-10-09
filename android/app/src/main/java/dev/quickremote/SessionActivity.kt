package dev.quickremote

import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.os.VibrationEffect
import android.text.InputType
import android.view.Gravity
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.TextView
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * A running session: the picture sits on the screen through a view
 * transform (Fit / Fill / 100%, pinch zoom, pan); touch drives the mouse;
 * a toolbar holds Keyboard, Zoom and Display; a dropped connection
 * reconnects over the last picture.
 */
class SessionActivity : Activity(), SurfaceHolder.Callback {
    @Volatile private var h = 0L
    private val stats = Stats()
    private var video: VideoDecoder? = null
    private var audio: AudioPlayer? = null
    private lateinit var settings: Settings
    private val view = ViewTransform()
    private lateinit var stage: FrameLayout
    private lateinit var surface: SurfaceView
    private lateinit var keyLayer: KeyLayer
    private lateinit var overlay: TextView
    private lateinit var banner: TextView
    private lateinit var toolbar: LinearLayout
    private lateinit var zoomBtn: Button
    private lateinit var keys: HorizontalScrollView
    private val modButtons = HashMap<Int, Button>()
    private val sticky = LinkedHashSet<Int>()
    private var keyboardShown = false
    private var streamW = 1920
    private var streamH = 1080
    private val ui = Handler(Looper.getMainLooper())
    private val pads = HashMap<Int, Int>() // device id → pad index
    // What the host said about its display (kind −1: nothing yet).
    private var displayKind = -1
    private var displayNote = ""
    private var lastRequest: String? = null
    private val requestDisplayNow = Runnable { sendDisplay() }
    // Reconnecting.
    private var target: String? = null
    private var reconnecting = false
    private var surfaceReady = false

    override fun onCreate(b: Bundle?) {
        super.onCreate(b)
        h = intent.getLongExtra("handle", 0)
        target = intent.getStringExtra("target")
        settings = Settings(this)
        view.mode = settings.view
        surface = SurfaceView(this).apply { holder.addCallback(this@SessionActivity) }
        keyLayer = KeyLayer(this).apply { isFocusable = true; isFocusableInTouchMode = true }
        stage = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        stage.addView(surface, FrameLayout.LayoutParams(1, 1))
        stage.addView(keyLayer, FrameLayout.LayoutParams(-1, -1))
        overlay = TextView(this).apply { setTextColor(0xFFE6E9EE.toInt()); setBackgroundColor(0xA010141A.toInt()); setPadding(16, 8, 16, 8); textSize = 12f; visibility = View.GONE }
        banner = TextView(this).apply { setTextColor(0xFFF0B75A.toInt()); setBackgroundColor(0xE010141A.toInt()); setPadding(28, 12, 28, 12); textSize = 15f; visibility = View.GONE }
        toolbar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(barButton("⌨", "Keyboard") { toggleKeyboard() })
            zoomBtn = barButton("100%", "Reset zoom") { view.setZoom(1f); placePicture() }
            addView(zoomBtn)
            addView(barButton("Display", "Display settings") { showDisplayDialog() })
            // Touch screens only (not TVs).
            visibility = if (packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN)) View.VISIBLE else View.GONE
        }
        keys = buildKeysStrip().apply { visibility = View.GONE }

        val column = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        column.addView(stage, LinearLayout.LayoutParams(-1, 0, 1f))
        column.addView(keys, LinearLayout.LayoutParams(-1, -2))
        val root = FrameLayout(this)
        root.addView(column, FrameLayout.LayoutParams(-1, -1))
        root.addView(overlay, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START))
        root.addView(banner, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.CENTER_HORIZONTAL).apply { topMargin = 24 })
        root.addView(toolbar, FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.END).apply { topMargin = 12; rightMargin = 12 })
        setContentView(root)
        // Relayout on every size change (first layout, rotation, split
        // screen, the keyboard); a new screen size is a display request.
        stage.addOnLayoutChangeListener { _, l, t, r, b, ol, ot, or_, ob ->
            if (r - l != or_ - ol || b - t != ob - ot) { placePicture(); if (settings.follow) requestDisplay() }
        }
        keyLayer.setOnTouchListener { _, ev -> handleTouch(ev) }
        window.insetsController?.let {
            it.hide(WindowInsets.Type.systemBars())
            it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        pollEvents()
    }

    private fun barButton(label: String, desc: String, click: () -> Unit) = Button(this).apply {
        text = label; contentDescription = desc; isAllCaps = false; minWidth = 0; minimumWidth = 0
        setTextColor(0xFFE6E9EE.toInt()); setBackgroundColor(0xE010141A.toInt()); setPadding(28, 8, 28, 8)
        setOnClickListener { click() }
        layoutParams = LinearLayout.LayoutParams(-2, -2).apply { leftMargin = 10 }
    }

    // ---- Picture placement and display requests ----

    private fun density() = resources.displayMetrics.density

    /** Places the surface where the view transform puts the picture. */
    private fun placePicture() {
        if (stage.width == 0 || stage.height == 0) return
        view.natural = density() / settings.ratio(density())
        view.layout(stage.width, stage.height, streamW, streamH)
        val lp = FrameLayout.LayoutParams(Math.round(view.width), Math.round(view.height))
        lp.leftMargin = Math.round(view.x); lp.topMargin = Math.round(view.y)
        surface.layoutParams = lp
        zoomBtn.text = "%d%%".format(Math.round(view.zoom * 100))
    }

    /** What to ask the host for: the whole screen (not less the keyboard) or the fixed size. */
    private fun wanted(): IntArray {
        val ratio = settings.ratio(density())
        val bounds = windowManager.currentWindowMetrics.bounds
        val (w, hh) = if (settings.follow) Settings.clampSize(bounds.width() / density() * ratio, bounds.height() / density() * ratio)
            else Settings.clampSize(settings.fixedW.toFloat(), settings.fixedH.toFloat())
        return intArrayOf(w, hh, settings.hz, Math.round(ratio * 100), if (settings.real) 1 else 0)
    }

    /** Asks the host for the display we want, 300 ms after the last change. */
    private fun requestDisplay(now: Boolean = false) {
        ui.removeCallbacks(requestDisplayNow)
        ui.postDelayed(requestDisplayNow, if (now) 0 else 300)
    }

    private fun sendDisplay() {
        if (h == 0L || reconnecting || displayKind < 0 || displayKind == KIND_FIXED) return
        val w = wanted()
        val key = w.joinToString(",")
        if (key == lastRequest) return
        lastRequest = key
        android.util.Log.i("QuickRemote", "display request ${w[0]}x${w[1]} ${w[2]} Hz scale ${w[3]}% target ${w[4]}")
        Native.displayRequest(h, w[0], w[1], w[2], w[3], w[4])
    }

    override fun onConfigurationChanged(c: Configuration) {
        super.onConfigurationChanged(c)
        android.util.Log.i("QuickRemote", "orientation ${if (c.orientation == Configuration.ORIENTATION_PORTRAIT) "portrait" else "landscape"}")
        if (settings.follow) requestDisplay()
    }

    // ---- Lifecycle ----

    override fun surfaceCreated(holder: SurfaceHolder) {
        android.util.Log.i("QuickRemote", "surface ready")
        surfaceReady = true
        if (h != 0L && !reconnecting) startMedia()
    }

    private fun startMedia() {
        video = VideoDecoder(h, surface.holder.surface, stats).also { it.start() }
        if (audio == null) audio = AudioPlayer(h).also { it.start() }
    }

    private fun stopMedia() {
        video?.running = false
        video?.join(500)
        video = null
        audio?.running = false
        audio = null
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady = false
        video?.running = false
        video?.join(500)
        video = null
    }

    // The host sends no video while this is in the background.
    override fun onPause() {
        super.onPause()
        if (h != 0L) Native.pause(h, true)
    }

    override fun onResume() {
        super.onResume()
        if (h != 0L) Native.pause(h, false)
    }

    override fun onDestroy() {
        android.util.Log.i("QuickRemote", "session view closed (finishing=$isFinishing, config change=$isChangingConfigurations)")
        audio?.running = false
        // A configuration change recreates this activity with the same
        // session handle (in its intent): keep the session for it.
        if (!isChangingConfigurations && h != 0L) Native.close(h)
        h = 0
        super.onDestroy()
    }

    private fun pollEvents() {
        while (h != 0L && !reconnecting) {
            val e = Native.pollEvent(h) ?: break
            val j = JSONObject(e)
            when (j.getString("t")) {
                "stream" -> {
                    streamW = j.getInt("width"); streamH = j.getInt("height")
                    android.util.Log.i("QuickRemote", "stream ${streamW}x$streamH")
                    placePicture()
                }
                "display" -> {
                    val first = displayKind < 0
                    displayKind = j.getInt("kind"); displayNote = j.optString("note")
                    android.util.Log.i("QuickRemote", "display kind $displayKind ${j.getInt("width")}x${j.getInt("height")} scale ${j.getInt("scale")}% $displayNote")
                    if (first) requestDisplay(true)
                }
                "rumble" -> rumble(j.getInt("idx"), j.getInt("strong"), j.getInt("weak"), j.getInt("ms"))
                "clipboard" -> (getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager)
                    .setPrimaryClip(ClipData.newPlainText("QuickRemote", j.getString("text")))
                // The host ended the session and said why: no reconnecting.
                "error" -> {
                    android.util.Log.i("QuickRemote", "session error: ${j.optString("text")}")
                    overlay.visibility = View.VISIBLE; overlay.text = j.optString("text"); ui.postDelayed({ finish() }, 2500)
                }
                "ended" -> {
                    android.util.Log.i("QuickRemote", "session ended: ${j.optString("text")}")
                    if (target != null && !isFinishing) reconnect() else { overlay.visibility = View.VISIBLE; overlay.text = j.optString("text"); ui.postDelayed({ finish() }, 2500) }
                }
            }
        }
        if (overlay.visibility == View.VISIBLE && !isFinishing && !reconnecting) {
            overlay.text = "%.0f fps · %s · decode %.1f ms · capture→shown %.1f ms · %s · zoom %d%%".format(
                stats.fps, stats.codec, stats.decodeUs / 1000f, stats.latencyMs, if (settings.trackpad) "trackpad" else "direct touch",
                Math.round(view.zoom * 100))
        }
        if (!isFinishing) ui.postDelayed({ pollEvents() }, 100)
    }

    /** Keeps the last picture and connects again after 1, 2, 4, 8, then every 10 s. */
    private fun reconnect() {
        reconnecting = true
        banner.text = "Reconnecting…"; banner.visibility = View.VISIBLE
        val old = h
        stopMedia()
        h = 0
        Native.close(old)
        val bounds = windowManager.currentWindowMetrics.bounds
        val (w, hh) = Pair(maxOf(bounds.width(), bounds.height()), minOf(bounds.width(), bounds.height()))
        thread(name = "qr-reconnect") {
            var attempt = 0
            while (!isFinishing && !isDestroyed) {
                Thread.sleep(1000L * listOf(1, 2, 4, 8, 10)[minOf(attempt, 4)])
                attempt++
                android.util.Log.i("QuickRemote", "reconnect attempt $attempt")
                try {
                    val nh = Native.connect(filesDir.absolutePath, target!!, null, android.os.Build.MODEL, null, hardwareCodecs(), w, hh)
                    runOnUiThread {
                        if (isFinishing || isDestroyed) { Native.close(nh); return@runOnUiThread }
                        h = nh; reconnecting = false; displayKind = -1; lastRequest = null
                        banner.visibility = View.GONE
                        android.util.Log.i("QuickRemote", "reconnected after $attempt attempts")
                        if (surfaceReady) startMedia()
                    }
                    return@thread
                } catch (e: IllegalStateException) {
                    val msg = e.message ?: ""
                    android.util.Log.i("QuickRemote", "reconnect failed: $msg")
                    // Not paired any more: asking for a PIN is the host list's job.
                    if (msg.contains("PIN") || msg.contains("pair")) {
                        runOnUiThread { banner.text = msg; ui.postDelayed({ finish() }, 2500) }
                        return@thread
                    }
                    runOnUiThread { banner.text = "Reconnecting… (attempt ${attempt + 1})" }
                }
            }
        }
    }

    // ---- Touch: tap = left click, long press = right click, two-finger tap =
    // right click, two-finger drag = scroll (or pan while zoomed in),
    // three-finger tap = middle click, four-finger tap = keyboard, pinch =
    // view zoom. One finger moves the pointer like a trackpad, or in direct
    // mode puts it under the finger and drags. ----

    private var gestureStart = 0L
    private var maxFingers = 0
    private var moved = false
    private var dragging = false
    private var longFired = false
    private var pinching = false
    private var lastX = 0f
    private var lastY = 0f
    private var downX = 0f
    private var downY = 0f
    private var pinchDist = 0f
    private var pinchX = 0f
    private var pinchY = 0f
    // Where the fingers' midpoint was when the last finger landed.
    private var twoX = 0f
    private var twoY = 0f
    private var wheelX = 0f
    private var wheelY = 0f
    private val longPress = Runnable {
        if (!moved && maxFingers == 1) {
            longFired = true
            android.util.Log.i("QuickRemote", "gesture long-press")
            click(3)
        }
    }

    private fun click(b: Int) { sendButton(b, true); sendButton(b, false) }

    private fun sendButton(b: Int, down: Boolean) { if (h != 0L) Native.button(h, b, down) }

    private fun spread(ev: MotionEvent): Float = Math.hypot((ev.getX(0) - ev.getX(1)).toDouble(), (ev.getY(0) - ev.getY(1)).toDouble()).toFloat()

    private fun midX(ev: MotionEvent): Float { var s = 0f; for (i in 0 until ev.pointerCount) s += ev.getX(i); return s / ev.pointerCount }
    private fun midY(ev: MotionEvent): Float { var s = 0f; for (i in 0 until ev.pointerCount) s += ev.getY(i); return s / ev.pointerCount }

    private fun startPinch(ev: MotionEvent) {
        if (ev.pointerCount < 2) return
        pinchDist = spread(ev); pinchX = midX(ev); pinchY = midY(ev)
    }

    private fun handleTouch(ev: MotionEvent): Boolean {
        if (h == 0L) return true
        if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_STYLUS || ev.getToolType(0) == MotionEvent.TOOL_TYPE_ERASER) {
            val (x, y) = view.toRemote(ev.x, ev.y)
            var flags = 1
            if (ev.action != MotionEvent.ACTION_UP && ev.action != MotionEvent.ACTION_HOVER_EXIT && ev.pressure > 0) flags = flags or 2
            if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_ERASER) flags = flags or 4
            if (ev.buttonState and MotionEvent.BUTTON_STYLUS_PRIMARY != 0) flags = flags or 8
            val tilt = Math.toDegrees(ev.getAxisValue(MotionEvent.AXIS_TILT).toDouble())
            val orient = ev.getAxisValue(MotionEvent.AXIS_ORIENTATION).toDouble()
            Native.pen(h, x * 16, y * 16, (ev.pressure * 65535).toInt(), (tilt * Math.sin(orient)).toInt(), (-tilt * Math.cos(orient)).toInt(), flags)
            return true
        }
        val trackpad = settings.trackpad
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                gestureStart = ev.eventTime; maxFingers = 1; moved = false; dragging = false; longFired = false; pinching = false
                lastX = ev.x; lastY = ev.y; downX = ev.x; downY = ev.y
                if (!trackpad) { val (x, y) = view.toRemote(ev.x, ev.y); Native.abs(h, x, y) }
                ui.postDelayed(longPress, LONG_PRESS_MS)
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                maxFingers = maxOf(maxFingers, ev.pointerCount)
                ui.removeCallbacks(longPress)
                if (dragging) { sendButton(1, false); dragging = false }
                startPinch(ev)
                twoX = pinchX; twoY = pinchY
            }
            MotionEvent.ACTION_MOVE -> {
                if (ev.pointerCount == 1 && maxFingers == 1) {
                    if (Math.hypot((ev.x - downX).toDouble(), (ev.y - downY).toDouble()) > SLOP_PX * density()) moved = true
                    if (!moved || longFired) return true
                    ui.removeCallbacks(longPress)
                    if (trackpad) {
                        Native.rel(h, (ev.x - lastX) * 1.6f, (ev.y - lastY) * 1.6f)
                    } else {
                        if (!dragging) {
                            // The drag starts where the finger first touched.
                            val (x0, y0) = view.toRemote(downX, downY); Native.abs(h, x0, y0)
                            sendButton(1, true); dragging = true
                        }
                        val (x, y) = view.toRemote(ev.x, ev.y); Native.abs(h, x, y)
                    }
                    lastX = ev.x; lastY = ev.y
                } else if (ev.pointerCount == 2) {
                    val d = spread(ev); val mx = midX(ev); val my = midY(ev)
                    if (Math.hypot((mx - twoX).toDouble(), (my - twoY).toDouble()) > SLOP_PX * density()) moved = true
                    if (!pinching && pinchDist > 0 && Math.abs(d - pinchDist) > pinchDist * 0.08f) pinching = true
                    if (pinching && pinchDist > 0) {
                        // Pinch: zoom around the fingers' midpoint, and follow it.
                        view.setZoom(view.zoom * (d / pinchDist), mx, my)
                        pinchDist = d
                    }
                    val dx = mx - pinchX; val dy = my - pinchY
                    if (view.pans()) view.panBy(dx, dy)
                    else if (!pinching) {
                        wheelX += dx * 4; wheelY += dy * 4
                        val wx = wheelX.toInt(); val wy = wheelY.toInt()
                        if (wx != 0 || wy != 0) { Native.wheel(h, wx, wy); wheelX -= wx; wheelY -= wy }
                    }
                    pinchX = mx; pinchY = my
                    if (pinching || view.pans()) placePicture()
                }
            }
            MotionEvent.ACTION_POINTER_UP -> {
                // The fingers left after this one is lifted.
                if (ev.pointerCount - 1 >= 2) {
                    val rest = (0 until ev.pointerCount).filter { it != ev.actionIndex }
                    pinchX = rest.map { ev.getX(it) }.average().toFloat(); pinchY = rest.map { ev.getY(it) }.average().toFloat()
                    pinchDist = Math.hypot((ev.getX(rest[0]) - ev.getX(rest[1])).toDouble(), (ev.getY(rest[0]) - ev.getY(rest[1])).toDouble()).toFloat()
                } else if (ev.pointerCount - 1 == 1) {
                    val i = if (ev.actionIndex == 0) 1 else 0
                    lastX = ev.getX(i); lastY = ev.getY(i)
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                ui.removeCallbacks(longPress)
                if (dragging) { sendButton(1, false); dragging = false; return true }
                if (ev.actionMasked == MotionEvent.ACTION_CANCEL) return true
                if (pinching) android.util.Log.i("QuickRemote", "view zoom %d%%".format(Math.round(view.zoom * 100)))
                if (moved || longFired || pinching || ev.eventTime - gestureStart > TAP_MS + 200) return true
                if (maxFingers == 1 && !trackpad) { val (x, y) = view.toRemote(ev.x, ev.y); Native.abs(h, x, y) }
                android.util.Log.i("QuickRemote", "gesture tap $maxFingers")
                when (maxFingers) {
                    1 -> click(1)
                    2 -> click(3)
                    3 -> click(2)
                    else -> toggleKeyboard()
                }
            }
        }
        return true
    }

    // ---- Keyboard and the extra-keys strip ----

    private fun toggleKeyboard() {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        keyboardShown = !keyboardShown
        android.util.Log.i("QuickRemote", "keyboard ${if (keyboardShown) "shown" else "hidden"}")
        keys.visibility = if (keyboardShown) View.VISIBLE else View.GONE
        if (keyboardShown) {
            keyLayer.requestFocus()
            imm.restartInput(keyLayer)
            imm.showSoftInput(keyLayer, InputMethodManager.SHOW_IMPLICIT)
        } else imm.hideSoftInputFromWindow(keyLayer.windowToken, 0)
    }

    private fun buildKeysStrip(): HorizontalScrollView {
        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; setPadding(8, 6, 8, 6); setBackgroundColor(0xF010141A.toInt()) }
        fun key(label: String, code: Int) = row.addView(barButton(label, label) { pressKey(code) })
        fun mod(label: String, code: Int) = row.addView(barButton(label, label) { toggleMod(code) }.also { modButtons[code] = it })
        key("Esc", 1); key("Tab", 15); mod("Ctrl", 29); mod("Alt", 56); mod("Super", 125)
        key("←", 105); key("↑", 103); key("↓", 108); key("→", 106)
        key("Home", 102); key("End", 107); key("PgUp", 104); key("PgDn", 109)
        return HorizontalScrollView(this).apply { addView(row); setBackgroundColor(0xF010141A.toInt()) }
    }

    private fun toggleMod(code: Int) {
        if (!sticky.remove(code)) sticky.add(code)
        modButtons[code]?.setTextColor(if (code in sticky) 0xFF6AA8FF.toInt() else 0xFFE6E9EE.toInt())
    }

    /** A key with the sticky modifiers held, which then release. */
    private fun pressKey(code: Int) {
        if (h == 0L) return
        for (m in sticky) Native.key(h, m, true)
        Native.key(h, code, true); Native.key(h, code, false)
        for (m in sticky.reversed()) Native.key(h, m, false)
        if (sticky.isNotEmpty()) {
            android.util.Log.i("QuickRemote", "key $code with modifiers $sticky")
            for (m in sticky.toList()) toggleMod(m)
        }
    }

    /** The evdev key for a typed character, for sticky-modifier combinations. */
    private fun charKey(c: Char): Int? = when (c.lowercaseChar()) {
        in 'a'..'z' -> Keys.evdev(KeyEvent.KEYCODE_A + (c.lowercaseChar() - 'a'))
        in '0'..'9' -> Keys.evdev(KeyEvent.KEYCODE_0 + (c - '0'))
        ' ' -> 57
        else -> null
    }

    private fun typeText(text: String) {
        if (h == 0L) return
        val k = if (sticky.isNotEmpty() && text.length == 1) charKey(text[0]) else null
        if (k != null) pressKey(k) else Native.text(h, text)
    }

    /** Receives the soft keyboard: raw key events, and committed text. */
    inner class KeyLayer(ctx: Context) : View(ctx) {
        // Only while asked for: a text editor in focus brings the keyboard
        // up by itself, over the picture.
        override fun onCheckIsTextEditor() = keyboardShown
        override fun onCreateInputConnection(out: EditorInfo): InputConnection {
            out.inputType = InputType.TYPE_NULL
            out.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI
            return object : BaseInputConnection(this, false) {
                override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                    typeText(text.toString()); return true
                }
            }
        }
    }

    // ---- The Display dialog ----

    private fun showDisplayDialog() {
        val pad = (16 * density()).toInt()
        val col = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(pad, pad / 2, pad, 0) }
        fun heading(t: String) = col.addView(TextView(this).apply { text = t; textSize = 12f; isAllCaps = true; setPadding(0, pad, 0, 4) })
        fun group(vararg labels: String, checked: Int): RadioGroup = RadioGroup(this).apply {
            orientation = RadioGroup.VERTICAL
            labels.forEachIndexed { i, l -> addView(RadioButton(this@SessionActivity).apply { text = l; id = 1000 + i }) }
            check(1000 + checked)
            col.addView(this)
        }
        fun RadioGroup.index() = checkedRadioButtonId - 1000

        val fixedDesktop = displayKind == KIND_FIXED
        heading("Resolution")
        val mode = group("Follow screen", "Fixed", checked = if (settings.follow) 0 else 1)
        val presetIdx = Settings.PRESETS.indexOf(settings.fixedW to settings.fixedH)
        val preset = Spinner(this).apply {
            adapter = ArrayAdapter(this@SessionActivity, android.R.layout.simple_spinner_dropdown_item,
                Settings.PRESETS.map { "${it.first} × ${it.second}" } + "Custom")
            setSelection(if (presetIdx >= 0) presetIdx else Settings.PRESETS.size)
        }
        col.addView(preset)
        val customRow = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        val cw = EditText(this).apply { inputType = InputType.TYPE_CLASS_NUMBER; setText(settings.fixedW.toString()); hint = "Width"; minEms = 4 }
        val ch = EditText(this).apply { inputType = InputType.TYPE_CLASS_NUMBER; setText(settings.fixedH.toString()); hint = "Height"; minEms = 4 }
        customRow.addView(cw); customRow.addView(TextView(this).apply { text = " × " }); customRow.addView(ch)
        col.addView(customRow)
        val hz = Spinner(this).apply {
            adapter = ArrayAdapter(this@SessionActivity, android.R.layout.simple_spinner_dropdown_item, Settings.RATES.map { "$it Hz" })
            setSelection(Settings.RATES.indexOf(settings.hz).coerceAtLeast(0))
        }
        col.addView(hz)
        if (fixedDesktop) {
            for (v in listOf<View>(mode, preset, cw, ch, hz)) v.isEnabled = false
            for (i in 0 until mode.childCount) mode.getChildAt(i).isEnabled = false
        }

        heading("Pixel ratio")
        val ratioIdx = if (settings.ratio <= 0f) 0 else Settings.RATIOS.indexOf(settings.ratio).coerceAtLeast(0) + 1
        val ratio = group("Auto (this screen: %.2f×, used %.2f×)".format(density(), density().coerceIn(1f, 2f)),
            "0.5× (fastest, softest)", "0.75× (faster)", "1×", "1.5×", "2×", checked = ratioIdx)

        heading("Monitor")
        val tgt = group("Virtual monitor of its own", "The desktop's real monitor", checked = if (settings.real) 1 else 0)
        if (displayKind != KIND_KDE && displayKind != KIND_GNOME) for (i in 0 until tgt.childCount) tgt.getChildAt(i).isEnabled = false

        heading("View")
        val vm = group("Fit", "Fill screen", "100%", checked = settings.view.ordinal)

        heading("Touch")
        val touch = group("Trackpad (drag moves the pointer)", "Direct (the pointer goes under the finger)", checked = if (settings.trackpad) 0 else 1)

        if (displayNote.isNotEmpty()) col.addView(TextView(this).apply { text = displayNote; setTextColor(0xFFF0B75A.toInt()); setPadding(0, pad, 0, 0) })

        AlertDialog.Builder(this)
            .setTitle("Display")
            .setView(ScrollView(this).apply { addView(col) })
            .setPositiveButton("Apply") { _, _ ->
                settings.follow = mode.index() == 0
                if (preset.selectedItemPosition < Settings.PRESETS.size) {
                    val (w, hh) = Settings.PRESETS[preset.selectedItemPosition]
                    settings.fixedW = w; settings.fixedH = hh
                } else {
                    val (w, hh) = Settings.clampSize(cw.text.toString().toFloatOrNull() ?: 1920f, ch.text.toString().toFloatOrNull() ?: 1080f)
                    settings.fixedW = w; settings.fixedH = hh
                }
                settings.hz = Settings.RATES[hz.selectedItemPosition.coerceAtLeast(0)]
                settings.ratio = if (ratio.index() <= 0) 0f else Settings.RATIOS[ratio.index() - 1]
                settings.real = tgt.index() == 1
                settings.view = ViewMode.entries[vm.index().coerceAtLeast(0)]
                settings.trackpad = touch.index() != 1
                settings.save()
                view.mode = settings.view
                placePicture()
                requestDisplay(true)
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    // ---- Hardware keyboards, TV remotes and gamepad buttons ----

    override fun dispatchKeyEvent(e: KeyEvent): Boolean {
        val src = e.source
        if (src and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD || src and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK) {
            padButton(e); return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_BACK && e.action == KeyEvent.ACTION_UP && e.isLongPress.not() && src and InputDevice.SOURCE_KEYBOARD == 0) {
            if (keyboardShown) { toggleKeyboard(); return true }
            finish(); return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_BACK) return true
        if (e.keyCode == KeyEvent.KEYCODE_F1 && e.isCtrlPressed && e.isAltPressed && e.action == KeyEvent.ACTION_DOWN) {
            overlay.visibility = if (overlay.visibility == View.VISIBLE) View.GONE else View.VISIBLE; return true
        }
        if (e.keyCode == KeyEvent.KEYCODE_F2 && e.isCtrlPressed && e.isAltPressed && e.action == KeyEvent.ACTION_DOWN) {
            settings.trackpad = !settings.trackpad; settings.save(); return true
        }
        if (h == 0L) return true
        val code = Keys.evdev(e.keyCode)
        if (code != null && e.action != KeyEvent.ACTION_MULTIPLE) {
            if (sticky.isNotEmpty() && code !in sticky && code !in MODIFIERS) {
                if (e.action == KeyEvent.ACTION_DOWN) pressKey(code)
                return true
            }
            Native.key(h, code, e.action == KeyEvent.ACTION_DOWN); return true
        }
        // Soft keyboards deliver composed text as ACTION_MULTIPLE characters.
        if (e.action == KeyEvent.ACTION_MULTIPLE && e.characters != null) { typeText(e.characters); return true }
        val ch = e.unicodeChar
        if (ch != 0 && e.action == KeyEvent.ACTION_DOWN) { typeText(String(Character.toChars(ch))); return true }
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
        if (e.source and InputDevice.SOURCE_MOUSE == InputDevice.SOURCE_MOUSE && h != 0L) {
            when (e.actionMasked) {
                MotionEvent.ACTION_HOVER_MOVE, MotionEvent.ACTION_MOVE -> { if (h != 0L) { val (x, y) = view.toRemote(e.x, e.y); Native.abs(h, x, y) } }
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

    companion object {
        const val LONG_PRESS_MS = 500L
        const val TAP_MS = 300L
        /** Movement (dp) before a touch stops being a tap. */
        const val SLOP_PX = 8f
        const val KIND_FIXED = 0
        const val KIND_KDE = 2
        const val KIND_GNOME = 3
        val MODIFIERS = setOf(29, 42, 54, 56, 97, 100, 125, 126)
    }
}
