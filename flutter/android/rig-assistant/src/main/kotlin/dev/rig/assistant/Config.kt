package dev.rig.assistant

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import java.net.URI
import java.util.Locale

/** What the person typed on Rig Assistant's setup screen: Rig's address, its access token and this phone's name. Parsed once here. */
class Config private constructor(val origin: String, val token: String, val phone: String) {
    fun socket(): URI = URI.create(origin.replaceFirst(Regex("^http"), "ws") + "/api/contributions/rig-assistant/sockets/phone")

    companion object {
        private val PHONE = Regex("^[a-z0-9][a-z0-9_-]{0,47}$")

        fun prefs(ctx: Context): SharedPreferences = ctx.getSharedPreferences("rig-assistant", Context.MODE_PRIVATE)

        fun defaultPhone(): String {
            val name = (Build.MODEL ?: "phone").lowercase(Locale.ROOT).replace(Regex("[^a-z0-9]+"), "-").trim('-')
            return if (name.isEmpty()) "phone" else name.take(48)
        }

        /** Loopback or the tailnet (100.64.0.0/10, *.ts.net), where http is already encrypted by WireGuard. */
        fun privateHost(given: String): Boolean {
            val host = given.lowercase(Locale.ROOT)
            if (host == "localhost" || host == "127.0.0.1" || host.endsWith(".ts.net")) return true
            val parts = host.split('.').map { part -> part.toIntOrNull()?.takeIf { it.toString() == part } ?: return false }
            return parts.size == 4 && parts.all { it in 0..255 } && parts[0] == 100 && parts[1] in 64..127
        }

        private fun storedUrl(ctx: Context): String = prefs(ctx).getString("url", "").orEmpty().trim().trimEnd('/')

        /** Why Rig's token must not go to this address, whichever way it got here, or null. */
        private fun exposes(url: String): String? {
            val uri = try { URI.create(url) } catch (e: IllegalArgumentException) { return null }
            val host = uri.host ?: return null
            if (uri.scheme != "http" || privateHost(host)) return null
            return "Rig's address is plain http to $host, which would send its access token unencrypted, so this phone will not connect. " +
                "Change it in Rig Assistant to Rig's https address, or to its Tailscale address (100.x.y.z or a name ending in .ts.net)."
        }

        /** Why the address the phone holds is refused, for the person to see; null for any other state of the setup. */
        fun refusedAddress(ctx: Context): String? = exposes(storedUrl(ctx))

        /** The config, or why there is none. */
        fun load(ctx: Context): Pair<Config?, String> {
            val prefs = prefs(ctx)
            val url = storedUrl(ctx)
            val token = prefs.getString("token", "").orEmpty().trim()
            val phone = prefs.getString("phone", defaultPhone()).orEmpty().trim()
            if (url.isEmpty() || token.isEmpty()) return null to "not set up: enter Rig's address and access token"
            if (!PHONE.matches(phone)) return null to "the phone's name must be lowercase letters, digits, - and _"
            val ok = try {
                val uri = URI.create(url)
                (uri.scheme == "https" || uri.scheme == "http") && uri.host != null
            } catch (e: IllegalArgumentException) {
                false
            }
            if (!ok) return null to "Rig's address must look like https://host"
            exposes(url)?.let { return null to it }
            return Config(url, token, phone) to ""
        }
    }
}
