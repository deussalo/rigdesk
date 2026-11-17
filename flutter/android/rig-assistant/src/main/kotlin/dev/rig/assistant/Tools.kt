package dev.rig.assistant

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
import android.speech.tts.UtteranceProgressListener
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.ForegroundColorSpan
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.WindowManager
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject
import java.util.Locale

/** A tool call the phone will not or cannot do, and why: the call's error. */
open class Refusal(why: String) : Exception(why)

/** One tool's MCP description. */
fun toolSpec(name: String, title: String, readOnly: Boolean, description: String, required: List<String>, vararg props: Pair<String, JSONObject>): JSONObject =
    JSONObject().put("name", name).put("title", title).put("description", description)
        .put("inputSchema", JSONObject().put("type", "object").put("properties", JSONObject().also { p -> props.forEach { p.put(it.first, it.second) } })
            .put("required", JSONArray(required)).put("additionalProperties", false))
        .put("annotations", JSONObject().put("readOnlyHint", readOnly).put("openWorldHint", false))

fun toolProp(type: String, description: String, choices: List<String>? = null): JSONObject =
    JSONObject().put("type", type).put("description", description).also { if (choices != null) it.put("enum", JSONArray(choices)) }

/** The tools themselves: what each takes, and running one call. A refusal says why, as the call's error. */
class Tools(private val ctx: Context) {
    private val messages = Messages(ctx)
    private val files = Files(ctx)

    private val main = Handler(Looper.getMainLooper())
    private val speaker = Speaker(ctx) { speaking -> main.post { this.speaking = speaking; drawBubble() } }
    // Main thread only: whether the phone is speaking a reply now, and the reply's text on the bubble.
    private var speaking = false
    private var bubbleText = ""

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
    } catch (e: Needs) {
        Switch.prompt(ctx, e)
        result("${e.message} The phone now shows a prompt asking the person to allow ${e.what}.", true)
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
        try {
            (Host.input ?: ctx).startActivity(intent)
        } catch (e: ActivityNotFoundException) {
            throw Refusal("no app opens ${uri.ifEmpty { pkg }}")
        }
        return "opened ${uri.ifEmpty { pkg }}" + if (locked()) " behind the lock screen: the phone is locked" else ""
    }

    private fun key(name: String): String {
        val (action, sdk) = KEYS[name] ?: throw Refusal("no key '$name': ${KEYS.keys.joinToString(", ")}")
        if (Build.VERSION.SDK_INT < sdk) throw Refusal("$name needs Android API $sdk; this phone has ${Build.VERSION.SDK_INT}")
        val input = Host.input ?: throw Refusal("key needs RigDesk's Input control, which is off: turn it on in RigDesk > Share screen")
        if (!input.performGlobalAction(action)) throw Refusal("Android refused $name")
        return "pressed $name"
    }

    /** What Rig itself says to the person (no agent resident, say): shown and spoken, a failure only logged. */
    fun tellQuietly(text: String) {
        try { tell(text, true) } catch (e: Exception) { Log.w(TAG, "say: $e") }
    }

    /** Cuts what the phone is saying at once; the text stays on screen for its reading time. Main thread. */
    fun hush() {
        speaker.hush()
        speaking = false
        drawBubble()
    }

    private fun tell(text: String, speak: Boolean): String {
        if (text.isEmpty()) throw Refusal("tell takes text")
        val shown = show(text)
        if (speak) speaker.say(text)
        return (if (shown) "shown" else "posted as a notification (Rig Assistant may not draw over other apps)") + if (speak) " and spoken" else ""
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
                v.setOnClickListener { if (speaking) hush() else removeBubble() }
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
            bubbleText = text
            drawBubble()
            shown = true
            done.countDown()
        }
        if (!done.await(3, java.util.concurrent.TimeUnit.SECONDS)) throw Refusal("the phone's main thread did not show the text within 3 s")
        return shown
    }

    private fun windows() = ctx.getSystemService(Context.WINDOW_SERVICE) as WindowManager

    private fun dp(v: Float) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, ctx.resources.displayMetrics).toInt()

    /** The reply, with a stop line while it is spoken; it hides its reading time after the speech ends or is cut. */
    private fun drawBubble() {
        val view = bubble ?: return
        main.removeCallbacks(hide)
        if (speaking) {
            val shown = SpannableStringBuilder(bubbleText).append("\n\n")
            shown.append("■  Tap to stop speaking", ForegroundColorSpan(0xFFB0BEC5.toInt()), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
            view.text = shown
        } else {
            view.text = bubbleText
            main.postDelayed(hide, (3_000L + 60L * bubbleText.length).coerceAtMost(15_000L))
        }
    }

    private fun removeBubble() {
        bubble?.let { try { windows().removeView(it) } catch (_: Exception) {} }
        bubble = null
    }

    private fun notify(text: String) {
        val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
            nm.createNotificationChannel(NotificationChannel(TOLD_CHANNEL, "What the agent tells you", NotificationManager.IMPORTANCE_HIGH))
        nm.notify(TOLD_ID, notification(ctx, TOLD_CHANNEL, "Rig", text))
    }

    companion object {
        private const val TAG = "RigAssistantTools"
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
class Speaker(private val ctx: Context, private val speaking: (Boolean) -> Unit) {
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
                        bound.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
                            override fun onStart(id: String?) = speaking(true)
                            override fun onDone(id: String?) = speaking(false)
                            override fun onStop(id: String?, interrupted: Boolean) = speaking(false)
                            @Deprecated("Deprecated in Java") override fun onError(id: String?) = speaking(false)
                        })
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

    /** Stops speaking now, and drops a reply still waiting for the engine. Main thread, so it lands before the mic opens. */
    fun hush() {
        waiting = null
        voice?.stop()
    }

    fun close() = main.post {
        voice?.shutdown()
        voice = null
        ready = false
    }
}
