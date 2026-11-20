package dev.rig.assistant

import android.accessibilityservice.AccessibilityService
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build

/**
 * What Rig Assistant takes from the app it sits in, and nothing more: the app's accessibility service when the person has turned it on
 * (RigDesk's Input control), for the key tool and for starting apps. Rig Assistant works without it.
 */
object Host {
    @Volatile var input: AccessibilityService? = null
}

fun granted(ctx: Context, permission: String): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.M || ctx.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

/** A plain notification on the channel named, made if missing. */
@Suppress("DEPRECATION")
fun notification(ctx: Context, channel: String, title: String, text: String): Notification {
    val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        (ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
            .createNotificationChannel(NotificationChannel(channel, channelName(channel), NotificationManager.IMPORTANCE_HIGH))
        Notification.Builder(ctx, channel)
    } else Notification.Builder(ctx)
    return builder.setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle(title).setContentText(text)
        .setStyle(Notification.BigTextStyle().bigText(text)).setAutoCancel(true).setGroup(channel).build()
}

private fun channelName(channel: String) = when (channel) {
    "rig-told" -> "What the agent tells you"
    "rig-asks" -> "What Rig asks you to allow"
    else -> "From Rig"
}
