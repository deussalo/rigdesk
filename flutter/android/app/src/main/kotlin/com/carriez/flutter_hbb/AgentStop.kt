package com.carriez.flutter_hbb

import android.content.Context
import android.graphics.PixelFormat
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowManager
import dev.rig.assistant.Desk
import dev.rig.assistant.Stop
import ffi.FFI
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import kotlin.math.min

/**
 * The agent's Stop/Resume on the phone itself, and "your input goes first", for a phone that Rig drives as a stock PC
 * through a rigdesk-bridge. The phone tells that bridge's agent API (Share screen > Rig bridge, kept as the local
 * option [HOST_OPTION]) when its person presses Stop or Resume or touches the screen. The pill shows only while the
 * agent is active: blocked, or within [Stop.ACTIVE_MS] of its last tool call. The pill itself is Rig Assistant's [Stop], shared with
 * phone/<tool> calls, so the phone shows one Stop for both.
 */
class AgentStop(private val ctx: Context) {
    private val main = Handler(Looper.getMainLooper())
    private val windows = ctx.getSystemService(Context.WINDOW_SERVICE) as WindowManager

    @Volatile private var closed = false
    private var worker: Thread? = null

    // Main thread only.
    private var holder = ""
    private var lastActionAtMs = 0L
    private var watcher: View? = null
    private var lastHumanSentMs = 0L

    private data class Host(val url: String, val token: String)

    fun start() {
        Stop.attach(ctx)
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
        Stop.desk(if (closed || holder.isEmpty()) null else Desk(holder, lastActionAtMs, ::pressed))
        if (!closed && holder == "agent") addWatcher() else removeWatcher()
    }

    private fun overlayAllowed() = Build.VERSION.SDK_INT < Build.VERSION_CODES.M || Settings.canDrawOverlays(ctx)

    private fun overlayType() =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
        else WindowManager.LayoutParams.TYPE_PHONE

    /** The person pressed the shared pill: block or unblock the desk/<tool> holder at the bridge. */
    private fun pressed(stop: Boolean) {
        holder = if (stop) "blocked" else "agent"
        if (!stop) lastActionAtMs = System.currentTimeMillis()
        refresh()
        send(if (stop) "/viewer/stop" else "/viewer/unblock")
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
        // A gesture the phone injects ends within this of its last event (long press and wheel swipes included).
        private const val OWN_INPUT_MS = 1_000L
        private const val HUMAN_EVERY_MS = 500L
    }
}
