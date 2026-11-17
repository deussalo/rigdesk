package dev.rig.assistant

import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.service.voice.VoiceInteractionSession
import android.speech.RecognitionListener
import android.speech.RecognitionService
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.ForegroundColorSpan
import android.util.Base64
import android.view.Gravity
import android.view.View
import android.view.WindowInsets
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * The sheet the assistant button opens. It records until the person taps Send or Cancel: when the phone's recogniser ends on
 * silence or a timeout, it starts again and keeps what was heard, so pauses never end the recording. Nothing reaches Rig until
 * Send; Cancel, Back or leaving the sheet drops what was heard.
 */
class AssistSession(context: Context) : VoiceInteractionSession(context) {
    private val main = Handler(Looper.getMainLooper())
    private val hideLater = Runnable { hide() }
    private val deliverLate = Runnable { deliver() }
    private lateinit var heard: TextView
    private lateinit var status: TextView
    private lateinit var typed: EditText
    private var recognizer: SpeechRecognizer? = null
    /** What the recogniser has settled, segment by segment, and what it is hearing now. */
    private val said = StringBuilder()
    private var hearing = ""
    private var recording = false
    /** Send was tapped: waiting for the recogniser's last words before sending. */
    private var finishing = false
    private var failures = 0
    /** Sent once per showing: a second Send before the sheet hides sends nothing. */
    private var sent = false
    /** When the recogniser last said it was ready, and how many segments in a row ended at once with nothing heard. */
    private var readyAt = 0L
    private var quick = 0
    /** A segmented session settled some words before it ended. */
    private var heardInSession = false
    @Volatile private var image: JSONObject? = null
    private var shot = CountDownLatch(1)

    private fun dp(value: Int) = Math.round(value * context.resources.displayMetrics.density)

    override fun onCreateContentView(): View {
        val sheet = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(16), dp(20), dp(20))
            background = GradientDrawable().apply {
                setColor(0xF21C1C1C.toInt())
                cornerRadii = floatArrayOf(dp(16).toFloat(), dp(16).toFloat(), dp(16).toFloat(), dp(16).toFloat(), 0f, 0f, 0f, 0f)
            }
        }
        status = TextView(context).apply { setTextColor(0xFF9E9E8E.toInt()); textSize = 13f }
        heard = TextView(context).apply { setTextColor(Color.WHITE); textSize = 20f; setPadding(0, dp(8), 0, dp(12)) }
        typed = EditText(context).apply {
            hint = "Or type it"
            setTextColor(Color.WHITE)
            setHintTextColor(0xFF77776A.toInt())
        }
        val cancel = Button(context).apply { text = "Cancel"; setOnClickListener { cancel() } }
        val send = Button(context).apply { text = "Send"; setOnClickListener { send() } }
        val row = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(cancel, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            addView(send, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        }
        sheet.addView(status)
        sheet.addView(heard)
        sheet.addView(typed)
        sheet.addView(row)
        val frame = LinearLayout(context).apply { gravity = Gravity.BOTTOM }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // The session's window draws behind the navigation bar and the keyboard, which would hide the buttons.
            frame.setOnApplyWindowInsetsListener { _, insets ->
                val under = insets.getInsets(WindowInsets.Type.navigationBars() or WindowInsets.Type.ime())
                sheet.setPadding(dp(20), dp(16), dp(20), dp(20) + under.bottom)
                insets
            }
        }
        frame.addView(sheet, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT))
        return frame
    }

    override fun onShow(args: Bundle?, showFlags: Int) {
        super.onShow(args, showFlags)
        main.removeCallbacks(hideLater)
        main.removeCallbacks(deliverLate)
        said.setLength(0)
        hearing = ""
        finishing = false
        failures = 0
        sent = false
        quick = 0
        heardInSession = false
        image = null
        shot = CountDownLatch(1)
        heard.text = ""
        typed.setText("")
        LinkService.start(context)
        recording = true
        listen()
    }

    override fun onHandleScreenshot(screenshot: Bitmap?) {
        val arrived = shot
        try {
            if (screenshot != null) image = jpeg(screenshot)
        } finally {
            arrived.countDown()
        }
    }

    override fun onHide() {
        stopRecording()
        super.onHide()
    }

    override fun onDestroy() {
        main.removeCallbacks(hideLater)
        main.removeCallbacks(deliverLate)
        stopRecording()
        super.onDestroy()
    }

    private fun stopRecording() {
        recording = false
        finishing = false
        main.removeCallbacks(deliverLate)
        recognizer?.destroy()
        recognizer = null
    }

    private fun target() = Link.resident.ifEmpty { "Rig" }

    private fun showListening() {
        status.text = if (Link.connected()) "Listening for ${target()}. Tap Send when you're done."
            else "Listening. Not connected to Rig (${Link.state}): Send keeps it until the phone reconnects."
    }

    private fun listen() {
        if (!granted(context, Manifest.permission.RECORD_AUDIO)) {
            recording = false
            status.text = "Allow the microphone, and the sheet opens again listening. Or type."
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                val asked = mutableListOf(Manifest.permission.RECORD_AUDIO)
                if (Build.VERSION.SDK_INT >= 33) asked.add(Manifest.permission.POST_NOTIFICATIONS)
                startAssistantActivity(Switch.runtime(context, *asked.toTypedArray()).putExtra(GrantActivity.REOPEN, true))
            }
            return
        }
        val r = recognizer ?: recognizer(context)?.also { made ->
            recognizer = made
            made.setRecognitionListener(listener)
        }
        if (r == null) {
            status.text = "This phone offers no speech recognition: type instead"
            recording = false
            return
        }
        showListening()
        // The recogniser reads these lengths with getInt: a Long is dropped with a Bundle warning and counts as unset.
        val asked = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
            .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            .putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
            .putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, SILENCE_MS)
            .putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, SILENCE_MS)
        // A segmented session keeps the microphone open across pauses and hands each settled stretch over as it ends,
        // so no words fall into the gap of a restart; it ends only after SILENCE_MS of silence, and is then restarted.
        if (Build.VERSION.SDK_INT >= 33) asked.putExtra(RecognizerIntent.EXTRA_SEGMENTED_SESSION, RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS)
        r.startListening(asked)
    }

    /** Settles what is being heard into what was said. */
    private fun settle(words: String) {
        val clean = words.trim()
        if (clean.isNotEmpty()) {
            if (said.isNotEmpty()) said.append(' ')
            said.append(clean)
        }
        hearing = ""
        render()
    }

    /** What was said shows white and never changes; the recogniser's open guess follows it dimmed, since it may still be revised. */
    private fun render() {
        val shown = SpannableStringBuilder(said)
        if (hearing.isNotEmpty()) {
            if (shown.isNotEmpty()) shown.append(' ')
            shown.append(hearing, ForegroundColorSpan(OPEN_COLOR), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        heard.text = shown
    }

    /** The recogniser ended a segment: send if Send is waiting for it, else listen again. A recogniser that ends at once
     *  every time (some ignore the silence extras) is restarted with a growing pause, up to 3 s, rather than spun. */
    private fun segmentEnded(heardSome: Boolean) {
        if (finishing) { deliver(); return }
        if (!recording) return
        quick = if (!heardSome && System.currentTimeMillis() - readyAt < QUICK_MS) quick + 1 else 0
        main.postDelayed({ if (recording) listen() }, minOf(3_000L, 300L * quick))
    }

    private val listener = object : RecognitionListener {
        override fun onReadyForSpeech(params: Bundle?) { failures = 0; readyAt = System.currentTimeMillis() }
        override fun onBeginningOfSpeech() {}
        override fun onRmsChanged(rmsdB: Float) {}
        override fun onBufferReceived(buffer: ByteArray?) {}
        override fun onEndOfSpeech() {}
        override fun onEvent(eventType: Int, params: Bundle?) {}

        override fun onPartialResults(partial: Bundle?) {
            val words = partial?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull() ?: return
            if (words.isNotBlank()) {
                hearing = words.trim()
                render()
            }
        }

        override fun onSegmentResults(segmentResults: Bundle) {
            val words = segmentResults.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull() ?: hearing
            if (words.isNotBlank()) heardInSession = true
            settle(words)
        }

        override fun onEndOfSegmentedSession() {
            settle(hearing)
            val heardSome = heardInSession
            heardInSession = false
            segmentEnded(heardSome)
        }

        override fun onResults(results: Bundle?) {
            failures = 0
            val words = results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull() ?: hearing
            settle(words)
            segmentEnded(words.isNotBlank())
        }

        override fun onError(error: Int) {
            val heardSome = hearing.isNotBlank()
            settle(hearing)  // keep what was heard before the error
            when (error) {
                SpeechRecognizer.ERROR_NO_MATCH, SpeechRecognizer.ERROR_SPEECH_TIMEOUT -> segmentEnded(heardSome)
                else -> {
                    // A busy or broken recogniser is let go and made again; the recording goes on unless it keeps failing.
                    recognizer?.destroy()
                    recognizer = null
                    if (finishing) { deliver(); return }
                    if (++failures > MAX_FAILURES) {
                        recording = false
                        status.text = "Speech recognition keeps failing ($error): type, or Send what was heard"
                        return
                    }
                    main.postDelayed({ if (recording) listen() }, 300L * failures)
                }
            }
        }
    }

    private fun cancel() {
        stopRecording()
        hide()
    }

    private fun send() {
        if (finishing || sent) return
        if (recording && recognizer != null) {
            finishing = true
            status.text = "Sending…"
            recognizer?.stopListening()
            main.postDelayed(deliverLate, LAST_WORDS_MS)
        } else deliver()
    }

    /** Sends everything heard and typed, once. */
    private fun deliver() {
        main.removeCallbacks(deliverLate)
        if (sent) return
        settle(hearing)
        heardInSession = false
        stopRecording()
        val text = listOf(said.toString().trim(), typed.text.toString().trim()).filter { it.isNotEmpty() }.joinToString("\n")
        if (text.isEmpty()) {
            status.text = "Nothing heard or typed to send"
            return
        }
        sent = true
        val arrived = shot
        BACK.execute { Link.event("assist", text, imageAfter(arrived)) }
        status.text = if (Link.connected()) "Sent to ${target()}" else "Kept: sent to ${target()} once the phone reconnects"
        main.postDelayed(hideLater, 900)
    }

    /** Android hands the screenshot over separately, a little after the press; a quick press waits a moment for it. */
    private fun imageAfter(arrived: CountDownLatch): JSONObject? {
        try { arrived.await(1_500, TimeUnit.MILLISECONDS) } catch (e: InterruptedException) { Thread.currentThread().interrupt() }
        return image
    }

    companion object {
        private val BACK = Executors.newSingleThreadExecutor { Thread(it, "rig-assist") }
        private const val MAX_FAILURES = 5
        /** A segment ending this soon after ready, with nothing heard, ended at once. */
        private const val QUICK_MS = 500L
        /** How long Send waits for the recogniser's final words after stopping it. */
        private const val LAST_WORDS_MS = 2_000L
        /** Silence that ends a recogniser session; the sheet then starts another, so this only bounds an idle microphone. */
        private const val SILENCE_MS = 30_000
        private const val OPEN_COLOR = 0xFF9E9E8E.toInt()

        private fun jpeg(screenshot: Bitmap): JSONObject {
            val scale = minOf(1f, 1280f / maxOf(screenshot.width, screenshot.height))
            val small = Bitmap.createScaledBitmap(screenshot, Math.round(screenshot.width * scale), Math.round(screenshot.height * scale), true)
            val bytes = ByteArrayOutputStream()
            small.compress(Bitmap.CompressFormat.JPEG, 80, bytes)
            return JSONObject().put("mime", "image/jpeg").put("data", Base64.encodeToString(bytes.toByteArray(), Base64.NO_WRAP))
        }

        /** Being the assistant makes this app the phone's default recogniser, which recognises nothing, so another is named outright. */
        private fun recognizer(ctx: Context): SpeechRecognizer? {
            if (Build.VERSION.SDK_INT >= 31 && SpeechRecognizer.isOnDeviceRecognitionAvailable(ctx)) return SpeechRecognizer.createOnDeviceSpeechRecognizer(ctx)
            var chosen: ComponentName? = null
            for (each in ctx.packageManager.queryIntentServices(Intent(RecognitionService.SERVICE_INTERFACE), 0)) {
                if (each.serviceInfo.packageName == ctx.packageName) continue
                val name = ComponentName(each.serviceInfo.packageName, each.serviceInfo.name)
                if (chosen == null || each.serviceInfo.packageName.startsWith("com.google")) chosen = name
            }
            return chosen?.let { SpeechRecognizer.createSpeechRecognizer(ctx, it) }
        }
    }
}
