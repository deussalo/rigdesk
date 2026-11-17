package dev.rig.assistant

import android.content.Context
import android.content.Intent
import android.graphics.PixelFormat
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.WindowManager
import android.widget.TextView

/** What RigDesk's desk/<tool> holder is doing, as its bridge last said: who holds the phone and when it last acted (phone clock). */
data class Desk(val holder: String, val actedAtMs: Long, val press: (stop: Boolean) -> Unit)

/**
 * The agent's one Stop/Resume pill on the phone, for phone/<tool> calls and RigDesk's desk/<tool> alike. Stop shows only while an agent is
 * active, within [ACTIVE_MS] of its last call (reads count), so it never covers an idle screen or a screenshot; green Resume shows
 * while stopped. Stop refuses every phone/<tool> call until Resume, and blocks the desk/<tool> holder through RigDesk's bridge.
 */
object Stop {
    const val ACTIVE_MS = 20_000L
    private const val TAG = "RigAssistantStop"
    private val main = Handler(Looper.getMainLooper())
    private val refreshLater = Runnable { refresh() }
    private lateinit var ctx: Context

    /** Read on rig-calls; written on main. */
    @Volatile private var phoneStopped = false
    // Main thread only.
    private var phoneActedAtMs = 0L
    private var desk: Desk? = null
    private var pill: TextView? = null
    private var asked = false

    fun attach(context: Context) {
        if (::ctx.isInitialized) return
        ctx = context.applicationContext
        phoneStopped = Config.prefs(ctx).getBoolean(STOPPED, false)
        main.post { refresh() }
    }

    /** Called for each phone/<tool> call before it runs: why it may not run, or null. */
    fun called(): String? {
        main.post { phoneActedAtMs = System.currentTimeMillis(); refresh() }
        return if (phoneStopped) "the person pressed Stop on the phone: every phone/* call is refused until they press Resume" else null
    }

    /** RigDesk's bridge state changed, or null once RigDesk stops following it. Main thread. */
    fun desk(now: Desk?) {
        desk = now
        refresh()
    }

    private fun stopped() = phoneStopped || desk?.holder == "blocked"

    private fun refresh() {
        main.removeCallbacks(refreshLater)
        val now = System.currentTimeMillis()
        val d = desk
        val idleMs = minOf(now - phoneActedAtMs, if (d?.holder == "agent") now - d.actedAtMs else Long.MAX_VALUE)
        val label = when {
            stopped() -> "Resume"
            idleMs < ACTIVE_MS -> "Stop"
            else -> null
        }
        if (label == null) removePill() else showPill(label)
        if (label == "Stop") main.postDelayed(refreshLater, ACTIVE_MS - idleMs + 100)
    }

    private fun pressed(stop: Boolean) {
        if (phoneStopped != stop) {
            phoneStopped = stop
            Config.prefs(ctx).edit().putBoolean(STOPPED, stop).apply()
            Link.event(if (stop) "stop" else "resume", "", null)
        }
        val d = desk
        if (d != null && d.holder == (if (stop) "agent" else "blocked")) d.press(stop)
        if (!stop) phoneActedAtMs = System.currentTimeMillis()
        refresh()
    }

    private fun showPill(label: String) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M && !Settings.canDrawOverlays(ctx)) {
            if (!asked) {
                asked = true
                Switch.prompt(ctx, Needs("Without it the phone cannot show Stop while an agent acts.", "drawing over apps",
                    Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:${ctx.packageName}"))))
            }
            return
        }
        val view = pill ?: TextView(ctx).also { v ->
            v.setTextColor(0xFFFFFFFF.toInt())
            v.setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            v.setPadding(dp(16f), dp(8f), dp(16f), dp(8f))
            v.setOnClickListener { pressed(v.text == "Stop") }
            val lp = WindowManager.LayoutParams(
                WindowManager.LayoutParams.WRAP_CONTENT, WindowManager.LayoutParams.WRAP_CONTENT,
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY
                else @Suppress("DEPRECATION") WindowManager.LayoutParams.TYPE_PHONE,
                WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN,
                PixelFormat.TRANSLUCENT
            ).apply { gravity = Gravity.TOP or Gravity.END; y = ctx.resources.displayMetrics.heightPixels / 3 }
            try { windows().addView(v, lp); pill = v } catch (e: Exception) { Log.w(TAG, "pill: $e"); return }
        }
        view.text = label
        view.background = GradientDrawable().apply {
            cornerRadius = dp(20f).toFloat()
            setColor(if (label == "Stop") 0xFFC62828.toInt() else 0xFF2E7D32.toInt())
        }
    }

    /** Puts the pill back on top: an overlay added later, such as a reply's bubble, would cover it and take its taps. Main thread. */
    fun raise() {
        val view = pill ?: return
        val lp = view.layoutParams as? WindowManager.LayoutParams ?: return
        try {
            windows().removeView(view)
            windows().addView(view, lp)
        } catch (e: Exception) {
            Log.w(TAG, "raise: $e")
            pill = null
        }
    }

    private fun removePill() {
        pill?.let { try { windows().removeView(it) } catch (_: Exception) {} }
        pill = null
    }

    private fun windows() = ctx.getSystemService(Context.WINDOW_SERVICE) as WindowManager

    private fun dp(v: Float) = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, ctx.resources.displayMetrics).toInt()

    private const val STOPPED = "stopped"
}
