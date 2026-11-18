package com.carriez.flutter_hbb

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import dev.rig.assistant.Config
import ffi.FFI
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.text.DateFormat
import java.util.Date

/**
 * RigDesk: the invite the person at this phone makes for a Rig agent, from a notification while the phone shares its
 * screen. RigDesk keeps Rig's public pairing key, fetched with Rig Assistant's address and token whenever the service
 * starts or that setup changes, and accepts only invites Rig proves with its private half. One notification shows the
 * state the device reports: Make invite (for any agent, for one remembered from an earlier session, or for one by id),
 * the open invite with Copy and Revoke, or the agent using the phone with End, and the last refused attempt.
 */
class RigInvite(private val service: MainService) {
    private val main = Handler(Looper.getMainLooper())
    private val notifications = service.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    private val prefs = Config.prefs(service)
    private val known = service.getSharedPreferences("rigdesk-invite", Context.MODE_PRIVATE)
    private val setupChanged = SharedPreferences.OnSharedPreferenceChangeListener { _, _ -> pairSoon() }

    @Volatile private var closed = false
    private var poller: Thread? = null

    // Main thread only.
    private var link: String? = null
    private var shown = ""
    private var paired = false
    private var pairWhy = "not paired with Rig yet"

    fun start() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            notifications.createNotificationChannel(
                NotificationChannel(CHANNEL, "Rig invites", NotificationManager.IMPORTANCE_LOW).apply {
                    description = "Invite a Rig agent to this phone, and see who is using it"
                })
        }
        prefs.registerOnSharedPreferenceChangeListener(setupChanged)
        pairSoon()
        poller = Thread(::poll, "rigdesk-invite").apply { isDaemon = true; start() }
    }

    fun close() {
        closed = true
        prefs.unregisterOnSharedPreferenceChangeListener(setupChanged)
        poller?.interrupt()
        notifications.cancel(NOTIFY_ID)
    }

    /** A notification button pressed: true if it was one of ours. */
    fun handle(intent: Intent): Boolean {
        when (intent.action) {
            ACT_MINT -> mint(null)
            ACT_MINT_FOR -> mint(intent.getStringExtra(EXTRA_AGENT)
                ?: RemoteInput.getResultsFromIntent(intent)?.getCharSequence(EXTRA_AGENT)?.toString()?.trim().orEmpty())
            ACT_COPY -> link?.let { copy(it) }
            ACT_REVOKE -> { FFI.inviteRevoke(); link = null }
            ACT_END -> if (!FFI.inviteEnd()) Log.w(TAG, "End pressed, but no agent was using the phone by invite")
            else -> return false
        }
        refresh()
        return true
    }

    /** An invite any agent may use, or with agent only that one: a mistyped id is not detected, so the notification names it. */
    private fun mint(agent: String?) {
        if (!MainService.isStart) {
            note("Start sharing the screen in RigDesk first: the phone answers an invite only while it shares")
            return
        }
        val answer = JSONObject(if (agent == null) FFI.inviteMint() else FFI.inviteMintFor(agent))
        val made = answer.optString("link")
        if (made.isEmpty()) {
            note(answer.optString("error", "the invite was not made"))
            return
        }
        link = made
        copy(made)
    }

    private fun copy(text: String) {
        val clipboard = service.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("RigDesk invite", text))
        note("Invite link copied: give it to the agent in Rig")
    }

    /** The agents that used this phone by invite, newest first, as (id, title): the person can invite one of them again. */
    private fun remembered(): List<Pair<String, String>> = try {
        val list = JSONArray(known.getString("agents", "[]"))
        (0 until list.length()).map { list.getJSONObject(it) }.map { it.optString("agent") to it.optString("title") }.filter { it.first.isNotEmpty() }
    } catch (e: Exception) {
        emptyList()
    }

    private fun remember(agent: String, title: String) {
        val agents = listOf(agent to title) + remembered().filter { it.first != agent }
        val list = JSONArray()
        agents.take(REMEMBERED).forEach { (id, name) -> list.put(JSONObject().put("agent", id).put("title", name)) }
        known.edit().putString("agents", list.toString()).apply()
    }

    private fun note(words: String) {
        android.widget.Toast.makeText(service, words, android.widget.Toast.LENGTH_LONG).show()
    }

    private fun pairSoon() {
        Thread({
            val (paired, why) = pair()
            main.post { this.paired = paired; pairWhy = why; refresh() }
        }, "rigdesk-invite-pair").apply { isDaemon = true; start() }
    }

    /** Fetches Rig's public pairing key and gives it to RigDesk; whether the phone is paired, and why not. */
    private fun pair(): Pair<Boolean, String> {
        val (config, why) = Config.load(service)
        if (config == null) return false to "Rig Assistant is $why"
        return try {
            val http = (URL(config.origin + PAIR_KEY_ROUTE).openConnection() as HttpURLConnection).apply {
                setRequestProperty("Authorization", "Bearer ${config.token}")
                connectTimeout = 15_000
                readTimeout = 15_000
            }
            try {
                val status = http.responseCode
                if (status != 200) return false to "Rig answered HTTP $status for its pairing key"
                val key = JSONObject(http.inputStream.bufferedReader().use { it.readText() }).optString("key")
                if (FFI.invitePairKey(key)) true to "" else false to "Rig's pairing key was not one RigDesk takes"
            } finally {
                http.disconnect()
            }
        } catch (e: Exception) {
            Log.w(TAG, "pairing key not fetched: $e")
            false to "Rig did not answer for its pairing key: ${e.message ?: e.javaClass.simpleName}"
        }
    }

    private fun poll() {
        while (!closed) {
            main.post(::refresh)
            try {
                Thread.sleep(POLL_MS)
            } catch (e: InterruptedException) {
                return
            }
        }
    }

    private fun refresh() {
        if (closed) return
        val state = JSONObject(FFI.inviteState())
        val open = state.optJSONObject("open")
        val holder = state.optJSONObject("holder")
        val refused = state.optJSONObject("refused")
        if (open == null) link = null
        val lastRefused = refused?.let { "Last refused ${clock(it.optLong("at_ms"))}: ${it.optString("reason")}" }
        val builder = NotificationCompat.Builder(service, CHANNEL)
            .setSmallIcon(R.mipmap.ic_stat_logo)
            .setColor(ContextCompat.getColor(service, R.color.primary))
            .setOnlyAlertOnce(true)
        val key: String
        when {
            holder != null -> {
                val title = holder.optString("title").ifEmpty { holder.optString("agent") }
                key = "held:${holder.optString("agent")}"
                if (key != shown) remember(holder.optString("agent"), title)
                builder.setOngoing(true)
                    .setContentTitle("$title is using this phone")
                    .setContentText("A Rig agent, by your invite. End takes the phone back.")
                    .addAction(0, "End", action(ACT_END))
            }
            open != null -> {
                val forAgent = if (open.isNull("agent")) "" else open.optString("agent")
                key = "open:${open.optLong("until_ms")}:$forAgent:${link != null}:$lastRefused"
                val forWho = if (forAgent.isEmpty()) "" else " for $forAgent"
                builder.setOngoing(true)
                    .setContentTitle("Invite$forWho open until ${clock(open.optLong("until_ms"))}")
                    .setContentText(lastRefused ?: if (forAgent.isEmpty()) "The first Rig agent to use it gets this phone, once."
                                                   else "Only Rig agent $forAgent can use it, once. Revoke it if the id is wrong.")
                if (link != null) builder.addAction(0, "Copy", action(ACT_COPY))
                builder.addAction(0, "Revoke", action(ACT_REVOKE))
            }
            !paired -> {
                key = "unpaired:$pairWhy"
                builder.setOngoing(false)
                    .setContentTitle("Rig invites are off")
                    .setContentText(pairWhy)
            }
            else -> {
                val last = remembered().firstOrNull()
                key = "idle:${MainService.isStart}:$lastRefused:${last?.first}"
                builder.setOngoing(false)
                    .setContentTitle("Invite a Rig agent")
                    .setContentText(lastRefused ?: if (MainService.isStart) "Make an invite link and give it to an agent in Rig."
                                    else "Start sharing the screen first: the phone answers an invite only while it shares.")
                if (MainService.isStart) {
                    builder.addAction(0, "Make invite", action(ACT_MINT))
                    last?.let { (id, title) -> builder.addAction(0, "For ${title.ifEmpty { id }}", action(ACT_MINT_FOR, id)) }
                    builder.addAction(forAgentById())
                }
            }
        }
        lastRefused?.let { builder.setStyle(NotificationCompat.BigTextStyle().bigText(it)) }
        if (key == shown) return
        shown = key
        notifications.notify(NOTIFY_ID, builder.build())
    }

    private fun action(verb: String, agent: String? = null): PendingIntent {
        val intent = Intent(service, MainService::class.java).setAction(verb)
        agent?.let { intent.putExtra(EXTRA_AGENT, it) }
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                    else PendingIntent.FLAG_UPDATE_CURRENT
        return PendingIntent.getService(service, (verb + agent.orEmpty()).hashCode(), intent, flags)
    }

    /** "For an agent…": the person types the agent's id into the notification. RemoteInput fills the intent, so it is mutable. */
    private fun forAgentById(): NotificationCompat.Action {
        val intent = Intent(service, MainService::class.java).setAction(ACT_MINT_FOR)
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
                    else PendingIntent.FLAG_UPDATE_CURRENT
        val input = RemoteInput.Builder(EXTRA_AGENT).setLabel("Rig agent id, such as agent_1a2b3c4d").build()
        return NotificationCompat.Action.Builder(0, "For an agent…", PendingIntent.getService(service, BY_ID_REQUEST, intent, flags))
            .addRemoteInput(input).build()
    }

    private fun clock(ms: Long): String = DateFormat.getTimeInstance(DateFormat.SHORT).format(Date(ms))

    companion object {
        private const val TAG = "RigInvite"
        private const val CHANNEL = "rig-invite"
        private const val NOTIFY_ID = 7_301
        private const val POLL_MS = 2_000L
        private const val PAIR_KEY_ROUTE = "/api/contributions/rigdesk/routes/pair_key"
        private const val REMEMBERED = 3
        private const val BY_ID_REQUEST = 7_302
        private const val EXTRA_AGENT = "agent"
        const val ACT_MINT = "RIG_INVITE_MINT"
        const val ACT_MINT_FOR = "RIG_INVITE_MINT_FOR"
        const val ACT_COPY = "RIG_INVITE_COPY"
        const val ACT_REVOKE = "RIG_INVITE_REVOKE"
        const val ACT_END = "RIG_INVITE_END"
    }
}
