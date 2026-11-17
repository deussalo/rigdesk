package dev.rig.assistant

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.util.ArrayDeque
import java.util.UUID
import java.util.concurrent.ExecutionException
import java.util.concurrent.Executors

/**
 * The phone's one connection to Rig's rig-assistant plugin: it dials, says hello with the phone's tools, answers calls, and sends
 * events (an assistant press) until Rig acks each one, replaying them after a reconnect. Rig keys each event by boot and seq, so a
 * replay is never told twice.
 *
 * Threads: rig-link dials and reads; rig-calls runs tools and writes their results; rig-send writes events, replays them and
 * closes sockets. The main thread only queues work, because a network write there throws and a write to a stalled socket blocks.
 */
object Link {
    private const val TAG = "RigAssistant"
    private val BACKOFF_MS = longArrayOf(1_000, 2_000, 5_000, 10_000, 30_000)
    private const val KEPT = 20
    private val boot = UUID.randomUUID().toString().substring(0, 8)
    private val unacked = ArrayDeque<JSONObject>()
    private val calls = Executors.newSingleThreadExecutor { Thread(it, "rig-calls") }
    private val out = Executors.newSingleThreadExecutor { Thread(it, "rig-send") }
    private val wake = Object()
    private var nudged = false
    private var seq = 0L
    private var loop: Thread? = null
    private var network: Network? = null
    private lateinit var ctx: Context
    private lateinit var tools: Tools
    /** Set on rig-send once hello and the replay are written, so no event overtakes them. */
    @Volatile private var ws: Ws? = null
    @Volatile var state = "starting"
        private set
    /** The resident agent's title as Rig last said, or empty with none. */
    @Volatile var resident = ""
        private set

    @Synchronized
    fun start(context: Context) {
        if (loop?.isAlive == true) return
        ctx = context.applicationContext
        tools = Tools(ctx)
        Stop.attach(ctx)
        watchNetwork()
        loop = Thread(::run, "rig-link").apply { isDaemon = true; start() }
    }

    fun connected() = ws != null

    /** Drop the connection so the loop dials again with the settings as they are now. */
    fun redial() {
        out.execute { ws?.close() }
        nudge()
    }

    /** kind: assist, stop, resume or notice. Kept until Rig acks it and sent again after a reconnect. Safe on the main thread. */
    fun event(kind: String, text: String, image: JSONObject?) {
        val event = JSONObject().put("type", "event").put("kind", kind).put("text", text).put("at_ms", System.currentTimeMillis())
        if (image != null) event.put("image", image)
        synchronized(this) {
            event.put("seq", ++seq)
            unacked.add(event)
            while (unacked.size > KEPT) unacked.removeFirst()
        }
        out.execute { send(event) }
    }

    private fun watchNetwork() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.N) return
        val connectivity = ctx.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        connectivity.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(now: Network) {
                val before = network
                network = now
                if (before != null && before != now) redial() else nudge()
            }
        })
    }

    private fun nudge() = synchronized(wake) {
        nudged = true
        wake.notifyAll()
    }

    private fun pause(ms: Long) = synchronized(wake) {
        val until = System.currentTimeMillis() + ms
        var left = ms
        while (!nudged && left > 0) {
            try { wake.wait(left) } catch (e: InterruptedException) { Thread.currentThread().interrupt(); return@synchronized }
            left = until - System.currentTimeMillis()
        }
        nudged = false
    }

    private fun run() {
        var failures = 0
        while (true) {
            val (config, why) = Config.load(ctx)
            if (config == null) {
                state = why
                pause(5_000)
                continue
            }
            var fresh: Ws? = null
            try {
                state = "connecting to ${config.origin}"
                fresh = Ws.connect(config.socket(), mapOf("Authorization" to "Bearer ${config.token}", "Origin" to config.origin))
                fresh.sendText(hello(config.phone).toString())
                val ready = fresh
                out.submit { for (event in unackedNow()) ready.sendText(event.toString()); ws = ready }.get()
                state = "connected to ${config.origin}"
                failures = 0
                while (true) received(JSONObject(fresh.receive()))
            } catch (e: ExecutionException) {
                Log.w(TAG, "link", e.cause)
                state = "not connected: ${e.cause?.message}"
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
                state = "not connected: interrupted"
            } catch (e: Exception) {
                Log.w(TAG, "link", e)
                state = "not connected: ${e.message}"
            } finally {
                ws = null
                fresh?.close()
            }
            pause(BACKOFF_MS[minOf(failures++, BACKOFF_MS.size - 1)])
        }
    }

    @Synchronized
    private fun unackedNow() = ArrayList(unacked)

    private fun hello(phone: String): JSONObject {
        val offered = JSONArray()
        val specs = tools.specs()
        for (i in 0 until specs.length()) {
            val spec = specs.getJSONObject(i)
            offered.put(JSONObject().put("name", spec.getString("name")).put("description", spec.getString("description"))
                .put("input_schema", spec.getJSONObject("inputSchema"))
                .put("read_only", spec.optJSONObject("annotations")?.optBoolean("readOnlyHint") ?: false))
        }
        return JSONObject().put("type", "hello").put("phone", phone).put("boot", boot).put("app", "RigDesk ${tools.version}").put("tools", offered)
    }

    private fun received(frame: JSONObject) {
        when (frame.getString("type")) {
            "call" -> {
                val id = frame.getLong("id")
                val tool = frame.getString("tool")
                val args = frame.optJSONObject("args") ?: JSONObject()
                calls.execute {
                    val refused = Stop.called()
                    send(if (refused != null) JSONObject().put("type", "result").put("id", id).put("ok", false).put("refused", refused)
                        else result(id, tools.call(tool, args)))
                }
            }
            "ack" -> {
                val upto = frame.getLong("seq")
                synchronized(this) { unacked.removeAll { it.optLong("seq") <= upto } }
            }
            "welcome" -> resident = frame.optString("resident", "")
            "say" -> tools.tellQuietly(frame.optString("text"))
            else -> Log.w(TAG, "unknown frame $frame")
        }
    }

    /** The tool's MCP answer as the plugin's result frame. */
    private fun result(id: Long, answer: JSONObject): JSONObject {
        val content = answer.optJSONArray("content")
        val text = if (content != null && content.length() > 0) content.getJSONObject(0).optString("text") else ""
        val failed = answer.optBoolean("isError")
        return JSONObject().put("type", "result").put("id", id).put("ok", !failed).put(if (failed) "refused" else "text", text)
    }

    /** Writes on the calling thread, which is rig-calls or rig-send, never main. */
    private fun send(frame: JSONObject) {
        val current = ws ?: return
        try {
            current.sendText(frame.toString())
        } catch (e: Exception) {
            Log.w(TAG, "send", e)
            current.close()
        }
    }
}
