package dev.frontcap.speechsubmission;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.speech.tts.TextToSpeech;
import android.widget.TextView;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import org.json.JSONObject;

/** Independent oracle: records only the actual framework return, never observer callbacks. */
public final class SubmissionActivity extends Activity {
    private TextToSpeech speech;
    private TextView display;
    private boolean ready, submitted;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        display = new TextView(this);
        display.setText("Waiting for native TTS initialization");
        setContentView(display);
        speech = new TextToSpeech(this, status -> {
            ready = status == TextToSpeech.SUCCESS;
            record("initialization", status, 0);
            display.setText(ready ? "Ready for submission diagnostic" : "Initialization failed");
        });
    }

    @Override public void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        if (!"submit".equals(intent.getStringExtra("action")) || submitted) return;
        if (!ready) throw new IllegalStateException("TTS is not initialized");
        submitted = true;
        Bundle parameters = new Bundle();
        parameters.putFloat(TextToSpeech.Engine.KEY_PARAM_VOLUME, 0.0f);
        String token = "same-consumer-token";
        record("ordinary", speech.speak("Hello", TextToSpeech.QUEUE_ADD, parameters, token), 5);
        String oversized = new String(new char[TextToSpeech.getMaxSpeechInputLength() + 1]).replace('\0', 'x');
        record("oversized", speech.speak(oversized, TextToSpeech.QUEUE_ADD, parameters, token), oversized.length());
        speech.shutdown();
        record("after-shutdown", speech.speak("Hello", TextToSpeech.QUEUE_ADD, parameters, token), 5);
        display.setText("Submission diagnostic complete; consult original results");
    }

    private synchronized void record(String operation, int result, int length) {
        try {
            JSONObject row = new JSONObject().put("operation", operation).put("returned", result).put("characters", length);
            try (FileOutputStream file = new FileOutputStream(new File(getFilesDir(), "submission-oracle.jsonl"), true)) {
                file.write((row.toString() + "\n").getBytes(StandardCharsets.UTF_8));
                file.getFD().sync();
            }
        } catch (Exception error) { throw new IllegalStateException(error); }
    }
}
