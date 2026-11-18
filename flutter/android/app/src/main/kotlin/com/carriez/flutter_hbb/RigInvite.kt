package com.carriez.flutter_hbb

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PersistableBundle
import android.util.Base64
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
import java.util.concurrent.atomic.AtomicInteger

/**
 * RigDesk: the invite the person at this phone makes for a Rig agent, from a notification while RigDesk's service runs;
 * capture itself starts only when a login is accepted. RigDesk keeps Rig's public pairing key and accepts only invites
 * Rig proves with its private half. The first key fetched with Rig Assistant's address and token is kept; a different one
 * later replaces it only when the person accepts it. One notification shows the state the device reports: Make invite
 * (for any agent, for one remembered from an earlier session, or for one by id), the open invite with Copy and Revoke, or
 * the agent using the phone with End, and the last refused attempt. Rig reaches the phone over the tailnet, and only
 * through a settings entry declaring this phone's ID, which the phone cannot see: every invite says so and shows the ID.
 */
class RigInvite(private val service: MainService) {
    private val main = Handler(Looper.getMainLooper())
    private val notifications = service.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    private val prefs = Config.prefs(service)
    private val known = service.getSharedPreferences("rigdesk-invite", Context.MODE_PRIVATE)
    private val setupChanged = SharedPreferences.OnSharedPreferenceChangeListener { _, changed ->
        if (changed == "url" || changed == "token") pairSoon()
    }
    private val pairing = AtomicInteger()

    @Volatile private var closed = false
    @Volatile private var pairAgainAtMs = 0L
    private var poller: Thread? = null

    // Main thread only.
    private var link: String? = null
    private var shown = ""
    private var pairWhy = "not paired with Rig yet"
    private var offeredKey: String? = null
    private var declinedKey: String? = null
    private var phoneId = ""

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
        // The invite lives in the process, which can outlast this service: none stays open that no notification shows.
        guarded("revoking the open invite") { FFI.inviteRevoke() }
        notifications.cancel(NOTIFY_ID)
    }

    /** A notification button pressed: true if it was one of ours. */
    fun handle(intent: Intent): Boolean {
        val verb = intent.action
        if (verb !in VERBS) return false
        guarded("$verb") {
            when (verb) {
                ACT_MINT -> mint(null)
                ACT_MINT_FOR -> mint(intent.getStringExtra(EXTRA_AGENT)
                    ?: RemoteInput.getResultsFromIntent(intent)?.getCharSequence(EXTRA_AGENT)?.toString()?.trim().orEmpty())
                ACT_COPY -> link?.let { copy(it) }
                ACT_REVOKE -> { FFI.inviteRevoke(); link = null }
                ACT_END -> if (!FFI.inviteEnd()) Log.w(TAG, "End pressed, but no agent was using the phone by invite")
                ACT_KEY_USE -> useKey(intent.getStringExtra(EXTRA_KEY))
                ACT_KEY_KEEP -> if (intent.getStringExtra(EXTRA_KEY) == offeredKey) { declinedKey = offeredKey; offeredKey = null }
            }
        }
        // Posted again even when nothing changed, which also ends a typed reply's spinner after a refused mint, and brings
        // back a notification swiped away: it is the only way to make an invite.
        shown = ""
        refresh()
        return true
    }

    /** An invite any agent may use, or with agent only that one: a mistyped id is not detected, so the notification names it. */
    private fun mint(agent: String?) {
        if (!MainService.isReady) {
            note(START_FIRST)
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
        val clip = ClipData.newPlainText("RigDesk invite", text)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            clip.description.extras = PersistableBundle().apply { putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true) }
        }
        clipboard.setPrimaryClip(clip)
        note("Invite link copied. ${reach()}")
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

    /** This runs on the thread of the process that captures the screen, so a failure here is logged and goes no further. */
    private fun guarded(what: String, body: () -> Unit) {
        try {
            body()
        } catch (e: Exception) {
            Log.w(TAG, "$what failed: $e")
        }
    }

    private fun reach(): String =
        "Rig reaches this phone only over Tailscale, and only if an entry in its settings declares this phone's ID, ${phoneId.ifEmpty { "shown in RigDesk" }}."

    /** Only the newest fetch counts, so a slow answer about an earlier setup cannot land after a later one. */
    private fun pairSoon() {
        val mine = pairing.incrementAndGet()
        Thread({
            val fetched = fetchKey()
            main.post {
                if (!closed && mine == pairing.get()) {
                    guarded("taking Rig's pairing key") { adopt(fetched) }
                    refresh()
                }
            }
        }, "rigdesk-invite-pair").apply { isDaemon = true; start() }
    }

    private fun keptKey(): String = FFI.getLocalOption(PAIR_KEY_OPTION)

    /** The key decides whose invites this phone accepts, so only the first is taken unasked. A key RigDesk already keeps still proves invites while Rig cannot be asked. */
    private fun adopt(fetched: Fetched) {
        pairWhy = fetched.why
        pairAgainAtMs = if (fetched.key == null) System.currentTimeMillis() + PAIR_RETRY_MS else 0L
        val key = fetched.key
        if (key == null) {
            // The offer came from a setup that no longer answers, so it is not left to be accepted.
            offeredKey = null
            return
        }
        val kept = keptKey()
        offeredKey = when {
            kept.isEmpty() -> {
                if (!FFI.invitePairKey(key)) pairWhy = NOT_A_KEY
                null
            }
            key == kept || key == declinedKey -> null
            else -> key
        }
    }

    /** The person accepted the key the notification showed: an invite made under the old one does not carry over. */
    private fun useKey(key: String?) {
        if (key == null || key != offeredKey) return
        FFI.inviteRevoke()
        link = null
        offeredKey = null
        if (!FFI.invitePairKey(key)) note(NOT_A_KEY)
    }

    private class Fetched(val key: String?, val why: String)

    /** Rig's public pairing key, or why it could not be had. A redirect is refused: the answer must come from the address the person set up. */
    private fun fetchKey(): Fetched {
        val (config, why) = Config.load(service)
        if (config == null) return Fetched(null, "Rig Assistant: $why")
        return try {
            val http = (URL(config.origin + PAIR_KEY_ROUTE).openConnection() as HttpURLConnection).apply {
                instanceFollowRedirects = false
                setRequestProperty("Authorization", "Bearer ${config.token}")
                connectTimeout = 15_000
                readTimeout = 15_000
            }
            try {
                val status = http.responseCode
                if (status != 200) return Fetched(null, Config.redirectRefused(status, "Rig") ?: "Rig answered HTTP $status for its pairing key")
                val key = JSONObject(http.inputStream.bufferedReader().use { it.readText() }).optString("key").trim()
                if (Base64.decode(key, Base64.DEFAULT).size == PUBLIC_KEY_BYTES) Fetched(key, "") else Fetched(null, NOT_A_KEY)
            } finally {
                http.disconnect()
            }
        } catch (e: Exception) {
            Log.w(TAG, "pairing key not fetched: $e")
            Fetched(null, "Rig did not answer for its pairing key")
        }
    }

    private fun poll() {
        while (!closed) {
            main.post(::refresh)
            if (pairAgainAtMs != 0L && System.currentTimeMillis() >= pairAgainAtMs) {
                pairAgainAtMs = 0L
                pairSoon()
            }
            try {
                Thread.sleep(POLL_MS)
            } catch (e: InterruptedException) {
                return
            }
        }
    }

    private fun refresh() {
        if (!closed) guarded("showing the invite state", ::show)
    }

    private fun show() {
        val state = JSONObject(FFI.inviteState())
        phoneId = state.optString("id")
        val open = state.optJSONObject("open")
        val holder = state.optJSONObject("holder")
        val refused = state.optJSONObject("refused")
        if (open == null) link = null
        val lastRefused = refused?.let { "Last refused ${clock(it.optLong("at_ms"))}: ${it.optString("reason")}" }
        val builder = NotificationCompat.Builder(service, CHANNEL)
            .setSmallIcon(R.mipmap.ic_stat_logo)
            .setColor(ContextCompat.getColor(service, R.color.primary))
            .setOnlyAlertOnce(true)
            .setOngoing(true)
            .setDeleteIntent(action(ACT_SWIPED))
        val offered = offeredKey
        val key: String
        val words: String
        when {
            holder != null -> {
                val title = holder.optString("title").ifEmpty { holder.optString("agent") }
                key = "held:${holder.optString("agent")}"
                if (key != shown) remember(holder.optString("agent"), title)
                words = "A Rig agent, by your invite. End takes the phone back."
                builder.setContentTitle("$title is using this phone")
                    .addAction(0, "End", action(ACT_END))
            }
            open != null -> {
                val forAgent = if (open.isNull("agent")) "" else open.optString("agent")
                key = "open:${open.optLong("until_ms")}:$forAgent:${link != null}:$lastRefused:$phoneId"
                val forWho = if (forAgent.isEmpty()) "" else " for $forAgent"
                val who = if (forAgent.isEmpty()) "The first Rig agent to use it gets this phone, once."
                          else "Only Rig agent $forAgent can use it, once. Revoke it if the id is wrong."
                words = listOfNotNull(lastRefused, who, reach()).joinToString(" ")
                builder.setContentTitle("Invite$forWho open until ${clock(open.optLong("until_ms"))}")
                if (link != null) builder.addAction(0, "Copy", action(ACT_COPY))
                builder.addAction(0, "Revoke", action(ACT_REVOKE))
            }
            offered != null -> {
                key = "key:$offered"
                words = "Rig now answers with key ${offered.take(KEY_SHOWN)}…, and this phone keeps ${keptKey().take(KEY_SHOWN)}…. " +
                    "Use the new one only if you reinstalled Rig or moved this phone to another Rig: whoever holds it can redeem this phone's invites."
                builder.setContentTitle("Rig's pairing key changed")
                    .addAction(0, "Use new key", action(ACT_KEY_USE, key = offered))
                    .addAction(0, "Keep current", action(ACT_KEY_KEEP, key = offered))
            }
            keptKey().isEmpty() -> {
                key = "unpaired:$pairWhy"
                words = pairWhy
                builder.setContentTitle("Rig invites are off")
                    .setOngoing(false)
                    .setDeleteIntent(null)
            }
            else -> {
                val last = remembered().firstOrNull()
                val ready = MainService.isReady
                key = "idle:$ready:$lastRefused:${last?.first}:$phoneId"
                val next = if (ready) "Make a link and give it to an agent in Rig." else START_FIRST
                words = listOfNotNull(lastRefused, next, reach()).joinToString(" ")
                builder.setContentTitle("Invite a Rig agent")
                if (ready) {
                    builder.addAction(0, "Make invite", action(ACT_MINT))
                    last?.let { (id, title) -> builder.addAction(0, "For ${title.ifEmpty { id }}", action(ACT_MINT_FOR, id)) }
                    builder.addAction(forAgentById())
                }
            }
        }
        builder.setContentText(words).setStyle(NotificationCompat.BigTextStyle().bigText(words))
        if (key == shown) return
        shown = key
        notifications.notify(NOTIFY_ID, builder.build())
    }

    private fun action(verb: String, agent: String? = null, key: String? = null): PendingIntent {
        val intent = Intent(service, MainService::class.java).setAction(verb)
        agent?.let { intent.putExtra(EXTRA_AGENT, it) }
        key?.let { intent.putExtra(EXTRA_KEY, it) }
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
                    else PendingIntent.FLAG_UPDATE_CURRENT
        return PendingIntent.getService(service, (verb + agent.orEmpty() + key.orEmpty()).hashCode(), intent, flags)
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
        private const val PAIR_RETRY_MS = 5 * 60_000L
        private const val PAIR_KEY_OPTION = "rigdesk-invite-key"
        private const val START_FIRST = "Tap Start service in RigDesk first: the phone answers an invite only while its service runs."
        private const val NOT_A_KEY = "Rig's pairing key was not one RigDesk takes"
        private const val PUBLIC_KEY_BYTES = 32
        private const val KEY_SHOWN = 8
        private const val PAIR_KEY_ROUTE = "/api/contributions/rigdesk/routes/pair_key"
        private const val REMEMBERED = 3
        private const val BY_ID_REQUEST = 7_302
        private const val EXTRA_AGENT = "agent"
        private const val EXTRA_KEY = "key"
        const val ACT_MINT = "RIG_INVITE_MINT"
        const val ACT_MINT_FOR = "RIG_INVITE_MINT_FOR"
        const val ACT_COPY = "RIG_INVITE_COPY"
        const val ACT_REVOKE = "RIG_INVITE_REVOKE"
        const val ACT_END = "RIG_INVITE_END"
        const val ACT_KEY_USE = "RIG_INVITE_KEY_USE"
        const val ACT_KEY_KEEP = "RIG_INVITE_KEY_KEEP"
        const val ACT_SWIPED = "RIG_INVITE_SWIPED"
        private val VERBS = setOf(ACT_MINT, ACT_MINT_FOR, ACT_COPY, ACT_REVOKE, ACT_END, ACT_KEY_USE, ACT_KEY_KEEP, ACT_SWIPED)
    }
}
