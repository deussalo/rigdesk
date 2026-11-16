package com.carriez.flutter_hbb

import android.accessibilityservice.AccessibilityService
import android.app.KeyguardManager
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.speech.tts.TextToSpeech
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.WindowManager
import android.widget.TextView
import androidx.core.app.NotificationCompat
import ffi.FFI
import org.json.JSONArray
import org.json.JSONObject
import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.security.MessageDigest
import java.util.Locale
import java.util.concurrent.Executors

/**
 * The phone's native tools (apps, open, key, tell) as an MCP server over streamable HTTP at http://<tailnet address>:[PORT]/mcp,
 * for the rigdesk-bridge that shares this phone with Rig: the bridge offers it to the agent holding the phone, beside the desk tools.
 * It answers only on the tailnet and loopback, and only to the bearer set in Share screen > Rig bridge, the same token the
 * phone presents to that bridge; with no Rig bridge set it refuses every call.
 */
class PhoneMcp(private val ctx: Context) {
    @Volatile private var closed = false
    private var socket: ServerSocket? = null
    private val calls = Executors.newFixedThreadPool(4)
    private val tools = PhoneTools(ctx)

    fun start() {
        Thread(::serve, "rigdesk-phone-mcp").apply { isDaemon = true; start() }
    }

    fun close() {
        closed = true
        try { socket?.close() } catch (_: Exception) {}
        calls.shutdownNow()
        tools.close()
    }

    private fun serve() {
        var waitMs = 1_000L
        while (!closed) {
            try {
                ServerSocket(PORT).use { server ->
                    socket = server
                    waitMs = 1_000L
                    while (!closed) {
                        val s = server.accept()
                        if (!reachable(s.localAddress) || !reachable(s.inetAddress)) { s.close(); continue }
                        calls.execute { answer(s) }
                    }
                }
            } catch (e: Exception) {
                if (closed) return
                Log.w(TAG, "port $PORT: $e")
            }
            try { Thread.sleep(waitMs) } catch (_: InterruptedException) {}
            waitMs = minOf(waitMs * 2, 30_000L)
        }
    }

    /**
     * Tailscale's addresses (100.64.0.0/10 and fd7a:115c:a1e0::/48) and loopback, checked at both ends: a carrier may give
     * the phone a 100.64/10 address of its own, and a LAN peer can address the tailnet IP over Wi-Fi, but Tailscale routes
     * replies to a tailnet sender through its tunnel.
     */
    private fun reachable(at: InetAddress): Boolean {
        if (at.isLoopbackAddress) return true
        val b = at.address
        return when (at) {
            is Inet4Address -> b[0] == 100.toByte() && (b[1].toInt() and 0xC0) == 64
            is Inet6Address -> b[0] == 0xfd.toByte() && b[1] == 0x7a.toByte() && b[2] == 0x11.toByte() && b[3] == 0x5c.toByte() &&
                b[4] == 0xa1.toByte() && b[5] == 0xe0.toByte()
            else -> false
        }
    }

    private class Request(val method: String, val path: String, val headers: Map<String, String>, val body: ByteArray)

    private fun answer(s: Socket) {
        s.use {
            try {
                s.soTimeout = 15_000
                val input = BufferedInputStream(s.getInputStream())
                val out = s.getOutputStream()
                val req = read(input) ?: return
                val (status, body) = respond(req)
                val head = StringBuilder("HTTP/1.1 $status\r\n")
                if (status.startsWith("401")) head.append("WWW-Authenticate: Bearer\r\n")
                if (body != null) head.append("Content-Type: application/json\r\nContent-Length: ${body.size}\r\n")
                else head.append("Content-Length: 0\r\n")
                head.append("Connection: close\r\n\r\n")
                out.write(head.toString().toByteArray())
                if (body != null) out.write(body)
                out.flush()
            } catch (e: Exception) {
                Log.d(TAG, "connection: $e")
            }
        }
    }

    private fun read(input: InputStream): Request? {
        val head = ByteArrayOutputStream()
        var tail = 0
        while (tail != 0x0d0a0d0a) {
            val c = input.read()
            if (c < 0) return null
            if (head.size() >= MAX_HEAD) throw IllegalStateException("the request head is over $MAX_HEAD bytes")
            head.write(c)
            tail = (tail shl 8) or c
        }
        val lines = head.toString(Charsets.ISO_8859_1.name()).split("\r\n")
        val first = lines[0].split(" ")
        if (first.size < 2) return null
        val headers = HashMap<String, String>()
        for (line in lines.drop(1)) {
            val colon = line.indexOf(':')
            if (colon > 0) headers[line.substring(0, colon).trim().lowercase(Locale.ROOT)] = line.substring(colon + 1).trim()
        }
        val length = headers["content-length"]?.toIntOrNull() ?: 0
        if (length < 0 || length > MAX_BODY) throw IllegalStateException("a body of $length bytes is refused")
        val body = ByteArray(length)
        var got = 0
        while (got < length) {
            val n = input.read(body, got, length - got)
            if (n < 0) throw IllegalStateException("the body ended at $got of $length bytes")
            got += n
        }
        return Request(first[0], first[1].substringBefore('?'), headers, body)
    }

    private fun bearer(): String? = try {
        JSONObject(FFI.getLocalOption(AgentStop.HOST_OPTION)).optString("token").trim().ifEmpty { null }
    } catch (e: Exception) {
        null
    }

    private fun json(v: JSONObject) = v.toString().toByteArray()

    private fun respond(req: Request): Pair<String, ByteArray?> {
        if (req.path != "/mcp") return "404 Not Found" to json(JSONObject().put("error", "the phone's tools are at /mcp"))
        val token = bearer() ?: return "503 Service Unavailable" to
            json(JSONObject().put("error", "no Rig bridge is set on the phone (RigDesk > Share screen > Rig bridge), so no token is"))
        val said = req.headers["authorization"]?.removePrefix("Bearer ")?.trim().orEmpty()
        if (!MessageDigest.isEqual(said.toByteArray(), token.toByteArray())) return "401 Unauthorized" to json(JSONObject().put("error", "wrong or no bearer"))
        return when (req.method) {
            "POST" -> rpc(req.body)
            "DELETE" -> "200 OK" to null // no sessions are kept
            else -> "405 Method Not Allowed" to json(JSONObject().put("error", "POST JSON-RPC to /mcp"))
        }
    }

    private fun rpc(body: ByteArray): Pair<String, ByteArray?> {
        val msg = try {
            JSONObject(String(body, Charsets.UTF_8))
        } catch (e: Exception) {
            return "400 Bad Request" to json(fault(JSONObject.NULL, -32700, "send one JSON-RPC message as a JSON object"))
        }
        if (!msg.has("id")) return "202 Accepted" to null // a notification
        val id = msg.get("id")
        val params = msg.optJSONObject("params") ?: JSONObject()
        val result = when (val method = msg.optString("method")) {
            "initialize" -> JSONObject()
                .put("protocolVersion", params.optString("protocolVersion").takeIf { it in PROTOCOL_VERSIONS } ?: PROTOCOL_VERSIONS[0])
                .put("capabilities", JSONObject().put("tools", JSONObject()))
                .put("serverInfo", JSONObject().put("name", "phone").put("version", tools.version))
                .put("instructions", "The phone's native Android tools, beside desk/* which sees and drives its screen.")
            "ping" -> JSONObject()
            "tools/list" -> JSONObject().put("tools", tools.specs())
            "tools/call" -> tools.call(params.optString("name"), params.optJSONObject("arguments") ?: JSONObject())
            else -> return "200 OK" to json(fault(id, -32601, "no method $method"))
        }
        return "200 OK" to json(JSONObject().put("jsonrpc", "2.0").put("id", id).put("result", result))
    }

    private fun fault(id: Any, code: Int, message: String) =
        JSONObject().put("jsonrpc", "2.0").put("id", id).put("error", JSONObject().put("code", code).put("message", message))

    companion object {
        private const val TAG = "PhoneMcp"
        const val PORT = 8790
        private val PROTOCOL_VERSIONS = listOf("2025-06-18", "2025-03-26", "2024-11-05")
        private const val MAX_HEAD = 16 * 1024
        private const val MAX_BODY = 1024 * 1024
    }
}

/** A tool call the phone will not or cannot do, and why: the call's error. */
class Refusal(why: String) : Exception(why)

/** One tool's MCP description. */
fun toolSpec(name: String, title: String, readOnly: Boolean, description: String, required: List<String>, vararg props: Pair<String, JSONObject>): JSONObject =
    JSONObject().put("name", name).put("title", title).put("description", description)
        .put("inputSchema", JSONObject().put("type", "object").put("properties", JSONObject().also { p -> props.forEach { p.put(it.first, it.second) } })
            .put("required", JSONArray(required)).put("additionalProperties", false))
        .put("annotations", JSONObject().put("readOnlyHint", readOnly).put("openWorldHint", false))

fun toolProp(type: String, description: String, choices: List<String>? = null): JSONObject =
    JSONObject().put("type", type).put("description", description).also { if (choices != null) it.put("enum", JSONArray(choices)) }

/** The tools themselves: what each takes, and running one call. A refusal says why, as the call's error. */
class PhoneTools(private val ctx: Context) {
    private val messages = PhoneMessages(ctx)
    private val files = PhoneFiles(ctx)

    private val main = Handler(Looper.getMainLooper())
    private val speaker = Speaker(ctx)

    val version: String = try {
        ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName ?: "?"
    } catch (e: Exception) {
        "?"
    }

    fun close() = speaker.close()

    fun specs(): JSONArray = JSONArray()
        .put(toolSpec("apps", "Apps", true, "Every app with a launcher icon on the phone: its label and package, one per line.", emptyList()))
        .put(toolSpec("open", "Open", false, "Open an app by its package (apps lists them), or a URI (https://…, tel:…, sms:…, mailto:…, geo:…, " +
            "whatsapp://send?phone=…&text=…) in whatever app takes it, or in the package named.", emptyList(),
            "package" to toolProp("string", "An app's package, such as com.android.chrome."),
            "uri" to toolProp("string", "A URI to view.")))
        .put(toolSpec("key", "Key", false, "Press a system key the screen has no button for.", listOf("name"),
            "name" to toolProp("string", "Which key.", KEYS.keys.toList())))
        .put(toolSpec("tell", "Tell", false, "Tell the person at the phone something: shown at the top of the screen for a few seconds and, " +
            "unless speak is false, spoken aloud.", listOf("text"),
            "text" to toolProp("string", "What to tell them, short."),
            "speak" to toolProp("boolean", "Speak it as well (default true).")))
        .also { all -> (messages.specs() + files.specs()).forEach { all.put(it) } }

    fun call(name: String, args: JSONObject): JSONObject = try {
        val text = when (name) {
            "apps" -> apps()
            "open" -> open(args.optString("package").trim(), args.optString("uri").trim())
            "key" -> key(args.optString("name"))
            "tell" -> tell(args.optString("text").trim(), args.optBoolean("speak", true))
            else -> messages.call(name, args) ?: files.call(name, args) ?: throw Refusal("the phone has no tool $name")
        }
        result(text, false)
    } catch (e: Refusal) {
        result(e.message ?: "refused", true)
    } catch (e: Exception) {
        Log.w(TAG, "$name: $e")
        result("the phone failed at $name: $e", true)
    }

    private fun result(text: String, error: Boolean) =
        JSONObject().put("content", JSONArray().put(JSONObject().put("type", "text").put("text", text))).put("isError", error)

    private fun locked() = (ctx.getSystemService(Context.KEYGUARD_SERVICE) as KeyguardManager).isKeyguardLocked

    private fun apps(): String {
        val pm = ctx.packageManager
        val found = pm.queryIntentActivities(Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER), 0)
        return found.map { "${it.loadLabel(pm)} — ${it.activityInfo.packageName}" }.distinct().sortedWith(String.CASE_INSENSITIVE_ORDER).joinToString("\n")
    }

    private fun open(pkg: String, uri: String): String {
        val intent = when {
            uri.isNotEmpty() -> Intent(Intent.ACTION_VIEW, Uri.parse(uri)).also { if (pkg.isNotEmpty()) it.setPackage(pkg) }
            pkg.isNotEmpty() -> ctx.packageManager.getLaunchIntentForPackage(pkg) ?: throw Refusal("no app $pkg with a launcher icon: apps lists them")
            else -> throw Refusal("open takes a package or a uri")
        }
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && !Settings.canDrawOverlays(ctx) && InputService.ctx == null)
            throw Refusal("Android lets RigDesk start an app from the background only with Input control or the overlay permission on")
        try {
            (InputService.ctx ?: ctx).startActivity(intent)
        } catch (e: ActivityNotFoundException) {
            throw Refusal("no app opens ${uri.ifEmpty { pkg }}")
        }
        return "opened ${uri.ifEmpty { pkg }}" + if (locked()) " behind the lock screen: the phone is locked" else ""
    }

    private fun key(name: String): String {
        val (action, sdk) = KEYS[name] ?: throw Refusal("no key '$name': ${KEYS.keys.joinToString(", ")}")
        if (Build.VERSION.SDK_INT < sdk) throw Refusal("$name needs Android API $sdk; this phone has ${Build.VERSION.SDK_INT}")
        val input = InputService.ctx ?: throw Refusal("RigDesk's Input control is off: turn it on in RigDesk > Share screen")
        if (!input.performGlobalAction(action)) throw Refusal("Android refused $name")
        return "pressed $name"
    }

    private fun tell(text: String, speak: Boolean): String {
        if (text.isEmpty()) throw Refusal("tell takes text")
        val shown = show(text)
        if (speak) speaker.say(text)
        return (if (shown) "shown" else "posted as a notification (RigDesk may not draw over other apps)") + if (speak) " and spoken" else ""
    }

    private var bubble: TextView? = null
    private val hide = Runnable { removeBubble() }

    /** Shown at the top of the screen over other apps, else as a notification: whether it was shown. */
    private fun show(text: String): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && !Settings.canDrawOverlays(ctx)) {
            notify(text)
            return false
        }
        val done = java.util.concurrent.CountDownLatch(1)
        var shown = false // set on the main thread before done counts down
        main.post {
            main.removeCallbacks(hide)
            val view = bubble ?: TextView(ctx).also { v ->
                v.setTextColor(0xFFFFFFFF.toInt())
                v.setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
                val pad = dp(14f)
                v.setPadding(pad, pad, pad, pad)
                v.background = GradientDrawable().apply { cornerRadius = dp(14f).toFloat(); setColor(0xEE263238.toInt()) }
                v.setOnClickListener { removeBubble() }
                val lp = WindowManager.LayoutParams(
                    WindowManager.LayoutParams.MATCH_PARENT, WindowManager.LayoutParams.WRAP_CONTENT,
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
                    else WindowManager.LayoutParams.TYPE_PHONE,
                    WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                    PixelFormat.TRANSLUCENT
                ).apply { gravity = Gravity.TOP; y = dp(48f); horizontalMargin = 0.04f }
                try {
                    windows().addView(v, lp)
                    bubble = v
                } catch (e: Exception) {
                    Log.w(TAG, "bubble: $e")
                    notify(text)
                    done.countDown()
                    return@post
                }
            }
            view.text = text
            main.postDelayed(hide, (3_000L + 60L * text.length).coerceAtMost(15_000L))
            shown = true
            done.countDown()
        }
        if (!done.await(3, java.util.concurrent.TimeUnit.SECONDS)) throw Refusal("the phone's main thread did not show the text within 3 s")
        return shown
    }

    private fun windows() = ctx.getSystemService(Context.WINDOW_SERVICE) as WindowManager

    private fun dp(v: Float) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, ctx.resources.displayMetrics).toInt()

    private fun removeBubble() {
        bubble?.let { try { windows().removeView(it) } catch (_: Exception) {} }
        bubble = null
    }

    private fun notify(text: String) {
        val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
            nm.createNotificationChannel(NotificationChannel(TOLD_CHANNEL, "What the agent tells you", NotificationManager.IMPORTANCE_HIGH))
        nm.notify(TOLD_ID, NotificationCompat.Builder(ctx, TOLD_CHANNEL).setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("Rig").setContentText(text).setStyle(NotificationCompat.BigTextStyle().bigText(text)).setAutoCancel(true).build())
    }

    companion object {
        private const val TAG = "PhoneTools"
        private const val TOLD_CHANNEL = "rig-told"
        private const val TOLD_ID = 7301
        /** Each key's global action and the API level that brought it. */
        private val KEYS = linkedMapOf(
            "back" to (AccessibilityService.GLOBAL_ACTION_BACK to 16),
            "home" to (AccessibilityService.GLOBAL_ACTION_HOME to 16),
            "recents" to (AccessibilityService.GLOBAL_ACTION_RECENTS to 16),
            "notifications" to (AccessibilityService.GLOBAL_ACTION_NOTIFICATIONS to 16),
            "quick_settings" to (AccessibilityService.GLOBAL_ACTION_QUICK_SETTINGS to 17),
            "power_dialog" to (AccessibilityService.GLOBAL_ACTION_POWER_DIALOG to 21),
            "lock_screen" to (AccessibilityService.GLOBAL_ACTION_LOCK_SCREEN to 28),
            "screenshot" to (AccessibilityService.GLOBAL_ACTION_TAKE_SCREENSHOT to 28),
        )
    }
}

/** The phone's text-to-speech, bound on first use; a failed engine is let go so the next call binds again. */
private class Speaker(private val ctx: Context) {
    private val main = Handler(Looper.getMainLooper())
    private var voice: TextToSpeech? = null // main thread only
    private var ready = false
    private var waiting: String? = null

    fun say(text: String) = main.post {
        val v = voice
        if (v == null) {
            waiting = text
            voice = TextToSpeech(ctx.applicationContext) { status ->
                main.post {
                    ready = status == TextToSpeech.SUCCESS
                    val bound = voice
                    if (ready && bound != null) {
                        bound.setLanguage(Locale.getDefault())
                        waiting?.let { bound.speak(it, TextToSpeech.QUEUE_FLUSH, null, "rig") }
                    } else {
                        bound?.shutdown()
                        voice = null
                    }
                    waiting = null
                }
            }
        } else if (ready) v.speak(text, TextToSpeech.QUEUE_FLUSH, null, "rig")
        else waiting = text
    }

    fun close() = main.post {
        voice?.shutdown()
        voice = null
        ready = false
    }
}
