package dev.rig.assistant

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * Setup over adb in one go: am broadcast -n <app>/dev.rig.assistant.Configure --es url … --es token … --es phone …
 * The manifest lets only a sender holding WRITE_SECURE_SETTINGS in, which adb's shell has and an ordinary app cannot get.
 */
class Configure : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        val edit = Config.prefs(ctx).edit()
        for (key in listOf("url", "token", "phone")) intent.getStringExtra(key)?.let { edit.putString(key, it.trim()) }
        edit.commit()
        val (config, why) = Config.load(ctx)
        resultData = if (config == null) "not configured: $why" else "configured: ${config.origin} as ${config.phone}"
        LinkService.start(ctx)
        Link.redial()
    }
}
