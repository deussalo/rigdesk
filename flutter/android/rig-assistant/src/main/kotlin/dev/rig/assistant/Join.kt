package dev.rig.assistant

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.SystemClock
import android.util.Log
import org.json.JSONObject
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList

/** A Rig agent's request to see and use this phone, as Rig sent it; lapses on the phone's own clock. */
data class JoinRequest(
    val ask: String,
    val nonce: String,
    val agentId: String,
    val agentTitle: String,
    val graph: String,
    val operator: String,
    val lapseMs: Long,
    val receivedAt: Long,
) {
    fun leftMs(): Long = lapseMs - (SystemClock.elapsedRealtime() - receivedAt)
}

/**
 * One tap: a Rig agent's request to use this phone, posted as a notification while RigDesk's screen sharing is off, since this
 * link is up whenever the phone is. Accept opens the app's one-tap screen (ACTION_ONE_TAP, in the app around this module),
 * which starts sharing and tells Rig the phone is ready; Deny tells Rig no. Both carry the request's ask and nonce. A request
 * Rig ends (answered in Rig, withdrawn, lapsed, joined) leaves the notifications at once, and none outlives its lapse.
 */
object Join {
    private const val TAG = "RigAssistantJoin"
    private const val CHANNEL = "rig-join"
    const val ACTION_ONE_TAP = "dev.rig.assistant.ONE_TAP"
    const val EXTRA_ASK = "ask"
    private val open = ConcurrentHashMap<String, JoinRequest>()
    private val endings = CopyOnWriteArrayList<(String) -> Unit>()

    fun request(ask: String): JoinRequest? = open[ask]?.takeIf { it.leftMs() > 0 }

    /** Told each ask that ends on the phone, whoever ended it: the app drops an Accept it holds. */
    fun onEnded(listener: (String) -> Unit) {
        endings.add(listener)
    }

    /** Rig's join_request frame. A resend after a reconnect posts the same notification again. */
    fun requested(ctx: Context, frame: JSONObject) {
        val agent = frame.optJSONObject("agent") ?: JSONObject()
        val request = JoinRequest(
            ask = frame.optString("ask"), nonce = frame.optString("nonce"),
            agentId = agent.optString("id"), agentTitle = agent.optString("title").ifBlank { agent.optString("id") },
            graph = agent.optString("graph"), operator = frame.optString("operator"),
            lapseMs = frame.optLong("lapse_s") * 1000, receivedAt = SystemClock.elapsedRealtime(),
        )
        if (request.ask.isBlank() || request.nonce.isBlank() || request.agentId.isBlank() || request.lapseMs <= 0) {
            Log.w(TAG, "join_request missing its ask, nonce, agent or lapse: $frame")
            return
        }
        val known = open.putIfAbsent(request.ask, request)
        post(ctx, known ?: request)
    }

    /** Rig's join_end frame: the request is over, however it ended. */
    fun ended(ctx: Context, ask: String) {
        open.remove(ask)
        manager(ctx).cancel(id(ask))
        endings.forEach { it(ask) }
    }

    /** The person tapped Accept: the notification goes, the request stays until Rig ends it. */
    fun accepted(ctx: Context, ask: String) {
        manager(ctx).cancel(id(ask))
    }

    /** What the phone says of this request, to Rig: ready, deny, or failed with why. A deny ends it here too. */
    fun said(ctx: Context, ask: String, said: String, reason: String = "") {
        val request = open[ask] ?: return
        Link.join(ask, request.nonce, said, reason)
        if (said != "ready") ended(ctx, ask)
    }

    private fun post(ctx: Context, request: JoinRequest) {
        val left = request.leftMs()
        if (left <= 0) return
        val nm = manager(ctx)
        val title = "${request.agentTitle} asks to use this phone"
        val text = "Rig agent of ${request.operator.ifBlank { "your Rig" }}. Accept shares this screen with it, so it can see, tap and type, until you stop it."
        @Suppress("DEPRECATION")
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "Rig agents asking to use this phone", NotificationManager.IMPORTANCE_HIGH))
            Notification.Builder(ctx, CHANNEL).setTimeoutAfter(left)
        } else Notification.Builder(ctx).setPriority(Notification.PRIORITY_MAX)
        val hidden = (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(ctx, CHANNEL) else @Suppress("DEPRECATION") Notification.Builder(ctx))
            .setSmallIcon(android.R.drawable.ic_menu_view).setContentTitle("A Rig agent asks to use this phone").setContentText("Unlock to answer.").build()
        val accept = Intent(ACTION_ONE_TAP).setPackage(ctx.packageName).putExtra(EXTRA_ASK, request.ask)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val deny = Intent(ctx, JoinDeny::class.java).putExtra(EXTRA_ASK, request.ask)
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
        val notification = builder.setSmallIcon(android.R.drawable.ic_menu_view).setContentTitle(title).setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text)).setCategory(Notification.CATEGORY_CALL).setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(hidden).setAutoCancel(false).setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Accept", PendingIntent.getActivity(ctx, id(request.ask), accept, flags)).build())
            .addAction(Notification.Action.Builder(null, "Deny", PendingIntent.getBroadcast(ctx, id(request.ask), deny, flags)).build())
            .build()
        nm.notify(id(request.ask), notification)
    }

    private fun manager(ctx: Context) = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    private fun id(ask: String) = 7400 + (ask.hashCode() and 0xff)
}

/** The notification's Deny: the person's no, told to Rig with the request's nonce. */
class JoinDeny : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        val ask = intent.getStringExtra(Join.EXTRA_ASK) ?: return
        Join.said(ctx, ask, "deny")
    }
}
