package dev.frontcap.speech;

import android.content.Context;
import android.os.Handler;
import android.os.Looper;
import android.speech.tts.TextToSpeech;
import android.speech.tts.UtteranceProgressListener;
import java.io.File;
import java.io.FileOutputStream;
import java.lang.ref.WeakReference;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.LinkedHashMap;
import org.json.JSONObject;

/** Serial local control experiment; all cursor and callback work runs on the main loop. */
final class SpeechControl {
    private final Handler main = new Handler(Looper.getMainLooper());
    private final WeakReference<TextToSpeech> source;
    private final WeakReference<UtteranceProgressListener> listener;
    private String consumerID, token;
    private int utterance = 1;
    private final File request, response, state;
    private final LinkedHashMap<Long, String[]> history = new LinkedHashMap<>();
    private boolean failed;
    private boolean inCallback;
    private int starts, completions, stops;
    private long stopPhase = -1;
    private int discardedPreparations;
    private String lastRequest = "";

    SpeechControl(Context context, TextToSpeech source, UtteranceProgressListener listener,
                  String consumerID, String token) {
        this.source = new WeakReference<>(source);
        this.listener = new WeakReference<>(listener);
        this.consumerID = consumerID; this.token = token;
        request = new File(context.getFilesDir(), "speech-request.json");
        response = new File(context.getFilesDir(), "speech-response.json");
        state = new File(context.getFilesDir(), "speech-control.json");
        publish(state, snapshot().toString());
        main.post(this::poll);
    }

    void prepared(long frames, int rate) {
        if (SpeechFileWorker.cursorStep(-1)[0] == 3) {
            discardedPreparations++;
            publish(state, snapshot().toString());
            return; // A cancelled request cannot be revived by late preparation.
        }
        if (failed || !SpeechFileWorker.cursorPrepare(frames, rate)) fail();
        publish(state, snapshot().toString());
    }

    boolean beginNext(TextToSpeech nextSource, UtteranceProgressListener nextListener,
                      String nextID, String nextToken) {
        if (failed || inCallback || source.get() != nextSource || listener.get() != nextListener || !ownerValid() ||
            !SpeechFileWorker.cursorReset()) return false;
        consumerID = nextID; token = nextToken; utterance++;
        starts = completions = stops = discardedPreparations = 0;
        stopPhase = -1;
        publish(state, snapshot().toString());
        return true;
    }

    void fail() {
        failed = true;
        SpeechFileWorker.cursorStep(-2);
        publish(state, snapshot().toString());
    }

    private boolean ownerValid() {
        return SpeechFileWorker.cursorOwner(source.get(), listener.get());
    }

    private void deliver(Runnable callback) {
        inCallback = true;
        try { callback.run(); }
        finally { inCallback = false; }
    }

    boolean cancel() {
        if (Looper.myLooper() != Looper.getMainLooper()) return false;
        if (failed || !ownerValid()) { fail(); return false; }
        long phase = SpeechFileWorker.cursorStep(-1)[0];
        if (phase == 2 || phase == 3) return true;
        if (phase != 0 && phase != 1) { fail(); return false; }
        UtteranceProgressListener target = listener.get();
        if (target == null) { fail(); return false; }
        if (SpeechFileWorker.cursorStep(-3)[0] != 3) { fail(); return false; }
        stopPhase = phase;
        stops++;
        deliver(() -> target.onStop(consumerID, starts > 0));
        if (!ownerValid()) { fail(); return false; }
        publish(state, snapshot().toString());
        return true;
    }

    private JSONObject snapshot() {
        long[] value = SpeechFileWorker.cursorStep(-1);
        JSONObject out = new JSONObject();
        put(out, "token", token);
        put(out, "utterance", utterance);
        put(out, "phase", failed ? 4 : value[0]);
        put(out, "frames", value[1]); put(out, "sample_rate", value[2]);
        put(out, "frame", value[3]); put(out, "fraction", value[4]);
        put(out, "remaining_ns", value[6]);
        put(out, "start_count", starts); put(out, "done_count", completions);
        put(out, "stop_count", stops);
        put(out, "stop_phase", stopPhase);
        put(out, "discarded_preparations", discardedPreparations);
        return out;
    }

    private String execute(String operation, long delta) {
        if (failed || !ownerValid()) { fail(); return "failed"; }
        if (operation.equals("read")) return "read";
        if (!operation.equals("advance") || delta < 0) return "invalid_operation";
        long[] before = SpeechFileWorker.cursorStep(-1);
        if (before[0] == 0) return "preparing";
        if (before[0] == 4) { fail(); return "failed"; }
        if (before[0] != 1) return "inactive";
        if (delta > before[6]) return "crossing_rejected";
        if (delta > 0 && starts == 0) {
            UtteranceProgressListener target = listener.get();
            if (target == null) { fail(); return "failed"; }
            starts++;
            deliver(() -> target.onStart(consumerID));
            if (!ownerValid() || SpeechFileWorker.cursorStep(-1)[0] != 1) {
                fail(); return "failed";
            }
        }
        long[] after = SpeechFileWorker.cursorStep(delta);
        if (after[5] == 2) {
            UtteranceProgressListener target = listener.get();
            if (target == null || !ownerValid()) { fail(); return "failed"; }
            completions++;
            deliver(() -> target.onDone(consumerID));
            if (!ownerValid()) { fail(); return "failed"; }
            return "completed";
        }
        return after[5] == 1 ? "advanced" : "failed";
    }

    private void poll() {
        try {
            if (request.isFile()) {
                if (request.length() > 4096) throw new IllegalStateException("Request capacity");
                String raw = new String(Files.readAllBytes(request.toPath()), StandardCharsets.UTF_8);
                if (!raw.equals(lastRequest)) {
                    JSONObject input = new JSONObject(raw);
                    long id = input.getLong("request");
                    String operation = input.getString("op");
                    long delta = input.optLong("delta_ns", 0);
                    String requestToken = input.getString("token");
                    String binding = requestToken + ":" + operation + ":" + delta;
                    String[] prior = history.get(id);
                    JSONObject result = new JSONObject();
                    put(result, "request", id);
                    if (prior != null && prior[0].equals(binding)) {
                        publish(response, prior[1]);
                        lastRequest = raw;
                        main.postDelayed(this::poll, 20);
                        return;
                    } else if (prior != null) {
                        put(result, "outcome", "conflict");
                    } else if (!token.equals(requestToken)) {
                        put(result, "outcome", "wrong_token");
                    } else if (id <= 0 || history.size() >= 128) {
                        put(result, "outcome", "request_capacity");
                    } else {
                        put(result, "outcome", execute(operation, delta));
                        put(result, "state", snapshot());
                        history.put(id, new String[]{binding, result.toString()});
                    }
                    if (!result.has("state")) put(result, "state", snapshot());
                    publish(state, snapshot().toString());
                    publish(response, result.toString());
                    lastRequest = raw;
                }
            }
        } catch (Exception error) {
            fail();
        }
        main.postDelayed(this::poll, 20);
    }

    private static void put(JSONObject object, String key, Object value) {
        try { object.put(key, value); }
        catch (org.json.JSONException error) { throw new IllegalStateException(error); }
    }

    private static void publish(File destination, String value) {
        File temporary = new File(destination.getPath() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(temporary)) {
            out.write((value + "\n").getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
        } catch (Exception error) { throw new IllegalStateException(error); }
        if (!temporary.renameTo(destination)) throw new IllegalStateException("Publish failed");
    }
}
