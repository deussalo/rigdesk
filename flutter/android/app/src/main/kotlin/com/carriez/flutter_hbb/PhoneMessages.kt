package com.carriez.flutter_hbb

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.ContactsContract
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.telephony.SmsManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/** RigDesk's notification access: while the person has it on, the phone's tools read, answer and dismiss notifications. */
class RigNotifications : NotificationListenerService() {
    override fun onListenerConnected() { live = this }
    override fun onListenerDisconnected() { if (live === this) live = null }
    override fun onDestroy() { if (live === this) live = null; super.onDestroy() }

    companion object {
        @Volatile var live: RigNotifications? = null
    }
}

/** The phone's notification, SMS and contact tools. */
class PhoneMessages(private val ctx: Context) {
    private val posted = AtomicInteger(0)

    fun specs(): List<JSONObject> = listOf(
        toolSpec("notifications", "Notifications", true, "The notifications on the phone, newest first: each one's key, app, time, title, " +
            "text and actions, marked reply for an action that takes typed text (a messaging app's Reply).", emptyList(),
            "app" to toolProp("string", "Only this app's, by package.")),
        toolSpec("notification_reply", "Reply", false, "Answer a notification inline with text, through its reply action " +
            "(WhatsApp, Signal, Messages and other messaging apps), without opening the app.", listOf("key", "text"),
            "key" to toolProp("string", "The notification's key, from notifications."),
            "text" to toolProp("string", "The reply.")),
        toolSpec("notification_action", "Notification action", false, "Press one of a notification's action buttons, by its title " +
            "(Mark as read, Archive, …); with no action, open the notification as a tap would.", listOf("key"),
            "key" to toolProp("string", "The notification's key, from notifications."),
            "action" to toolProp("string", "The action's title, as notifications lists it.")),
        toolSpec("notification_dismiss", "Dismiss", false, "Dismiss a notification, as a swipe would.", listOf("key"),
            "key" to toolProp("string", "The notification's key, from notifications.")),
        toolSpec("notify", "Notify", false, "Post a notification on the phone from Rig, which stays until the person clears it " +
            "(tell is for something to see and hear now).", listOf("text"),
            "title" to toolProp("string", "Its title (default Rig)."),
            "text" to toolProp("string", "What it says.")),
        toolSpec("sms_send", "Send SMS", false, "Send an SMS from the phone's own number, split into parts as needed; answers once " +
            "the network took every part.", listOf("to", "text"),
            "to" to toolProp("string", "A phone number (contacts finds one)."),
            "text" to toolProp("string", "The message.")),
        toolSpec("contacts", "Contacts", true, "Search the phone's contacts by name, number or email: each match's name, numbers " +
            "and emails, at most 20.", listOf("query"),
            "query" to toolProp("string", "Part of a name, number or email.")),
    )

    /** The answer to a call of one of these tools, or null for a tool that is not one of them. */
    fun call(name: String, args: JSONObject): String? = when (name) {
        "notifications" -> list(args.optString("app").trim())
        "notification_reply" -> reply(args.optString("key"), args.optString("text"))
        "notification_action" -> press(args.optString("key"), args.optString("action").trim())
        "notification_dismiss" -> dismiss(args.optString("key"))
        "notify" -> post(args.optString("title").trim().ifEmpty { "Rig" }, args.optString("text").trim())
        "sms_send" -> sms(args.optString("to").trim(), args.optString("text"))
        "contacts" -> contacts(args.optString("query").trim())
        else -> null
    }

    private fun listener(): RigNotifications = RigNotifications.live ?: throw Refusal(
        "RigDesk has no notification access: turn it on in the phone's Settings > Notifications > Device & app notifications > RigDesk " +
            "(or adb shell cmd notification allow_listener ${ComponentName(ctx, RigNotifications::class.java).flattenToString()})")

    private fun active(): List<StatusBarNotification> = listener().activeNotifications?.toList().orEmpty()

    private fun find(key: String): StatusBarNotification =
        active().firstOrNull { it.key == key } ?: throw Refusal("no notification $key now: notifications lists those there are")

    private fun label(pkg: String): String = try {
        ctx.packageManager.getApplicationLabel(ctx.packageManager.getApplicationInfo(pkg, 0)).toString()
    } catch (e: PackageManager.NameNotFoundException) {
        pkg
    }

    private fun takesText(a: Notification.Action) = a.remoteInputs?.any { it.allowFreeFormInput } == true

    private fun list(app: String): String {
        val when_ = SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.ROOT)
        val shown = active().filter { app.isEmpty() || it.packageName == app }
            .filter { (it.notification.flags and Notification.FLAG_GROUP_SUMMARY) == 0 }
            .sortedByDescending { it.postTime }
        if (shown.isEmpty()) return if (app.isEmpty()) "no notifications" else "no notifications from $app"
        return shown.joinToString("\n\n") { n ->
            val x = n.notification.extras
            val title = x.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
            val text = (x.getCharSequence(Notification.EXTRA_BIG_TEXT) ?: x.getCharSequence(Notification.EXTRA_TEXT))?.toString().orEmpty()
            val lines = x.getCharSequenceArray(Notification.EXTRA_TEXT_LINES)?.joinToString("\n") { "  $it" }
            val actions = n.notification.actions.orEmpty().joinToString(", ") { a -> a.title.toString() + if (takesText(a)) " (reply)" else "" }
            buildString {
                append("key ${n.key}\n${label(n.packageName)} (${n.packageName}) at ${when_.format(Date(n.postTime))}")
                if (n.isOngoing) append(", ongoing")
                if (title.isNotEmpty()) append("\n$title")
                if (text.isNotEmpty()) append("\n$text")
                if (!lines.isNullOrEmpty()) append("\n$lines")
                if (actions.isNotEmpty()) append("\nactions: $actions")
            }
        }
    }

    private fun reply(key: String, text: String): String {
        if (text.isBlank()) throw Refusal("notification_reply takes text")
        val n = find(key)
        val action = n.notification.actions.orEmpty().firstOrNull { takesText(it) }
            ?: throw Refusal("${label(n.packageName)}'s notification takes no typed reply: notification_action or open the app")
        val inputs = action.remoteInputs
        val answers = Bundle().apply { inputs.filter { it.allowFreeFormInput }.forEach { putCharSequence(it.resultKey, text) } }
        val filled = Intent().addFlags(Intent.FLAG_RECEIVER_FOREGROUND)
        RemoteInput.addResultsToIntent(inputs, filled, answers)
        try {
            action.actionIntent.send(ctx, 0, filled)
        } catch (e: PendingIntent.CanceledException) {
            throw Refusal("${label(n.packageName)} withdrew that reply action: list the notifications again")
        }
        return "replied to ${label(n.packageName)}'s notification through ${action.title}"
    }

    private fun press(key: String, title: String): String {
        val n = find(key)
        val intent = if (title.isEmpty()) {
            n.notification.contentIntent ?: throw Refusal("${label(n.packageName)}'s notification opens nothing when tapped")
        } else {
            val actions = n.notification.actions.orEmpty()
            (actions.firstOrNull { it.title.toString() == title } ?: actions.firstOrNull { it.title.toString().equals(title, ignoreCase = true) }
                ?: throw Refusal("no action $title on it: ${actions.joinToString(", ") { it.title }.ifEmpty { "it has none" }}")).actionIntent
        }
        try {
            intent.send()
        } catch (e: PendingIntent.CanceledException) {
            throw Refusal("${label(n.packageName)} withdrew that action: list the notifications again")
        }
        if (title.isEmpty() && (n.notification.flags and Notification.FLAG_AUTO_CANCEL) != 0) listener().cancelNotification(n.key)
        return if (title.isEmpty()) "opened ${label(n.packageName)}'s notification" else "pressed $title on ${label(n.packageName)}'s notification"
    }

    private fun dismiss(key: String): String {
        val n = find(key)
        if (!n.isClearable) throw Refusal("${label(n.packageName)}'s notification is ongoing: Android lets no one dismiss it")
        listener().cancelNotification(n.key)
        return "dismissed ${label(n.packageName)}'s notification"
    }

    private fun post(title: String, text: String): String {
        if (text.isEmpty()) throw Refusal("notify takes text")
        val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (!nm.areNotificationsEnabled()) throw Refusal("RigDesk's notifications are off: turn them on in the phone's Settings > Apps > RigDesk")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "From Rig", NotificationManager.IMPORTANCE_DEFAULT))
        nm.notify(POSTED_BASE + posted.incrementAndGet() % 1000, NotificationCompat.Builder(ctx, CHANNEL).setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle(title).setContentText(text).setStyle(NotificationCompat.BigTextStyle().bigText(text)).setAutoCancel(true).build())
        return "posted"
    }

    private fun granted(permission: String) = ContextCompat.checkSelfPermission(ctx, permission) == PackageManager.PERMISSION_GRANTED

    private fun sms(to: String, text: String): String {
        if (to.isEmpty() || text.isEmpty()) throw Refusal("sms_send takes to and text")
        if (!granted(Manifest.permission.SEND_SMS))
            throw Refusal("RigDesk may not send SMS: allow it in the phone's Settings > Apps > RigDesk > Permissions > SMS (or adb shell pm grant ${ctx.packageName} android.permission.SEND_SMS)")
        val sms: SmsManager = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) ctx.getSystemService(SmsManager::class.java)
            else SmsManager.getDefault()
        val parts = sms.divideMessage(text)
        val action = "${ctx.packageName}.SMS_SENT.${System.nanoTime()}"
        val left = CountDownLatch(parts.size)
        val failed = ArrayList<String>()
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context, i: Intent) {
                if (resultCode != Activity.RESULT_OK) synchronized(failed) { failed.add(SENT_ERRORS[resultCode] ?: "error $resultCode") }
                left.countDown()
            }
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) ctx.registerReceiver(receiver, IntentFilter(action), Context.RECEIVER_NOT_EXPORTED)
        else ctx.registerReceiver(receiver, IntentFilter(action))
        try {
            val flags = PendingIntent.FLAG_ONE_SHOT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0)
            val sent = ArrayList((0 until parts.size).map { PendingIntent.getBroadcast(ctx, it, Intent(action).setPackage(ctx.packageName), flags) })
            sms.sendMultipartTextMessage(to, null, parts, sent, null)
            if (!left.await(SMS_WAIT_S, TimeUnit.SECONDS)) throw Refusal("sent to $to, but the network confirmed only ${parts.size - left.count} of ${parts.size} parts within $SMS_WAIT_S s")
        } finally {
            ctx.unregisterReceiver(receiver)
        }
        synchronized(failed) { if (failed.isNotEmpty()) throw Refusal("the SMS to $to failed: ${failed.distinct().joinToString(", ")}") }
        return "sent to $to" + if (parts.size > 1) " in ${parts.size} parts" else ""
    }

    private fun contacts(query: String): String {
        if (query.isEmpty()) throw Refusal("contacts takes a query")
        if (!granted(Manifest.permission.READ_CONTACTS))
            throw Refusal("RigDesk may not read contacts: allow it in the phone's Settings > Apps > RigDesk > Permissions > Contacts (or adb shell pm grant ${ctx.packageName} android.permission.READ_CONTACTS)")
        val found = LinkedHashMap<Long, String>()
        fun match(uri: Uri, idColumn: String, nameColumn: String) {
            ctx.contentResolver.query(uri, arrayOf(idColumn, nameColumn), null, null, null)?.use { c ->
                while (c.moveToNext() && found.size < 20) found.putIfAbsent(c.getLong(0), c.getString(1) ?: "(no name)")
            }
        }
        val q = Uri.encode(query)
        match(Uri.withAppendedPath(ContactsContract.Contacts.CONTENT_FILTER_URI, q), ContactsContract.Contacts._ID, ContactsContract.Contacts.DISPLAY_NAME)
        match(Uri.withAppendedPath(ContactsContract.CommonDataKinds.Phone.CONTENT_FILTER_URI, q),
            ContactsContract.CommonDataKinds.Phone.CONTACT_ID, ContactsContract.CommonDataKinds.Phone.DISPLAY_NAME)
        match(Uri.withAppendedPath(ContactsContract.CommonDataKinds.Email.CONTENT_FILTER_URI, q),
            ContactsContract.CommonDataKinds.Email.CONTACT_ID, ContactsContract.CommonDataKinds.Email.DISPLAY_NAME)
        if (found.isEmpty()) return "no contact matches $query"
        return found.entries.joinToString("\n") { (id, name) ->
            val numbers = data(ContactsContract.CommonDataKinds.Phone.CONTENT_URI, ContactsContract.CommonDataKinds.Phone.NUMBER,
                ContactsContract.CommonDataKinds.Phone.CONTACT_ID, id)
            val emails = data(ContactsContract.CommonDataKinds.Email.CONTENT_URI, ContactsContract.CommonDataKinds.Email.ADDRESS,
                ContactsContract.CommonDataKinds.Email.CONTACT_ID, id)
            (listOf(name) + numbers + emails).joinToString(" — ")
        }
    }

    private fun data(uri: Uri, column: String, contactColumn: String, id: Long): List<String> {
        val out = LinkedHashSet<String>()
        ctx.contentResolver.query(uri, arrayOf(column), "$contactColumn = ?", arrayOf(id.toString()), null)?.use { c ->
            while (c.moveToNext()) c.getString(0)?.let { out.add(it) }
        }
        return out.toList()
    }

    companion object {
        private const val CHANNEL = "rig-posted"
        private const val POSTED_BASE = 7400
        private const val SMS_WAIT_S = 30L
        private val SENT_ERRORS = mapOf(
            SmsManager.RESULT_ERROR_GENERIC_FAILURE to "generic failure",
            SmsManager.RESULT_ERROR_NO_SERVICE to "no service",
            SmsManager.RESULT_ERROR_NULL_PDU to "null PDU",
            SmsManager.RESULT_ERROR_RADIO_OFF to "radio off",
        )
    }
}
