package dev.rig.assistant

import android.app.Activity
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings

/** A tool refused for want of a switch the person has not turned on yet: the phone shows one prompt that opens it. */
class Needs(why: String, val what: String, val opens: Intent) : Refusal(why)

/** The switches asked for at first need, each with the screen that turns it on. */
object Switch {
    fun runtime(ctx: Context, vararg permissions: String): Intent =
        Intent(ctx, GrantActivity::class.java).putExtra(GrantActivity.PERMISSIONS, permissions)

    fun notificationAccess(): Intent = Intent("android.settings.ACTION_NOTIFICATION_LISTENER_SETTINGS")

    fun allFiles(ctx: Context): Intent =
        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.parse("package:${ctx.packageName}"))

    /** One notification per switch, replaced rather than stacked when an agent asks again. */
    fun prompt(ctx: Context, needs: Needs) {
        val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val tap = PendingIntent.getActivity(ctx, needs.what.hashCode(), needs.opens.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0))
        val shown = notification(ctx, "rig-asks", "Rig asks for ${needs.what}", "Tap to allow it. ${needs.message}").apply { contentIntent = tap }
        nm.notify(PROMPT_BASE + (needs.what.hashCode() and 0xff), shown)
    }

    private const val PROMPT_BASE = 7400
}

/**
 * Asks for runtime permissions (the microphone at the first hold of Home, SMS or contacts when an agent first needs them),
 * then gets out of the way. Opened from the assistant's sheet, it opens the sheet again once the microphone is allowed.
 */
class GrantActivity : Activity() {
    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        val asked = intent.getStringArrayExtra(PERMISSIONS)?.filterNot { granted(this, it) }.orEmpty()
        if (asked.isEmpty() || Build.VERSION.SDK_INT < 23) { done(); return }
        requestPermissions(asked.toTypedArray(), 1)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        done()
    }

    private fun done() {
        if (intent.getBooleanExtra(REOPEN, false) && granted(this, android.Manifest.permission.RECORD_AUDIO)) AssistService.reopen()
        finish()
    }

    companion object {
        const val PERMISSIONS = "permissions"
        const val REOPEN = "reopen"
    }
}
