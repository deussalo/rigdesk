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
import dev.rig.assistant.JoinRequest
import ffi.FFI
import java.util.Collections

/**
 * RigDesk: one tap. The person's Accept on Rig Assistant's notification lands here, or the operator's Accept in Rig does, at once. The first time only, it offers the switch for
 * tapping and typing; then Android's own Start now, unless sharing already runs; then the service. Once the phone listens, its
 * Accept is filed for that ask (base::one_tap) and Rig is told the phone is ready, so Rig's knock for that ask comes in with no
 * second Allow. Transparent: the person sees only those screens.
 */
class OneTapActivity : Activity() {
    private var request: JoinRequest? = null
    private var awaitingInput = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState != null) return finish()
        val ask = intent.getStringExtra(Join.EXTRA_ASK) ?: return finish()
        val asked = Join.request(ask)
        if (asked == null) {
            Toast.makeText(this, "That request has already ended", Toast.LENGTH_LONG).show()
            return finish()
        }
        request = asked
        Join.accepted(this, ask)
        OneTap.watch()
        val prefs = getSharedPreferences(KEY_SHARED_PREFERENCES, MODE_PRIVATE)
        if (InputService.isOpen || prefs.getBoolean(KEY_ONE_TAP_ASKED_INPUT, false)) return share()
        prefs.edit().putBoolean(KEY_ONE_TAP_ASKED_INPUT, true).apply()
        AlertDialog.Builder(this)
            .setTitle("Let ${asked.agentTitle} tap and type too?")
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
        val asked = request ?: return finish()
        if (Join.request(asked.ask) == null) return finish()
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
        val asked = request ?: return finish()
        when {
            Join.request(asked.ask) == null -> Log.i(TAG, "${asked.ask} ended while Start now showed: nothing is shared")
            resultCode == RESULT_OK && data != null -> {
                val service = Intent(this, MainService::class.java).apply {
                    action = ACT_INIT_MEDIA_PROJECTION_AND_SERVICE
                    putExtra(EXT_MEDIA_PROJECTION_RES_INTENT, data)
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) startForegroundService(service) else startService(service)
                OneTap.ready(applicationContext, asked)
            }
            else -> Join.said(this, asked.ask, "deny", "the person cancelled Android's Start now")
        }
        finish()
    }

    companion object {
        private const val TAG = "RigOneTap"
        private const val REQ_ONE_TAP_PROJECTION = 401
        private const val KEY_ONE_TAP_ASKED_INPUT = "KEY_ONE_TAP_ASKED_INPUT"
    }
}

/** The phone's side of one tap once sharing is under way: it waits until the phone listens, files the Accept, says ready. */
object OneTap {
    private const val TAG = "RigOneTap"
    private const val LISTEN_WAIT_MS = 15_000L
    private val accepted: MutableSet<String> = Collections.synchronizedSet(HashSet())
    @Volatile private var watching = false

    /** An Accept filed here goes as soon as its request ends on the phone, however it ended. */
    fun watch() {
        if (watching) return
        watching = true
        Join.onEnded { ask -> if (accepted.remove(ask)) FFI.rigOneTapWithdraw(ask) }
    }

    fun ready(ctx: Context, asked: JoinRequest) {
        Thread({
            val until = SystemClock.elapsedRealtime() + LISTEN_WAIT_MS
            while (!MainService.isReady && SystemClock.elapsedRealtime() < until) SystemClock.sleep(100)
            if (MainService.isReady) FFI.startService()
            while (!FFI.rigListening() && SystemClock.elapsedRealtime() < until) SystemClock.sleep(100)
            when {
                Join.request(asked.ask) == null -> Log.i(TAG, "${asked.ask} ended before the phone was ready")
                !MainService.isReady -> Join.said(ctx, asked.ask, "failed", "screen sharing did not start within ${LISTEN_WAIT_MS / 1000} s")
                !FFI.rigListening() -> Join.said(ctx, asked.ask, "failed", "RigDesk is not listening for Rig: turn on direct IP access in its settings")
                else -> {
                    val refused = FFI.rigOneTapAccept(asked.ask, asked.agentId, asked.agentTitle, asked.graph, asked.operator, asked.leftMs(), asked.rigAccepted)
                    if (refused.isEmpty()) {
                        accepted.add(asked.ask)
                        Join.said(ctx, asked.ask, "ready")
                    } else {
                        Join.said(ctx, asked.ask, "failed", refused)
                    }
                }
            }
        }, "rig-one-tap").start()
    }
}
