package dev.frontcap.s18;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.graphics.Color;
import android.view.View;
import android.view.MotionEvent;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;
import java.io.File;
import java.io.FileWriter;
import org.json.JSONObject;

// Explicit fixture controls; renderer time is host-paced, application clocks stay manual.
public class VirtualActivity extends Activity {
  private int count, fired, sequence;
  private long uptime, elapsed, nano, wall;
  private TextView label, state;
  private View color;
  private ClockControl control;
  private InputControl input;

  private JSONObject publish() {
    try {
      JSONObject record = new JSONObject();
      record.put("sequence", ++sequence);
      record.put("count", count); record.put("fired", fired);
      record.put("uptime_ms", SystemClock.uptimeMillis() - uptime);
      record.put("elapsed_ms", SystemClock.elapsedRealtime() - elapsed);
      record.put("nano_ns", System.nanoTime() - nano);
      record.put("wall_ms", System.currentTimeMillis() - wall);
      record.put("manual", "1".equals(System.getenv("FC_MANUAL")));
      record.put("host_render", "1".equals(System.getenv("FC_HOST_RENDER")));
      File temporary = new File(getFilesDir(), "state.tmp");
      try (FileWriter out = new FileWriter(temporary)) { out.write(record.toString()); }
      if (!temporary.renameTo(new File(getFilesDir(), "state.json"))) throw new IllegalStateException("state rename");
      label.setText("Count: " + count);
      label.setContentDescription("native-count-" + count);
      state.setText("Time: " + record.getLong("uptime_ms") + " Timer: " + fired);
      color.setBackgroundColor(fired > 0 ? Color.BLUE : (count > 0 ? Color.GREEN : Color.RED));
      return record;
    } catch (Exception error) { throw new RuntimeException(error); }
  }

  private void button(LinearLayout layout, String name, String description, Runnable action) {
    Button button = new Button(this);
    button.setText(name); button.setContentDescription(description);
    if (getIntent().hasExtra("input_stop_at")) {
      button.setOnTouchListener((view, event) -> {
        if (event.getActionMasked() == MotionEvent.ACTION_UP) input.touchUp(description);
        return false;
      });
    }
    button.setOnClickListener(view -> input.clicked(description, () -> { action.run(); return publish(); }));
    layout.addView(button);
  }

  @Override public void onCreate(Bundle saved) {
    super.onCreate(saved);
    System.loadLibrary("vclock");
    Steps.prepare();
    uptime = SystemClock.uptimeMillis(); elapsed = SystemClock.elapsedRealtime();
    nano = System.nanoTime(); wall = System.currentTimeMillis();
    LinearLayout layout = new LinearLayout(this);
    layout.setOrientation(LinearLayout.VERTICAL); layout.setPadding(32, 120, 32, 32);
    layout.setBackgroundColor(Color.WHITE);
    label = new TextView(this); label.setTextSize(28); label.setTextColor(Color.BLACK);
    layout.addView(label);
    color = new View(this); color.setContentDescription("native-color");
    layout.addView(color, new LinearLayout.LayoutParams(240, 160));
    state = new TextView(this); state.setTextColor(Color.BLACK); state.setContentDescription("native-time");
    layout.addView(state);
    button(layout, "Increment", "native-increment", () -> count++);
    control = new ClockControl(this, new ClockControl.Clock() {
      public long now() { return System.nanoTime() - nano; }
      public long advanceLimit() { return Steps.advanceLimit(); }
      public void advance(long delta) { Steps.advance(delta); }
      public JSONObject observe() { return publish(); }
    }, getIntent().getStringExtra("clock_stop_at"));
    input = new InputControl(this, control.generation(), getIntent().getStringExtra("input_stop_at"));
    new Handler(Looper.getMainLooper()).postDelayed(() -> { fired++; publish(); }, 5000);
    publish(); setContentView(layout);
  }
  @Override public void onDestroy() {
    if (control != null) control.close();
    if (input != null) input.close();
    super.onDestroy();
  }
}
