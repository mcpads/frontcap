package dev.frontcap.s18;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Process;
import android.os.Trace;
import android.util.AtomicFile;
import android.util.Log;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;
import org.json.JSONObject;

// Registers a fixture tap. A real button click or editor touch-up completes it.
// The fixture requires exclusive input: Android's injected touch carries no request ID.
public final class InputControl extends BroadcastReceiver {
  public interface Action { JSONObject run(); }
  private final Context context;
  private final String generation;
  private final Set<String> targets;
  private final AtomicFile receipt, checkpoint;
  private final String stopAt;
  private JSONObject accepted;
  private String owner;
  private long lastId;
  private boolean armed, unclaimed;
  private int clicks;

  public InputControl(Context context, String generation, String stopAt) {
    this(context, generation, stopAt, "native-increment");
  }

  public InputControl(Context context, String generation, String stopAt, String... targets) {
    Set<String> allowed = new HashSet<>(Arrays.asList(targets));
    if (allowed.isEmpty() || allowed.contains(null) || allowed.contains(""))
      throw new IllegalArgumentException("Input targets must be nonempty names");
    this.targets = Collections.unmodifiableSet(allowed);
    this.context = context;
    this.generation = generation;
    if (stopAt != null && !"before-touch-up".equals(stopAt) && !"before-action".equals(stopAt) && !"after-action".equals(stopAt)
        && !"after-receipt".equals(stopAt)) throw new IllegalArgumentException("Invalid input checkpoint");
    this.stopAt = stopAt;
    checkpoint = new AtomicFile(new File(context.getFilesDir(), "input-checkpoint.json"));
    receipt = new AtomicFile(new File(context.getFilesDir(), "input-control.json"));
    context.registerReceiver(this, new IntentFilter(context.getPackageName() + ".INPUT_CONTROL"),
        "android.permission.DUMP", null, Context.RECEIVER_EXPORTED);
    try { save(checkpoint, result("ready")); save(result("ready")); }
    catch (Exception error) { throw new IllegalStateException(error); }
  }

  public void close() { context.unregisterReceiver(this); }

  private JSONObject result(String status) throws Exception {
    return new JSONObject().put("generation", generation).put("pid", Process.myPid())
        .put("status", status).put("last_id", lastId).put("clicks", clicks);
  }

  private void save(JSONObject result) throws Exception { save(receipt, result); }
  private void save(AtomicFile file, JSONObject result) throws Exception { save(file, result, null); }
  private void save(AtomicFile file, JSONObject result, long[] timing) throws Exception {
    save(file, result, timing, null, null);
  }
  private void save(AtomicFile file, JSONObject result, long[] timing, long[][] observations, long[][] scheduling) throws Exception {
    mark(timing, 0, observations, scheduling);
    FileOutputStream stream = file.startWrite();
    try {
      mark(timing, 1, observations, scheduling);
      stream.write(result.toString().getBytes(StandardCharsets.UTF_8));
      mark(timing, 2, observations, scheduling);
      file.finishWrite(stream);
      mark(timing, 3, observations, scheduling);
    } catch (Exception error) { file.failWrite(stream); throw error; }
  }

  private void mark(long[] timing, int index, long[][] observations, long[][] scheduling) {
    if (timing == null) return;
    try {
      timing[index] = Steps.realNanos();
      if (observations != null) observations[index+2] = Steps.clockObservation();
      if (scheduling != null) scheduling[index+2] = Steps.schedulerObservation();
    }
    catch (Exception | LinkageError ignored) { /* Optional timing cannot change persistence. */ }
  }

  private boolean same(JSONObject a, JSONObject b) throws Exception {
    for (String key : new String[]{"generation", "controller_id", "request_id", "operation", "target", "x", "y"})
      if (!a.get(key).equals(b.get(key))) return false;
    return true;
  }

  public interface RegistrationReply { void complete(int code, String data); }

  @Override public void onReceive(Context ignored, Intent intent) {
    register(intent, "broadcast", (code, data) -> setResult(code, data, null));
  }

  /** Must run on the Activity main thread, just like the registered receiver. */
  public void register(Intent intent, String transport, RegistrationReply reply) {
    if (!"broadcast".equals(transport) && !"provider".equals(transport))
      throw new IllegalArgumentException("Unsupported input registration transport");
    long[][] observations = new long[7][];
    observations[0] = observeClock();
    long[][] scheduling = new long[7][];
    scheduling[0] = observeScheduler();
    boolean tracing = false;
    try {
      String session = intent.getStringExtra("generation");
      String controller = intent.getStringExtra("controller_id");
      String operation = intent.getStringExtra("operation");
      String target = intent.getStringExtra("target");
      long id = intent.getLongExtra("request_id", -1);
      int x = intent.getIntExtra("x", -1), y = intent.getIntExtra("y", -1);
      String error = null;
      if (!generation.equals(session)) error = "generation_mismatch";
      else if (controller == null || !UUID.fromString(controller).toString().equals(controller)
          || id <= 0 || !"tap".equals(operation) || !targets.contains(target) || x <= 0 || y <= 0)
        error = "invalid_request";
      JSONObject request = new JSONObject().put("generation", session).put("controller_id", controller)
          .put("request_id", id).put("operation", operation).put("target", target).put("x", x).put("y", y);
      if (error == null) {
        if (unclaimed) error = "unclaimed_input";
        else if (owner != null && !owner.equals(controller)) error = "controller_mismatch";
        else if (accepted != null && same(accepted, request)) {
          reply.complete(-1, session + ":" + id); return;
        } else if (armed) error = "input_pending";
        else if (id == lastId) error = "request_conflict";
        else if (id < lastId) error = "request_retired";
        else if (lastId == Long.MAX_VALUE || id != lastId + 1) error = "sequence_gap";
      }
      if (error != null) {
        // Rejections never overwrite the accepted request's recovery evidence.
        reply.complete(1, error); return;
      }
      // Validated UUIDs plus a positive long fit within Trace's 127-character label limit.
      // This names the accepted request even when a short-lived cmd loses procfs metadata.
      try {
        if (Trace.isEnabled()) {
          Trace.beginSection("FrontcapInput:" + session + ":" + controller + ":" + id);
          tracing = true;
        }
      } catch (RuntimeException | LinkageError ignoredTrace) { /* Optional observation only. */ }
      accepted = request; owner = controller; armed = true;
      observations[1] = observeClock();
      scheduling[1] = observeScheduler();
      diagnostic("registration_accepted", target, transport, true);
      long[] timing = new long[4];
      save(receipt, result("armed").put("request", accepted), timing, observations, scheduling);
      diagnostic("registration_saved", target, transport, true, timing);
      reply.complete(-1, session + ":" + id);
      observations[6] = observeClock();
      scheduling[6] = observeScheduler();
      diagnostic("registration_result_set", target, transport, true);
      registrationTiming(request, transport, observations, scheduling);
    } catch (IllegalArgumentException error) {
      reply.complete(1, "invalid_request");
    } catch (Exception error) {
      throw new IllegalStateException("Input registration failed", error);
    } finally {
      if (tracing) {
        try { Trace.endSection(); }
        catch (RuntimeException | LinkageError ignoredTrace) { /* Preserve the primary result. */ }
      }
    }
  }

  private void checkpoint(String phase) throws Exception {
    if (!phase.equals(stopAt)) return;
    String name = "input-release-" + generation + "-" + accepted.getLong("request_id");
    File release = new File(context.getFilesDir(), name);
    if (release.exists()) throw new IllegalStateException("Stale input release");
    long started = Steps.realNanos();
    JSONObject marker = result("waiting").put("phase", phase).put("request", accepted)
        .put("release_file", name).put("wait_ms", 5000).put("started_real_ns", started);
    save(checkpoint, marker);
    while (Steps.realNanos() - started < 5000000000L) {
      if (release.isFile()) {
        if (!release.delete()) throw new IllegalStateException("Input release deletion failed");
        save(checkpoint, marker.put("status", "released").put("released_real_ns", Steps.realNanos()));
        return;
      }
      Steps.hold(10);
    }
    save(checkpoint, marker.put("status", "expired").put("expired_real_ns", Steps.realNanos()));
    Process.killProcess(Process.myPid());
    System.exit(78);
  }

  public void touchUp(String target) {
    try {
      if (armed && !unclaimed && accepted.getString("target").equals(target)) checkpoint("before-touch-up");
    } catch (Exception error) { throw new IllegalStateException("Touch checkpoint failed", error); }
  }

  public void clicked(String target, Action action) { delivered(target, "android-onclick", action); }

  public void editorTouched(String target, Action observation) {
    delivered(target, "android-editor-touch-up", observation);
  }

  private long[] observeClock() {
    try { return Steps.clockObservation(); }
    catch (Exception | LinkageError ignored) { return null; }
  }

  private long[] observeScheduler() {
    try { return Steps.schedulerObservation(); }
    catch (Exception | LinkageError ignored) { return null; }
  }

  private void registrationTiming(JSONObject request, String transport, long[][] observations, long[][] scheduling) {
    try {
      String[] names = {"receiver_entered", "accepted", "write_started", "stream_opened",
          "bytes_written", "finish_returned", "result_set"};
      JSONObject samples = new JSONObject();
      JSONObject scheduler = new JSONObject();
      for (int i=0; i<names.length; i++) {
        long[] q = scheduling[i];
        scheduler.put(names[i], q == null ? JSONObject.NULL : new JSONObject()
            .put("before_ns", q[0]).put("after_ns", q[1]).put("tid", q[2]).put("errno", q[3])
            .put("running_ns", q[4]).put("runqueue_ns", q[5]).put("timeslices", q[6]).put("schedstats_enabled", q[7]).put("schedstats_errno", q[8]));
        long[] v = observations[i];
        samples.put(names[i], v == null ? JSONObject.NULL : new JSONObject()
            .put("before_ns", v[0]).put("thread_cpu_ns", v[1]).put("after_ns", v[2]).put("tid", v[3]));
      }
      Log.i("FrontcapInput", new JSONObject().put("phase", "receiver_timing")
          .put("generation", generation).put("pid", Process.myPid()).put("request", request)
          .put("registration_transport", transport).put("samples", samples).put("scheduler", scheduler).toString());
    } catch (Exception | LinkageError ignored) { /* Diagnostics cannot alter registration. */ }
  }

  // Timing diagnostics never constitute a terminal receipt or authorize another input.
  private void diagnostic(String phase, String target, String delivery, boolean matched) {
    diagnostic(phase, target, delivery, matched, null);
  }

  private void diagnostic(String phase, String target, String delivery, boolean matched, long[] timing) {
    try {
      JSONObject persistence = timing == null ? null : new JSONObject();
      if (timing != null) {
        String[] names = {"write_started", "stream_opened", "bytes_written", "finish_returned"};
        for (int i = 0; i < names.length; i++)
          persistence.put(names[i], timing[i] > 0 ? timing[i] : JSONObject.NULL);
      }
      Log.i("FrontcapInput", new JSONObject().put("phase", phase).put("generation", generation)
          .put("pid", Process.myPid()).put("tid", Process.myTid())
          .put("real_monotonic_ns", Steps.realNanos()).put("target", target).put("delivery", delivery)
          .put("request", matched ? accepted : JSONObject.NULL)
          .put("persistence_real_monotonic_ns", timing == null ? JSONObject.NULL : persistence).toString());
    } catch (Exception | LinkageError ignored) { /* Keep diagnostic failures out of input outcomes. */ }
  }

  private void delivered(String target, String delivery, Action action) {
    try {
      boolean matched = armed && !unclaimed && accepted.getString("target").equals(target);
      diagnostic("delivery_entered", target, delivery, matched);
      // Emit these boundaries only after the original atomic write, in the existing phase log.
      long[] timing = new long[4];
      save(receipt, result(matched ? "executing" : "unclaimed")
          .put("request", matched ? accepted : JSONObject.NULL), timing);
      diagnostic("intent_saved", target, delivery, matched, timing);
      // Never suppress a duplicate or unrelated business action to make recovery appear safe.
      if (matched) checkpoint("before-action");
      diagnostic("action_started", target, delivery, matched);
      JSONObject observation = action.run();
      diagnostic("action_finished", target, delivery, matched);
      clicks++;
      if (matched) checkpoint("after-action");
      if (matched) lastId = accepted.getLong("request_id");
      else unclaimed = true;
      armed = false;
      diagnostic("receipt_started", target, delivery, matched);
      save(result(matched ? "ok" : "unclaimed").put("request", matched ? accepted : JSONObject.NULL)
          .put("delivery", delivery).put("target", target).put("observation", observation));
      diagnostic("receipt_saved", target, delivery, matched);
      if (matched) checkpoint("after-receipt");
    } catch (Exception error) {
      throw new IllegalStateException("Input outcome could not be recorded", error);
    }
  }
}
