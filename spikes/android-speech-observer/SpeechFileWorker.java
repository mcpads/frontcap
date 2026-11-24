package dev.frontcap.speech;

import android.content.Context;
import android.media.AudioFormat;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.speech.tts.TextToSpeech;
import android.speech.tts.UtteranceProgressListener;
import android.speech.tts.Voice;
import org.json.JSONObject;
import java.io.File;
import java.io.FileOutputStream;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;

/** Platform-injected preparation experiment; never a consumer dependency. */
public final class SpeechFileWorker extends UtteranceProgressListener {
    private static SpeechFileWorker active;
    private final Handler main = new Handler(Looper.getMainLooper());
    private final File audio, receipt, latestReceipt;
    private final String identity, engineName;
    private final Voice voice;
    private final Bundle parameters;
    private SpeechControl control;
    static native boolean cursorPrepare(long frames, int rate);
    static native boolean cursorReset();
    static native long[] cursorStep(long delta);
    static native boolean cursorOwner(TextToSpeech source, UtteranceProgressListener listener);
    public static boolean stop() {
        return active != null && active.control != null && active.control.cancel();
    }
    private String text;
    private TextToSpeech engine;
    private final JSONObject report = new JSONObject();
    private boolean closed;
    private long bytes, chunks, firstChunk, lastChunk;
    private int sampleRate, format, channels;
    private boolean formatSeen;
    private final Runnable deadline = () -> finish("preparation_timeout");

    public static boolean prepare(Context context, TextToSpeech source, String text,
                               Bundle defaults, Bundle request, String engineName, String identity,
                               UtteranceProgressListener listener, String consumerID) {
        if (Looper.myLooper() != Looper.getMainLooper()) return false;
        Voice voice = source.getVoice();
        Bundle parameters = new Bundle(defaults);
        if (request != null) parameters.putAll(request);
        SpeechControl control = active == null ? null : active.control;
        if (active != null && (!active.closed || control == null || listener == null ||
            !control.beginNext(source, listener, consumerID, identity))) return false;
        active = new SpeechFileWorker(context.getApplicationContext(), text, voice,
            parameters, engineName, identity);
        if (listener != null) active.control = control != null ? control :
            new SpeechControl(context.getApplicationContext(), source, listener, consumerID, identity);
        active.begin(context.getApplicationContext());
        return true;
    }

    private SpeechFileWorker(Context context, String text, Voice voice, Bundle parameters,
                             String engineName, String identity) {
        this.text = text; this.voice = voice; this.parameters = parameters;
        this.engineName = engineName; this.identity = identity;
        audio = new File(context.getFilesDir(), "speech-worker.wav");
        receipt = new File(context.getFilesDir(), "speech-worker-" + identity + ".json");
        latestReceipt = new File(context.getFilesDir(), "speech-worker.json");
    }

    private void put(String key, Object value) {
        try { report.put(key, value); }
        catch (org.json.JSONException error) { throw new IllegalStateException(error); }
    }

    private void save() {
        publish(receipt);
        if (active == this) publish(latestReceipt);
    }

    private void publish(File destination) {
        File temporary = new File(destination.getPath() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(temporary)) {
            out.write((report.toString() + "\n").getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
        } catch (Exception error) { throw new IllegalStateException("Worker receipt write failed", error); }
        if (!temporary.renameTo(destination)) throw new IllegalStateException("Worker receipt publish failed");
    }

    private void begin(Context context) {
        put("identity", identity);
        put("status", "preparing");
        put("created_ns", SystemClock.elapsedRealtimeNanos());
        put("engine", engineName);
        put("characters", text.length());
        put("voice", voice == null ? "" : voice.getName());
        save();
        if (voice == null || voice.isNetworkConnectionRequired() ||
            voice.getFeatures().contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED) ||
            engineName == null || engineName.isEmpty()) {
            finish("offline_voice_unavailable"); return;
        }
        main.postDelayed(deadline, 20_000);
        engine = new TextToSpeech(context, status -> main.post(() -> initialized(status)), engineName);
    }

    private void initialized(int status) {
        if (closed) return;
        if (status != TextToSpeech.SUCCESS || engine.setVoice(voice) != TextToSpeech.SUCCESS) {
            finish("worker_initialization_failed"); return;
        }
        engine.setOnUtteranceProgressListener(this);
        put("submitted_ns", SystemClock.elapsedRealtimeNanos());
        int result = engine.synthesizeToFile(text, parameters, audio, identity);
        put("submission_status", result);
        save();
        if (result != TextToSpeech.SUCCESS) finish("file_submission_failed");
    }

    private void callback(String id, Runnable action) {
        main.post(() -> {
            if (!identity.equals(id)) { finish("worker_identity_mismatch"); return; }
            if (closed) {
                if (control != null) control.fail();
                put("status", "failed");
                put("error", "late_worker_callback");
                save();
                return;
            }
            action.run();
        });
    }

    @Override public void onStart(String id) {
        callback(id, () -> { put("started_ns", SystemClock.elapsedRealtimeNanos()); save(); });
    }
    @Override public void onBeginSynthesis(String id, int rate, int encoding, int count) {
        callback(id, () -> {
            if (formatSeen || rate <= 0 || count <= 0 || encoding != AudioFormat.ENCODING_PCM_16BIT) {
                finish("unsupported_or_repeated_format"); return;
            }
            formatSeen = true; sampleRate = rate; format = encoding; channels = count;
            put("format_ns", SystemClock.elapsedRealtimeNanos());
        });
    }
    @Override public void onAudioAvailable(String id, byte[] data) {
        int length = data.length; // No audio copy or audio bytes in receipts.
        callback(id, () -> {
            if (!formatSeen || length % (2L * channels) != 0) {
                finish("unaligned_or_unprepared_audio"); return;
            }
            long now = SystemClock.elapsedRealtimeNanos();
            if (chunks == 0) firstChunk = now;
            lastChunk = now; chunks++; bytes += length;
            if (bytes > 64L * 1024 * 1024) finish("audio_capacity");
        });
    }
    @Override public void onDone(String id) { callback(id, () -> finish(null)); }
    @Override public void onError(String id) { onError(id, TextToSpeech.ERROR); }
    @Override public void onError(String id, int code) { callback(id, () -> finish("engine_error_" + code)); }
    @Override public void onStop(String id, boolean interrupted) { callback(id, () -> finish("worker_stopped")); }

    private static long uint32(RandomAccessFile input) throws Exception {
        return Integer.toUnsignedLong(Integer.reverseBytes(input.readInt()));
    }
    private static int uint16(RandomAccessFile input) throws Exception {
        return Short.toUnsignedInt(Short.reverseBytes(input.readShort()));
    }
    private static String fourCC(RandomAccessFile input) throws Exception {
        byte[] value = new byte[4]; input.readFully(value);
        return new String(value, StandardCharsets.US_ASCII);
    }

    private void checkWave() throws Exception {
        try (RandomAccessFile input = new RandomAccessFile(audio, "r")) {
            long length = input.length();
            if (length < 44 || length > 64L * 1024 * 1024 + 4096 ||
                !fourCC(input).equals("RIFF") || uint32(input) + 8 != length ||
                !fourCC(input).equals("WAVE")) throw new IllegalStateException("Invalid RIFF");
            boolean haveFormat = false, haveData = false;
            while (input.getFilePointer() + 8 <= length) {
                String name = fourCC(input);
                long size = uint32(input), start = input.getFilePointer();
                if (size > length - start) throw new IllegalStateException("Truncated chunk");
                if (name.equals("fmt ")) {
                    if (haveFormat || size < 16 || uint16(input) != 1 ||
                        uint16(input) != channels || uint32(input) != sampleRate ||
                        uint32(input) != (long) sampleRate * channels * 2 ||
                        uint16(input) != channels * 2 || uint16(input) != 16)
                        throw new IllegalStateException("PCM format mismatch");
                    haveFormat = true;
                } else if (name.equals("data")) {
                    if (haveData || size != bytes) throw new IllegalStateException("PCM byte count mismatch");
                    haveData = true;
                }
                long end = start + size + (size & 1);
                if (end > length) throw new IllegalStateException("Truncated padding");
                input.seek(end);
            }
            if (input.getFilePointer() != length || !haveFormat || !haveData ||
                !formatSeen || bytes == 0 || chunks == 0)
                throw new IllegalStateException("Incomplete PCM");
            put("wave_bytes", length);
            put("frames", bytes / (channels * 2L));
        }
    }

    private void finish(String error) {
        if (closed) return;
        closed = true;
        main.removeCallbacks(deadline);
        put("completed_ns", SystemClock.elapsedRealtimeNanos());
        if (error == null) {
            try { checkWave(); }
            catch (Exception invalid) { error = invalid.toString(); }
        }
        boolean deleted = !audio.exists() || audio.delete();
        if (!deleted && error == null) error = "audio_cleanup_failed";
        put("status", error == null ? "prepared" : "failed");
        if (error != null) put("error", error);
        put("sample_rate", sampleRate); put("audio_format", format); put("channels", channels);
        put("pcm_bytes", bytes); put("chunks", chunks);
        put("first_chunk_ns", firstChunk); put("last_chunk_ns", lastChunk);
        put("audio_deleted", deleted);
        text = null;
        if (engine != null) engine.shutdown();
        save();
        if (control != null) {
            if (error == null) control.prepared(bytes / (channels * 2L), sampleRate);
            else control.fail();
        }
        // Keep this worker until an explicitly admitted following request replaces it.
    }
}
