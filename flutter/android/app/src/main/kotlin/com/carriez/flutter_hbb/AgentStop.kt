package com.carriez.flutter_hbb

import android.content.Context
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import ffi.FFI
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import kotlin.math.min

/**
 * The agent's Stop/Resume on the phone itself, and "your input goes first", for a phone that Rig drives as a stock PC
 * through a rigdesk-bridge. The phone tells that bridge's agent API (Share screen > Rig bridge, kept as the local
 * option [HOST_OPTION]) when its person presses Stop or Resume or touches the screen. The pill shows only while the
 * agent is active: blocked, or within [ACTIVE_MS] of its last tool call.
 */
class AgentStop(private val ctx: Context) {
    private val main = Handler(Looper.getMainLooper())
    private val windows = ctx.getSystemService(Context.WINDOW_SERVICE) as WindowManager

    @Volatile private var closed = false
    private var worker: Thread? = null

    // Main thread only.
    private var holder = ""
    private var lastActionAtMs = 0L
    private var pill: TextView? = null
    private var watcher: View? = null
    private var lastHumanSentMs = 0L
    private val refreshLater = Runnable { refresh() }

    private data class Host(val url: String, val token: String)

    fun start() {
        worker = Thread(::follow, "rigdesk-agent-stop").apply { isDaemon = true; start() }
    }

    fun close() {
        closed = true
        worker?.interrupt()
        main.post { holder = ""; refresh() }
    }

    private fun host(): Host? = try {
        val j = JSONObject(FFI.getLocalOption(HOST_OPTION))
        val url = j.optString("url").trim().trimEnd('/')
        val token = j.optString("token").trim()
        if ((url.startsWith("https://") || url.startsWith("http://")) && token.isNotEmpty()) Host(url, token) else null
    } catch (e: Exception) {
        null
    }

    private fun open(host: Host, path: String, method: String = "GET"): HttpURLConnection =
        (URL(host.url + path).openConnection() as HttpURLConnection).apply {
            requestMethod = method
            setRequestProperty("Authorization", "Bearer ${host.token}")
            connectTimeout = 15_000
            readTimeout = 15_000
        }

    /** The bridge's state, then its events, for as long as the service runs; reconnects with backoff. */
    private fun follow() {
        var lastId = 0L
        var boot = ""
        var waitMs = 1_000L
        while (!closed) {
            val host = host()
            if (host == null) {
                main.post { holder = ""; refresh() }
                pause(30_000)
                continue
            }
            try {
                val health = open(host, "/health").let { c ->
                    if (c.responseCode != 200) throw IllegalStateException("/health answered HTTP ${c.responseCode}")
                    JSONObject(c.inputStream.bufferedReader().use { it.readText() })
                }
                val nowBoot = health.optString("boot")
                if (nowBoot != boot) { boot = nowBoot; lastId = 0 }
                val standing = health.optJSONObject("standing")
                val held = if (standing == null || standing.isNull("holder")) "" else standing.optString("holder")
                main.post { holder = held; refresh() }
                val events = open(host, "/events").apply {
                    setRequestProperty("Accept", "text/event-stream")
                    setRequestProperty("Last-Event-ID", lastId.toString())
                    readTimeout = 60_000 // the bridge beats every 15 s
                }
                if (events.responseCode != 200) throw IllegalStateException("/events answered HTTP ${events.responseCode}")
                waitMs = 1_000L
                events.inputStream.bufferedReader().use { reader ->
                    val data = StringBuilder()
                    while (!closed) {
                        val line = reader.readLine() ?: break
                        if (line.startsWith("data:")) {
                            data.append(line.substring(5).trim())
                        } else if (line.isEmpty() && data.isNotEmpty()) {
                            val id = onEvent(JSONObject(data.toString()))
                            if (id > 0) lastId = id
                            data.setLength(0)
                        }
                    }
                }
            } catch (e: Exception) {
                if (!closed) Log.d(TAG, "bridge: $e")
            }
            pause(waitMs)
            waitMs = min(waitMs * 2, 30_000L)
        }
    }

    private fun pause(ms: Long) {
        try { Thread.sleep(ms) } catch (_: InterruptedException) {}
    }

    private fun onEvent(e: JSONObject): Long {
        val atMs = e.optLong("at_ms")
        when (e.optString("type")) {
            "claimed" -> main.post { holder = "agent"; lastActionAtMs = maxOf(lastActionAtMs, atMs); refresh() }
            "holder" -> main.post { holder = e.optString("holder"); refresh() }
            "revoked" -> main.post { holder = ""; refresh() }
            // Live only, never replayed: the phone's own clock, so the window needs no clocks in agreement.
            "action" -> main.post { lastActionAtMs = System.currentTimeMillis(); refresh() }
        }
        return e.optLong("id")
    }

    private fun refresh() {
        main.removeCallbacks(refreshLater)
        val idleMs = System.currentTimeMillis() - lastActionAtMs
        val label = when {
            closed -> null
            holder == "blocked" -> "Resume"
            holder == "agent" && idleMs < ACTIVE_MS -> "Stop"
            else -> null
        }
        if (label == null) removePill() else showPill(label)
        if (!closed && holder == "agent") addWatcher() else removeWatcher()
        if (label == "Stop") main.postDelayed(refreshLater, ACTIVE_MS - idleMs + 100)
    }

    private fun overlayAllowed() = Build.VERSION.SDK_INT < Build.VERSION_CODES.M || Settings.canDrawOverlays(ctx)

    private fun overlayType() =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
        else WindowManager.LayoutParams.TYPE_PHONE

    private fun dp(v: Float) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, ctx.resources.displayMetrics).toInt()

    private fun showPill(label: String) {
        if (!overlayAllowed()) return
        val view = pill ?: TextView(ctx).also { v ->
            v.setTextColor(0xFFFFFFFF.toInt())
            v.setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            v.setPadding(dp(16f), dp(8f), dp(16f), dp(8f))
            v.setOnClickListener { pressed(v.text.toString()) }
            val lp = WindowManager.LayoutParams(
                WindowManager.LayoutParams.WRAP_CONTENT, WindowManager.LayoutParams.WRAP_CONTENT, overlayType(),
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                PixelFormat.TRANSLUCENT
            ).apply { gravity = Gravity.TOP or Gravity.END; y = ctx.resources.displayMetrics.heightPixels / 3 }
            try { windows.addView(v, lp); pill = v } catch (e: Exception) { Log.w(TAG, "pill: $e"); return }
        }
        view.text = label
        view.background = GradientDrawable().apply {
            cornerRadius = dp(20f).toFloat()
            setColor(if (label == "Stop") 0xFFC62828.toInt() else 0xFF2E7D32.toInt())
        }
    }

    private fun removePill() {
        pill?.let { try { windows.removeView(it) } catch (_: Exception) {} }
        pill = null
    }

    private fun pressed(label: String) {
        holder = if (label == "Stop") "blocked" else "agent"
        if (label != "Stop") lastActionAtMs = System.currentTimeMillis()
        refresh()
        send(if (label == "Stop") "/viewer/stop" else "/viewer/unblock")
    }

    /** A touch anywhere: a 1 px window watching outside touches. The phone's own injected input is not the person's. */
    private fun addWatcher() {
        if (watcher != null || !overlayAllowed()) return
        val v = View(ctx)
        v.setOnTouchListener { _, ev ->
            if (ev.actionMasked == MotionEvent.ACTION_OUTSIDE) humanTouched()
            false
        }
        val lp = WindowManager.LayoutParams(
            1, 1, overlayType(),
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL or
                WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH or WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
            PixelFormat.TRANSLUCENT
        ).apply { gravity = Gravity.TOP or Gravity.START; alpha = 0f }
        try { windows.addView(v, lp); watcher = v } catch (e: Exception) { Log.w(TAG, "watcher: $e") }
    }

    private fun removeWatcher() {
        watcher?.let { try { windows.removeView(it) } catch (_: Exception) {} }
        watcher = null
    }

    private fun humanTouched() {
        val now = SystemClock.uptimeMillis()
        if (now - InputService.lastInjectedMs < OWN_INPUT_MS || now - lastHumanSentMs < HUMAN_EVERY_MS) return
        lastHumanSentMs = now
        send("/viewer/human")
    }

    private fun send(path: String) {
        val host = host() ?: return
        Thread {
            try {
                val c = open(host, path, "POST").apply {
                    doOutput = true
                    setRequestProperty("Content-Type", "application/json")
                }
                c.outputStream.use { it.write("{}".toByteArray()) }
                if (c.responseCode != 200) Log.w(TAG, "$path answered HTTP ${c.responseCode}")
                c.disconnect()
            } catch (e: Exception) {
                Log.w(TAG, "$path: $e")
            }
        }.start()
    }

    companion object {
        private const val TAG = "AgentStop"
        const val HOST_OPTION = "rigdesk-agent-host-self"
        private const val ACTIVE_MS = 20_000L
        // A gesture the phone injects ends within this of its last event (long press and wheel swipes included).
        private const val OWN_INPUT_MS = 1_000L
        private const val HUMAN_EVERY_MS = 500L
    }
}
