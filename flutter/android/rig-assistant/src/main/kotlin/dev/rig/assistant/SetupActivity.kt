package dev.rig.assistant

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.ComponentName
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.text.InputType
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView

/**
 * Rig Assistant's own setup, apart from RigDesk's: Rig's address, an access token and the phone's name, then each switch the
 * phone asks for, marked when on. Rig Assistant needs nothing of RigDesk's screen sharing.
 */
class SetupActivity : Activity() {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var state: TextView
    private val switches = mutableListOf<Pair<Button, () -> Boolean>>()
    private val refresh = object : Runnable {
        override fun run() {
            val who = Link.resident.ifEmpty { "no agent resident yet: assign one on Rig's Rig Assistant page" }
            state.text = "${Link.state}\n${if (Link.connected()) "Resident: $who" else ""}"
            for ((button, on) in switches) button.alpha = if (on()) 0.5f else 1f
            main.postDelayed(this, 1_000)
        }
    }

    private fun dp(value: Int) = Math.round(value * resources.displayMetrics.density)

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        LinkService.start(this)
        val prefs = Config.prefs(this)
        val column = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(dp(20), dp(24), dp(20), dp(24)) }
        fun label(text: String) = column.addView(TextView(this).apply { this.text = text; setPadding(0, dp(12), 0, 0) })
        fun field(key: String, hint: String, fallback: String, secret: Boolean) = EditText(this).apply {
            this.hint = hint
            setText(prefs.getString(key, fallback))
            inputType = if (secret) InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD else InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            column.addView(this)
        }
        column.addView(TextView(this).apply { text = "Rig Assistant"; textSize = 22f })
        label("Rig's address")
        val url = field("url", "https://your-rig.example.ts.net", "", false)
        label("Access token")
        val token = field("token", "Rig access token", "", true)
        label("This phone's name")
        val phone = field("phone", Config.defaultPhone(), Config.defaultPhone(), false)
        column.addView(Button(this).apply {
            text = "Save and connect"
            setOnClickListener {
                prefs.edit().putString("url", url.text.toString().trim()).putString("token", token.text.toString().trim())
                    .putString("phone", phone.text.toString().trim()).apply()
                Link.redial()
            }
        })
        state = TextView(this).apply { setPadding(0, dp(8), 0, dp(8)) }
        column.addView(state)
        label("Switches (dimmed once on)")
        fun switch(text: String, on: () -> Boolean, open: () -> Unit) {
            val button = Button(this).apply { this.text = text; setOnClickListener { try { open() } catch (e: Exception) { state.text = "$e" } } }
            column.addView(button)
            switches.add(button to on)
        }
        switch("Default assistant (hold Home)", ::isAssistant) { startActivity(Intent(Settings.ACTION_VOICE_INPUT_SETTINGS)) }
        switch("Microphone", { granted(this, Manifest.permission.RECORD_AUDIO) }) { ask(Manifest.permission.RECORD_AUDIO) }
        if (Build.VERSION.SDK_INT >= 33)
            switch("Notifications", { granted(this, Manifest.permission.POST_NOTIFICATIONS) }) { ask(Manifest.permission.POST_NOTIFICATIONS) }
        switch("Show over other apps (tell)", { Build.VERSION.SDK_INT < 23 || Settings.canDrawOverlays(this) }) {
            startActivity(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName")))
        }
        switch("Notification access", { RigNotifications.live != null || listening() }) {
            startActivity(Intent("android.settings.ACTION_NOTIFICATION_LISTENER_SETTINGS"))
        }
        switch("SMS and contacts", { granted(this, Manifest.permission.SEND_SMS) && granted(this, Manifest.permission.READ_CONTACTS) }) {
            ask(Manifest.permission.SEND_SMS, Manifest.permission.READ_CONTACTS)
        }
        if (Build.VERSION.SDK_INT >= 30) switch("All files access", { Environment.isExternalStorageManager() }) {
            startActivity(Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.parse("package:$packageName")))
        }
        if (Build.VERSION.SDK_INT >= 23) switch("Run in the background", {
            (getSystemService(POWER_SERVICE) as android.os.PowerManager).isIgnoringBatteryOptimizations(packageName)
        }) {
            @Suppress("BatteryLife")
            startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
        }
        setContentView(ScrollView(this).apply { addView(column) })
        pair(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        pair(intent)
    }

    /** A pairing link: rigassistant://pair?url=…&token=…&phone=…. Anyone can fire one, so it takes the person's Pair tap. */
    private fun pair(intent: Intent?) {
        val link = intent?.data ?: return
        if (link.scheme != "rigassistant" || link.host != "pair") return
        val url = link.getQueryParameter("url").orEmpty().trim()
        val token = link.getQueryParameter("token").orEmpty().trim()
        val phone = link.getQueryParameter("phone").orEmpty().trim().ifEmpty { Config.defaultPhone() }
        val host = try { Uri.parse(url).host } catch (e: Exception) { null }
        if (host == null || token.isEmpty()) {
            state.text = "That pairing link is incomplete: it needs Rig's address and a token"
            return
        }
        AlertDialog.Builder(this)
            .setTitle("Pair with Rig?")
            .setMessage("Connect this phone to the Rig at $host as \"$phone\". Its agent can then use the phone's tools you allow.")
            .setNegativeButton("Cancel", null)
            .setPositiveButton("Pair") { _, _ ->
                Config.prefs(this).edit().putString("url", url).putString("token", token).putString("phone", phone).apply()
                Link.redial()
                recreate()
                if (!isAssistant()) startActivity(Intent(Settings.ACTION_VOICE_INPUT_SETTINGS))
            }
            .show()
        setIntent(Intent(this, SetupActivity::class.java))
    }

    override fun onResume() {
        super.onResume()
        main.post(refresh)
    }

    override fun onPause() {
        main.removeCallbacks(refresh)
        super.onPause()
    }

    private fun ask(vararg permissions: String) {
        if (Build.VERSION.SDK_INT >= 23) requestPermissions(permissions, 1)
    }

    private fun isAssistant(): Boolean = if (Build.VERSION.SDK_INT >= 29)
        (getSystemService(ROLE_SERVICE) as android.app.role.RoleManager).isRoleHeld(android.app.role.RoleManager.ROLE_ASSISTANT)
    else secure("voice_interaction_service").let { ComponentName.unflattenFromString(it)?.packageName == packageName }

    private fun listening(): Boolean =
        secure("enabled_notification_listeners").split(':').any { ComponentName.unflattenFromString(it) == ComponentName(this, RigNotifications::class.java) }

    /** A secure setting, or empty where Android does not let an app read it. */
    private fun secure(key: String): String = try { Settings.Secure.getString(contentResolver, key).orEmpty() } catch (e: SecurityException) { "" }
}
