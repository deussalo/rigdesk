package com.carriez.flutter_hbb

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import dev.rig.assistant.Join
import ffi.FFI

/**
 * RigDesk: one tap. The person's Accept on Rig Assistant's notification lands here, and is their yes for that ask in the
 * one-tap desk (base::one_tap) at once. The first time only, it offers the switch for tapping and typing; then Android's own
 * Start now, unless sharing already runs; then the service. Once the phone listens, the desk tells Rig the phone is ready, so
 * Rig's knock for that ask comes in with no second Allow. Transparent: the person sees only those screens.
 */
class OneTapActivity : Activity() {
    private var ask: String? = null
    private var awaitingInput = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState != null) return finish()
        val asked = intent.getStringExtra(Join.EXTRA_ASK) ?: return finish()
        val (agent, refused) = Join.accept(this, asked)
        if (agent == null) {
            Toast.makeText(this, refused.ifEmpty { "That request has already ended" }, Toast.LENGTH_LONG).show()
            return finish()
        }
        ask = asked
        val prefs = getSharedPreferences(KEY_SHARED_PREFERENCES, MODE_PRIVATE)
        if (InputService.isOpen || prefs.getBoolean(KEY_ONE_TAP_ASKED_INPUT, false)) return share()
        prefs.edit().putBoolean(KEY_ONE_TAP_ASKED_INPUT, true).apply()
        AlertDialog.Builder(this)
            .setTitle("Let $agent tap and type too?")
            .setMessage("RigDesk's input control is off, so the agent could only see the screen. Turn it on once in Accessibility > RigDesk Input, then come back. This is asked once.")
            .setPositiveButton("Turn on") { _, _ ->
                awaitingInput = true
                startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
            }
            .setNegativeButton("Screen only") { _, _ -> share() }
            .setOnCancelListener { share() }
            .show()
    }

    override fun onResume() {
        super.onResume()
        if (awaitingInput) {
            awaitingInput = false
            share()
        }
    }

    private fun share() {
        val asked = ask ?: return finish()
        if (Join.allowed(this, asked).first == null) return finish()
        if (MainService.isReady) {
            OneTap.ready(applicationContext, asked)
            return finish()
        }
        val projection = getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val consent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            projection.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            projection.createScreenCaptureIntent()
        }
        startActivityForResult(consent, REQ_ONE_TAP_PROJECTION)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQ_ONE_TAP_PROJECTION) return
        val asked = ask ?: return finish()
        when {
            Join.allowed(this, asked).first == null -> Log.i(TAG, "$asked ended while Start now showed: nothing is shared")
            resultCode == RESULT_OK && data != null -> {
                val service = Intent(this, MainService::class.java).apply {
                    action = ACT_INIT_MEDIA_PROJECTION_AND_SERVICE
                    putExtra(EXT_MEDIA_PROJECTION_RES_INTENT, data)
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) startForegroundService(service) else startService(service)
                OneTap.ready(applicationContext, asked)
            }
            else -> Join.deny(this, asked, "the person cancelled Android's Start now")
        }
        finish()
    }

    companion object {
        private const val TAG = "RigOneTap"
        private const val REQ_ONE_TAP_PROJECTION = 401
        private const val KEY_ONE_TAP_ASKED_INPUT = "KEY_ONE_TAP_ASKED_INPUT"
    }
}

/** The phone's side of one tap once sharing is under way: it waits until the phone listens, then the desk says ready. */
object OneTap {
    private const val TAG = "RigOneTap"
    private const val LISTEN_WAIT_MS = 15_000L

    fun ready(ctx: Context, ask: String) {
        Thread({
            val until = SystemClock.elapsedRealtime() + LISTEN_WAIT_MS
            while (!MainService.isReady && SystemClock.elapsedRealtime() < until) SystemClock.sleep(100)
            if (MainService.isReady) FFI.startService()
            while (!FFI.rigListening() && SystemClock.elapsedRealtime() < until) SystemClock.sleep(100)
            when {
                Join.allowed(ctx, ask).first == null -> Log.i(TAG, "$ask ended before the phone was ready")
                !MainService.isReady -> Join.notReady(ctx, ask, "screen sharing did not start within ${LISTEN_WAIT_MS / 1000} s")
                !FFI.rigListening() -> Join.notReady(ctx, ask, "RigDesk is not listening for Rig: turn on direct IP access in its settings")
                else -> Join.ready(ctx, ask)
            }
        }, "rig-one-tap").start()
    }
}
