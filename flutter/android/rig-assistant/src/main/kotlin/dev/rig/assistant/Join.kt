package dev.rig.assistant

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.widget.Toast
import org.json.JSONObject

/**
 * One tap: a Rig agent's request to use this phone, posted as a notification, since this link is up whenever the phone is.
 * RigDesk's one-tap desk (base::one_tap, through [Host.oneTap]) holds every request from Rig's join_request to the end of its
 * session; this only draws what the desk answers each call with: the notifications to post and take down, what to tell Rig,
 * and why a call was not taken. Accept opens the app's one-tap screen (ACTION_ONE_TAP, in the app around this module), which
 * starts sharing and tells the desk the phone is ready; Deny is the person's no. None outlives its lapse.
 */
object Join {
    private const val TAG = "RigAssistantJoin"
    private const val CHANNEL = "rig-join"
    const val ACTION_ONE_TAP = "dev.rig.assistant.ONE_TAP"
    const val EXTRA_ASK = "ask"
    private val main = Handler(Looper.getMainLooper())
    @Volatile private var tick: Runnable? = null

    /** Rig's join_request frame. A resend after a reconnect posts the same notification again. */
    fun requested(ctx: Context, frame: JSONObject) {
        call(ctx, JSONObject(frame.toString()).put("op", "request"))
    }

    /** Rig's join_end frame: Rig is done with the request, however it ended. */
    fun ended(ctx: Context, ask: String) {
        call(ctx, op("end", ask))
    }

    /** The person tapped Accept: the agent's title when their yes now stands for the ask, else null and why not. */
    fun accept(ctx: Context, ask: String): Pair<String?, String> = answered(call(ctx, op("accept", ask)))

    /** The agent's title while the person's yes for the ask stands, else null and why not. */
    fun allowed(ctx: Context, ask: String): Pair<String?, String> = answered(call(ctx, op("allowed", ask)))

    /** The person's no: Deny on the notification, or Cancel on Android's Start now after their Accept. */
    fun deny(ctx: Context, ask: String, why: String = ""): String = call(ctx, op("deny", ask).put("why", why)).optString("refused")

    /** Sharing runs and the phone listens: Rig is told the phone is ready while the person's yes stands. */
    fun ready(ctx: Context, ask: String) {
        call(ctx, op("ready", ask))
    }

    /** Sharing did not start, or the phone does not listen: Rig is told why. */
    fun notReady(ctx: Context, ask: String, why: String) {
        call(ctx, op("not_ready", ask).put("why", why))
    }

    private fun op(op: String, ask: String) = JSONObject().put("op", op).put("ask", ask)

    private fun answered(render: JSONObject): Pair<String?, String> =
        (if (render.isNull("allowed")) null else render.optString("allowed")) to
            (if (render.isNull("refused")) "" else render.optString("refused"))

    /** One call to the desk, and its answer drawn. */
    private fun call(ctx: Context, call: JSONObject): JSONObject {
        val render = try {
            JSONObject(Host.oneTap(call.toString()))
        } catch (e: Throwable) {
            Log.w(TAG, "the one-tap desk did not answer ${call.optString("op")}", e)
            JSONObject().put("refused", "RigDesk's one-tap desk did not answer: ${e.message}")
        }
        draw(ctx, render)
        if (!render.isNull("refused")) Log.i(TAG, "${call.optString("op")} ${call.optString("ask")}: ${render.optString("refused")}")
        return render
    }

    private fun draw(ctx: Context, render: JSONObject) {
        val nm = manager(ctx)
        render.optJSONArray("hide")?.let { hide -> for (i in 0 until hide.length()) nm.cancel(id(hide.getString(i))) }
        render.optJSONArray("say")?.let { say ->
            for (i in 0 until say.length()) say.getJSONObject(i).let { Link.join(it.getString("ask"), it.getString("nonce"), it.getString("said"), it.optString("reason")) }
        }
        val prompts = render.optJSONArray("prompts") ?: return
        var soonest = Long.MAX_VALUE
        for (i in 0 until prompts.length()) {
            val prompt = prompts.getJSONObject(i)
            val ask = prompt.getString("ask")
            val left = prompt.optLong("left_ms")
            soonest = minOf(soonest, left)
            if (!prompt.optBoolean("post")) continue
            val why = post(ctx, ask, prompt.optString("agent_title"), prompt.optString("operator"), left)
            if (prompt.optBoolean("fresh")) call(ctx, if (why == null) op("shown", ask) else op("unshown", ask).put("why", why))
        }
        // The desk lapses a request on its own clock; this asks it then, so the notification goes with it.
        tick?.let { main.removeCallbacks(it) }
        if (soonest != Long.MAX_VALUE) {
            val app = ctx.applicationContext
            tick = Runnable { call(app, JSONObject().put("op", "tick")) }.also { main.postDelayed(it, soonest + 250) }
        }
    }

    /** Posts the request's notification: null once posted, else why it could not be. */
    private fun post(ctx: Context, ask: String, agentTitle: String, operator: String, left: Long): String? {
        val nm = manager(ctx)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N && !nm.areNotificationsEnabled()) return "RigDesk's notifications are off"
        val title = "$agentTitle asks to use this phone"
        val text = "Rig agent of ${operator.ifBlank { "your Rig" }}. Accept shares this screen with it, so it can see, tap and type, until you stop it."
        @Suppress("DEPRECATION")
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "Rig agents asking to use this phone", NotificationManager.IMPORTANCE_HIGH))
            Notification.Builder(ctx, CHANNEL).setTimeoutAfter(left)
        } else Notification.Builder(ctx).setPriority(Notification.PRIORITY_MAX)
        val hidden = (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(ctx, CHANNEL) else @Suppress("DEPRECATION") Notification.Builder(ctx))
            .setSmallIcon(android.R.drawable.ic_menu_view).setContentTitle("A Rig agent asks to use this phone").setContentText("Unlock to answer.").build()
        val accept = Intent(ACTION_ONE_TAP).setPackage(ctx.packageName).putExtra(EXTRA_ASK, ask)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val deny = Intent(ctx, JoinDeny::class.java).putExtra(EXTRA_ASK, ask)
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
        val notification = builder.setSmallIcon(android.R.drawable.ic_menu_view).setContentTitle(title).setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text)).setCategory(Notification.CATEGORY_CALL).setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(hidden).setAutoCancel(false).setOngoing(true).setGroup("$CHANNEL-$ask")
            .addAction(Notification.Action.Builder(null, "Accept", PendingIntent.getActivity(ctx, id(ask), accept, flags)).build())
            .addAction(Notification.Action.Builder(null, "Deny", PendingIntent.getBroadcast(ctx, id(ask), deny, flags)).build())
            .build()
        nm.notify(id(ask), notification)
        return null
    }

    private fun manager(ctx: Context) = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    private fun id(ask: String) = 7400 + (ask.hashCode() and 0xff)
}

/** The notification's Deny: the person's no, which the desk tells Rig with the request's nonce. */
class JoinDeny : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        val ask = intent.getStringExtra(Join.EXTRA_ASK) ?: return
        val refused = Join.deny(ctx, ask)
        if (refused.isNotEmpty()) Toast.makeText(ctx, refused, Toast.LENGTH_LONG).show()
    }
}
