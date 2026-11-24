package dev.frontcap.s18;

import android.app.Activity;
import android.os.Looper;
import org.json.JSONObject;

/** One source-owned Activity generation attached to the existing native clock runtime. */
public final class ActivityClock implements AutoCloseable {
  public interface Observation { JSONObject read(); }
  private final ClockControl control;
  private final long origin;
  private boolean closed;

  public ActivityClock(Activity app, Observation observation) {
    if (Looper.myLooper() != Looper.getMainLooper())
      throw new IllegalStateException("Attach the Activity clock on the main thread");
    System.loadLibrary("vclock");
    Steps.prepare();
    origin = System.nanoTime();
    control = new ClockControl(app, new ClockControl.Clock() {
      public long now() { return ActivityClock.this.now(); }
      public long advanceLimit() { return Steps.advanceLimit(); }
      public void advance(long delta) { Steps.advance(delta); }
      public JSONObject observe() {
        if ("1".equals(System.getenv("FC_PROFILE_SAVER_WAIT"))) {
          try {
            JSONObject snapshot = new JSONObject().put("pid", android.os.Process.myPid())
                .put("generation", generation()).put("now_ns", now())
                .put("waits", new JSONObject(Steps.futexStats()));
            java.nio.file.Files.write(new java.io.File(app.getFilesDir(), "futex-stats.json").toPath(),
                snapshot.toString().getBytes(java.nio.charset.StandardCharsets.UTF_8));
          } catch (Exception error) { throw new IllegalStateException(error); }
        }
        return observation.read();
      }
    }, app.getIntent().getStringExtra("clock_stop_at"));
    ActivityClockProvider.bind(control);
    ViewObservationProvider.bind(app,this);
  }

  public long now() { return System.nanoTime() - origin; }
  public String generation() { return control.generation(); }
  @Override public void close() {
    if (Looper.myLooper() != Looper.getMainLooper())
      throw new IllegalStateException("Detach the Activity clock on the main thread");
    if (closed) return;
    ViewObservationProvider.unbind(this);
    ActivityClockProvider.unbind(control);
    control.close();
    closed = true;
  }
}
