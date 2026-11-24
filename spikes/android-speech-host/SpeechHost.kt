package dev.frontcap.speech

import android.content.Context
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import android.util.Log
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import org.json.JSONObject
import java.io.File
import java.util.Locale

// Expose Android TextToSpeech to a Flutter engine.
class SpeechHost(context: Context, messenger: BinaryMessenger, private val mutedOutput: Boolean = false) {
    private val main = Handler(Looper.getMainLooper())
    private val channel = MethodChannel(messenger, "dev.frontcap/speech")
    private val logFile = File(context.filesDir, "native-speech.jsonl")
    private val waiting = ArrayDeque<Pair<MethodCall, MethodChannel.Result>>()
    private var state = "initializing"
    private var active: String? = null
    private var nextID = 0L
    private var records = 0
    private val timeout = Runnable { finishInitialization(false, "initialization_timeout") }
    private val engine = TextToSpeech(context.applicationContext) { status ->
        main.post { finishInitialization(status == TextToSpeech.SUCCESS, "engine_initialization") }
    }

    init {
        engine.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
            override fun onStart(id: String) { main.post { event("playing", id) } }
            override fun onDone(id: String) { main.post { terminal("done", id) } }
            @Deprecated("Engine compatibility callback")
            override fun onError(id: String) { onError(id, TextToSpeech.ERROR) }
            override fun onError(id: String, code: Int) {
                main.post { terminal("error", id, mapOf("native_code" to code)) }
            }
            override fun onStop(id: String, interrupted: Boolean) {
                main.post { terminal("stopped", id, mapOf("interrupted" to interrupted)) }
            }
        })
        channel.setMethodCallHandler { call, result ->
            when (state) {
                "initializing" -> if (waiting.size < 8) waiting.addLast(call to result)
                    else result.error("speech_capacity", "Initialization queue full", null)
                "ready" -> handle(call, result)
                else -> result.error("speech_unavailable", state, null)
            }
        }
        main.postDelayed(timeout, 10_000)
    }

    private fun finishInitialization(success: Boolean, reason: String) {
        if (state != "initializing") return
        main.removeCallbacks(timeout)
        state = if (success) "ready" else "failed"
        event("availability", extra = mapOf("state" to state, "reason" to reason,
            "engine" to (engine.defaultEngine ?: ""), "muted_output" to mutedOutput,
            "offline_advertised_voices" to (engine.voices?.count { !it.isNetworkConnectionRequired } ?: 0)))
        while (waiting.isNotEmpty()) {
            val (call, result) = waiting.removeFirst()
            if (success) handle(call, result) else result.error("speech_unavailable", reason, null)
        }
    }

    private fun event(kind: String, id: String? = null, extra: Map<String, Any> = emptyMap()) {
        if (state == "closed") return
        val current = id != null && id == active
        val row = mutableMapOf<String, Any>("kind" to kind, "uptime_ms" to SystemClock.elapsedRealtime(),
            "current" to current, "speaking" to engine.isSpeaking)
        if (id != null) row["utterance_id"] = id
        row.putAll(extra)
        if (records < 512) {
            row["sequence"] = ++records
            if (records == 512) row["limit_reached"] = true
            try { logFile.appendText(JSONObject(row).toString() + "\n") }
            catch (error: Exception) { Log.w("LOCAL_SPEECH", "Record write failed", error) }
        }
        if (current && kind in setOf("playing", "done", "stopped", "error"))
            channel.invokeMethod("event", row)
    }

    private fun terminal(kind: String, id: String, extra: Map<String, Any> = emptyMap()) {
        event(kind, id, extra)
        if (active == id) active = null
    }

    private fun handle(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            "speak" -> {
                val text = call.argument<String>("text")
                val language = call.argument<String>("language")
                if (text.isNullOrEmpty() || text.length > TextToSpeech.getMaxSpeechInputLength() ||
                    language.isNullOrEmpty()) {
                    result.error("invalid_speech", "Text and language are required", null)
                    return
                }
                val locale = Locale.forLanguageTag(language)
                val candidates = engine.voices.orEmpty().filter { it.locale.language == locale.language }
                val voice = candidates.filter { !it.isNetworkConnectionRequired &&
                    !it.features.contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED) }
                    .sortedWith(compareByDescending<android.speech.tts.Voice> { it.locale == locale }
                        .thenByDescending { it.quality }.thenBy { it.name }).firstOrNull()
                val voiceStatus = if (voice == null) TextToSpeech.ERROR else engine.setVoice(voice)
                if (voiceStatus != TextToSpeech.SUCCESS || voice == null) {
                    event("voice_unavailable", extra = mapOf("language" to language,
                        "reason" to if (voice == null) "no_installed_offline_voice" else "voice_selection_failed",
                        "native_code" to voiceStatus, "advertised_candidates" to candidates.size,
                        "offline_candidates" to candidates.count { !it.isNetworkConnectionRequired },
                        "uninstalled_candidates" to candidates.count {
                            it.features.contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED) }))
                    result.error("offline_voice_unavailable", language, null)
                    return
                }
                val id = (++nextID).toString()
                active = id
                val params = Bundle().apply {
                    putFloat(TextToSpeech.Engine.KEY_PARAM_VOLUME, if (mutedOutput) 0f else 1f)
                }
                event("speak_requested", id, mapOf("characters" to text.length,
                    "language" to voice.locale.toLanguageTag(), "voice" to voice.name,
                    "muted_output" to mutedOutput))
                val status = engine.speak(text, TextToSpeech.QUEUE_FLUSH, params, id)
                if (status != TextToSpeech.SUCCESS)
                    terminal("error", id, mapOf("native_code" to status, "reason" to "submission_failed"))
                result.success(mapOf("accepted" to (status == TextToSpeech.SUCCESS), "utterance_id" to id))
            }
            "stop" -> {
                event("stop_requested", active)
                result.success(mapOf("accepted" to (engine.stop() == TextToSpeech.SUCCESS)))
            }
            "pause", "resume" -> result.error("unsupported_speech_operation",
                "Android TextToSpeech has no native pause/resume operation", null)
            else -> result.notImplemented()
        }
    }

    fun close() {
        if (state == "closed") return
        event("disposed", active)
        state = "closed"
        channel.setMethodCallHandler(null)
        main.removeCallbacks(timeout)
        while (waiting.isNotEmpty()) waiting.removeFirst().second.error("disposed", null, null)
        active = null
        engine.stop()
        engine.shutdown()
    }
}
