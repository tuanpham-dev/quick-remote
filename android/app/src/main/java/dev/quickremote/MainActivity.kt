package dev.quickremote

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Bundle
import android.text.InputType
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import org.json.JSONArray
import kotlin.concurrent.thread

/** Host list: hosts on the network, and connect by address or host ID. */
class MainActivity : Activity() {
    private lateinit var list: LinearLayout
    private lateinit var status: TextView

    override fun onCreate(b: Bundle?) {
        super.onCreate(b)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(48, 48, 48, 48) }
        root.addView(TextView(this).apply { text = "QuickRemote"; textSize = 26f })
        status = TextView(this).apply { text = "Looking for hosts…" }
        root.addView(status)
        list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(list)
        val addr = EditText(this).apply { hint = "Address or host ID"; isSingleLine = true }
        root.addView(addr)
        root.addView(Button(this).apply { text = "Connect"; setOnClickListener { connect(addr.text.toString().trim(), null) } })
        root.addView(Button(this).apply { text = "Refresh"; setOnClickListener { discover() } })
        setContentView(ScrollView(this).apply { addView(root) })
        discover()
    }

    private fun discover() {
        status.text = "Looking for hosts…"
        list.removeAllViews()
        thread {
            // mDNS needs a multicast lock on Android.
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            val lock = wifi.createMulticastLock("quickremote").apply { setReferenceCounted(false); acquire() }
            val json = try { Native.listHosts(2500) } finally { lock.release() }
            val hosts = JSONArray(json)
            runOnUiThread {
                status.text = if (hosts.length() == 0) "No hosts on this network." else "On this network"
                for (i in 0 until hosts.length()) {
                    val hst = hosts.getJSONObject(i)
                    list.addView(Button(this).apply {
                        text = "${hst.getString("name")} · ${hst.getString("os")}"
                        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
                        setOnClickListener { connect(hst.getString("addr"), null) }
                    })
                }
            }
        }
    }

    private fun connect(target: String, pin: String?) {
        if (target.isEmpty()) return
        status.text = "Connecting to $target…"
        val dm = resources.displayMetrics
        thread {
            try {
                val h = Native.connect(filesDir.absolutePath, target, pin, android.os.Build.MODEL, null,
                    hardwareCodecs(), maxOf(dm.widthPixels, dm.heightPixels), minOf(dm.widthPixels, dm.heightPixels))
                runOnUiThread { startActivity(Intent(this, SessionActivity::class.java).putExtra("handle", h)) ; status.text = "" }
            } catch (e: IllegalStateException) {
                val msg = e.message ?: "Couldn't connect."
                runOnUiThread {
                    if (msg.contains("PIN")) askPin(target, msg) else status.text = msg
                }
            }
        }
    }

    private fun askPin(target: String, msg: String) {
        val input = EditText(this).apply { inputType = InputType.TYPE_CLASS_NUMBER; hint = "6-digit PIN" }
        AlertDialog.Builder(this)
            .setTitle("Pair with this host")
            .setMessage(msg)
            .setView(input)
            .setPositiveButton("Pair") { _, _ -> connect(target, input.text.toString().trim()) }
            .setNegativeButton("Cancel", null)
            .show()
    }
}
