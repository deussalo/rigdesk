package dev.rig.assistant

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.RemoteException
import android.service.voice.VoiceInteractionService
import android.service.voice.VoiceInteractionSession
import android.service.voice.VoiceInteractionSessionService
import android.speech.RecognitionService
import android.speech.SpeechRecognizer
import android.util.Log

/** Makes Rig Assistant a choice for the phone's default assistant; Android binds it from boot while it is chosen, which starts the link. */
class AssistService : VoiceInteractionService() {
    override fun onReady() {
        super.onReady()
        LinkService.start(this)
    }
}

class AssistSessionService : VoiceInteractionSessionService() {
    override fun onNewSession(args: Bundle?): VoiceInteractionSession = AssistSession(this)
}

/** Android requires an assistant to name a recognition service. This one recognises nothing: AssistSession uses the phone's own. */
class Recognition : RecognitionService() {
    override fun onStartListening(intent: Intent?, listener: Callback) {
        try { listener.error(SpeechRecognizer.ERROR_CLIENT) } catch (_: RemoteException) {}
    }
    override fun onCancel(listener: Callback?) {}
    override fun onStopListening(listener: Callback?) {}
}

/** Keeps the process, and so the link to Rig, alive while the screen is off. */
class LinkService : Service() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Link.start(this)
        val shown = builder().setSmallIcon(android.R.drawable.stat_notify_sync).setContentTitle("Rig Assistant")
            .setContentText("Connected to Rig while the phone is on").setOngoing(true)
            .setContentIntent(PendingIntent.getActivity(this, 0, Intent(this, SetupActivity::class.java), PendingIntent.FLAG_IMMUTABLE)).build()
        try {
            if (Build.VERSION.SDK_INT >= 34) startForeground(ID, shown, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            else startForeground(ID, shown)
        } catch (e: Exception) {
            Log.w(TAG, "Android refused the foreground service: $e")
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    @Suppress("DEPRECATION")
    private fun builder(): Notification.Builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
            .createNotificationChannel(NotificationChannel(CHANNEL, "Connection to Rig", NotificationManager.IMPORTANCE_MIN))
        Notification.Builder(this, CHANNEL)
    } else Notification.Builder(this)

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "RigAssistant"
        private const val CHANNEL = "rig-assistant-link"
        private const val ID = 7310

        /** The link runs either way; the service only keeps the process up with the screen off. Android may refuse to start it from the
         *  background (the app not exempt from battery optimisation), which is noted, not a crash. */
        fun start(ctx: Context) {
            Link.start(ctx)
            try {
                val intent = Intent(ctx, LinkService::class.java)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) ctx.startForegroundService(intent) else ctx.startService(intent)
            } catch (e: Exception) {
                Log.w(TAG, "Android refused the foreground service from the background: $e")
            }
        }
    }
}
