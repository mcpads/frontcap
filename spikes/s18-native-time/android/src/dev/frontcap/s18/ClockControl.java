package dev.frontcap.s18;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Process;
import android.util.AtomicFile;
import android.util.Log;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import org.json.JSONObject;

// One controller, one activity generation; broadcast completion is not application idle.
public final class ClockControl extends BroadcastReceiver {
  public interface Completion { void acknowledge(int code, String data); }

  public interface Clock {
    long now();
    long advanceLimit();
    void advance(long delta);
    JSONObject observe();
  }
  private final Context context;
  private final Clock clock;
  private final String generation = UUID.randomUUID().toString();
  private final AtomicFile receipt;
  private final AtomicFile checkpoint;
  private final String stopAt;
  private long lastId;
  private String lastOperation;
  private long lastDelta;
  private String lastReply;

  public ClockControl(Context context, Clock clock, String stopAt) {
    this.context = context;
    this.clock = clock;
    if (stopAt != null && !"before-advance".equals(stopAt) && !"after-advance".equals(stopAt)
        && !"after-receipt".equals(stopAt)) throw new IllegalArgumentException("Invalid clock checkpoint");
    this.stopAt = stopAt;
    receipt = new AtomicFile(new File(context.getFilesDir(), "clock-control.json"));
    checkpoint = new AtomicFile(new File(context.getFilesDir(), "clock-checkpoint.json"));
    context.registerReceiver(this, new IntentFilter(context.getPackageName() + ".CLOCK_CONTROL"),
        "android.permission.DUMP", null, Context.RECEIVER_EXPORTED);
    try {
      save(checkpoint, new JSONObject().put("generation", generation).put("status", "armed"));
      save(new JSONObject().put("generation", generation).put("pid", Process.myPid())
          .put("status", "ready").put("last_id", 0).put("now_ns", clock.now()));
    } catch (Exception error) { throw new IllegalStateException(error); }
  }

  public void close() { context.unregisterReceiver(this); }
  public String generation() { return generation; }

  private void save(JSONObject result) throws Exception {
    save(receipt, result);
  }
  private void save(AtomicFile file, JSONObject result) throws Exception {
    FileOutputStream stream = file.startWrite();
    try {
      stream.write(result.toString().getBytes(StandardCharsets.UTF_8));
      file.finishWrite(stream);
    } catch (Exception error) { file.failWrite(stream); throw error; }
  }

  private void checkpoint(String phase, JSONObject request) throws Exception {
    if (!phase.equals(stopAt) || !"advance".equals(request.getString("operation"))) return;
    JSONObject marker = new JSONObject().put("generation", generation).put("pid", Process.myPid())
        .put("phase", phase).put("request", request).put("now_ns", clock.now())
        .put("status", "waiting").put("wait_ms", 5000);
    save(checkpoint, marker);
    // Existing native hold uses real SYS_nanosleep, unaffected by the manual app clock.
    // The host must force-stop this app before expiry; the marker is never a receipt.
    Steps.hold(5000);
    save(checkpoint, marker.put("status", "expired"));
    Process.killProcess(Process.myPid());
    System.exit(78);
  }

  // Diagnostic only: these events never authorize replay or replace the atomic receipt.
  private void diagnostic(String phase, JSONObject request, String transport) {
    try {
      Log.i("FrontcapClock", new JSONObject().put("phase", phase).put("generation", generation)
          .put("pid", Process.myPid()).put("tid", Process.myTid())
          .put("real_monotonic_ns", Steps.realNanos()).put("request", request).put("delivery", transport).toString());
    } catch (Exception | LinkageError ignored) { /* Observation failure must not change clock execution. */ }
  }

  private void acknowledge(JSONObject request, Completion completion, String transport) throws Exception {
    // am can finish an ordered broadcast after receiver death with its default result.
    // Confirm that this receiver reached the response boundary, including for rejections.
    completion.acknowledge(-1, request.getString("generation") + ":" + request.getLong("request_id"));
    diagnostic("acknowledged", request, transport);
  }

  @Override public void onReceive(Context ignored, Intent intent) {
    execute(intent, "broadcast", (code, data) -> setResult(code, data, null));
  }

  /** Called only on the Activity main queue; submission transport does not alter clock semantics. */
  public void execute(Intent intent, String transport, Completion completion) {
    if (!"broadcast".equals(transport) && !"provider".equals(transport))
      throw new IllegalArgumentException("Unknown clock transport");
    try {
      String session = intent.getStringExtra("generation");
      String operation = intent.getStringExtra("operation");
      long id = intent.getLongExtra("request_id", -1);
      long delta = intent.getLongExtra("delta_ns", -1);
      JSONObject request = new JSONObject().put("generation", session == null ? "" : session)
          .put("request_id", id).put("operation", operation == null ? "" : operation).put("delta_ns", delta);
      diagnostic("received", request, transport);
      JSONObject reply = new JSONObject().put("generation", generation).put("request", request)
          .put("pid", Process.myPid()).put("replayed", false).put("applied", false).put("before_ns", clock.now());
      String error = null;
      if (!generation.equals(session)) error = "generation_mismatch";
      else if (id <= 0 || delta < 0 || (!"read".equals(operation) && !"advance".equals(operation))
          || ("read".equals(operation) && delta != 0)) error = "invalid_request";
      else if (id == lastId) {
        if (!operation.equals(lastOperation) || delta != lastDelta) error = "request_conflict";
        else { save(new JSONObject(lastReply).put("replayed", true)); diagnostic("replay_saved", request, transport); acknowledge(request, completion, transport); return; }
      } else if (id < lastId) error = "request_retired";
      else if (lastId == Long.MAX_VALUE || id != lastId + 1) error = "sequence_gap";
      else if (clock.now() < 0 || delta > clock.advanceLimit()) error = "clock_overflow";
      if (error != null) {
        save(reply.put("status", "rejected").put("error", error).put("now_ns", clock.now()).put("last_id", lastId));
        diagnostic("rejection_saved", request, transport);
        acknowledge(request, completion, transport);
        return;
      }
      // Publish intent before mutation. Process death fences this generation rather than replaying it.
      save(reply.put("status", "executing").put("now_ns", clock.now()).put("last_id", lastId));
      diagnostic("intent_saved", request, transport);
      checkpoint("before-advance", request);
      if ("advance".equals(operation)) { clock.advance(delta); diagnostic("advanced", request, transport); }
      checkpoint("after-advance", request);
      reply.put("status", "ok").put("applied", "advance".equals(operation))
          .put("now_ns", clock.now()).put("observation", clock.observe()).put("last_id", id);
      diagnostic("observed", request, transport);
      lastId = id;
      lastOperation = operation;
      lastDelta = delta;
      lastReply = reply.toString();
      save(reply);
      diagnostic("receipt_saved", request, transport);
      checkpoint("after-receipt", request);
      acknowledge(request, completion, transport);
    } catch (Exception error) {
      // An unrecorded outcome must not leave a live generation accepting another mutation.
      throw new IllegalStateException("Clock command failed", error);
    }
  }
}
