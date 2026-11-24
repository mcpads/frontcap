package dev.frontcap.s18;

import android.app.Activity;
import android.content.*;
import android.database.Cursor;
import android.net.Uri;
import android.os.*;
import android.util.AtomicFile;
import android.util.Log;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import org.json.JSONObject;

/** Generation-bound source snapshots and separately validated committed text edits. */
public final class ViewObservationProvider extends ContentProvider {
  private static final class Source {
    final Activity app;
    final ActivityClock clock;
    final ViewObservation views = new ViewObservation();
    final ViewEditing editing;
    final ViewTapping tapping;
    final ViewScrolling scrolling;
    String observation, catalog;
    long observedTime;
    Source(Activity app, ActivityClock clock) {
      this.app=app; this.clock=clock; editing=new ViewEditing(app,clock);
      scrolling=new ViewScrolling(app,clock,()->observation,this::ownsWindow);
      tapping=new ViewTapping(app,clock,()->observation,scrolling);
    }
    boolean ownsWindow() { return tapping.ownsWindow(); }
    void publish(String request) {
      try {
        JSONObject result = new JSONObject().put("request",request).put("generation",clock.generation())
            .put("pid",android.os.Process.myPid()).put("now_ns",clock.now());
        observation=null; catalog=null;
        try { result.put("catalog",views.read(app)).put("state","observed"); }
        catch (Exception error) {
          String reason=error.getMessage();
          if (!java.util.Arrays.asList("window_not_laid_out","text_budget_exceeded","view_budget_exceeded").contains(reason))
            reason="capture_failed";
          result.put("state","unavailable").put("reason",reason);
        }
        byte[] bytes = result.toString().getBytes(StandardCharsets.UTF_8);
        if (bytes.length > 1024*1024) {
          result.remove("catalog"); result.put("state","unavailable").put("reason","response_budget_exceeded");
          bytes=result.toString().getBytes(StandardCharsets.UTF_8);
        }
        AtomicFile file = new AtomicFile(new File(app.getFilesDir(),"view-observation.json"));
        FileOutputStream stream = file.startWrite();
        try {
          stream.write(bytes); file.finishWrite(stream);
          if ("observed".equals(result.getString("state"))) {
            observation=request; catalog=result.getJSONObject("catalog").toString(); observedTime=result.getLong("now_ns");
          }
        }
        catch (Exception error) { file.failWrite(stream); throw error; }
      } catch (Exception error) { Log.e("FrontcapViews","Cannot publish observation",error); }
    }
  }
  private static volatile Source active;
  static void bind(Activity app, ActivityClock clock) { active=new Source(app,clock); }
  static void unbind(ActivityClock clock) { Source source=active; if (source!=null && source.clock==clock) { source.tapping.close(); active=null; } }
  @Override public boolean onCreate() { return true; }
  @Override public Bundle call(String method, String arg, Bundle extras) {
    getContext().enforceCallingPermission("android.permission.DUMP","View observation requires DUMP");
    if ("scroll".equals(method)) return scroll(arg,extras);
    if ("tap".equals(method)) return tap(arg,extras);
    if ("edit".equals(method)) return edit(arg,extras);
    if (!"observe".equals(method) || arg!=null || extras==null || extras.size()!=2)
      throw new IllegalArgumentException("Unsupported View observation");
    String generation=extras.getString("generation"), request=extras.getString("request");
    if (request==null || !UUID.fromString(request).toString().equals(request))
      throw new IllegalArgumentException("Invalid observation request");
    Source source=active;
    if (source==null || !source.clock.generation().equals(generation))
      throw new IllegalStateException("View observation generation unavailable");
    if (!new Handler(Looper.getMainLooper()).post(() -> {
      if (active==source) source.publish(request);
    })) throw new IllegalStateException("Activity main queue rejected observation");
    Bundle reply=new Bundle(); reply.putBoolean("submitted",true); return reply;
  }
  private Bundle edit(String arg, Bundle extras) {
    if (arg!=null || extras==null || extras.size()!=2) throw new IllegalArgumentException("Unsupported View edit");
    Source source=active;
    if (source==null || !source.clock.generation().equals(extras.getString("generation")))
      throw new IllegalStateException("View edit generation unavailable");
    final JSONObject request;
    try { request=ViewEditing.parse(source.app,extras.getString("payload_sha256")); }
    catch (Exception error) { throw new IllegalArgumentException("Invalid View edit",error); }
    if (!source.clock.generation().equals(request.optString("generation")))
      throw new IllegalStateException("View edit generation unavailable");
    if (!new Handler(Looper.getMainLooper()).post(() -> {
      if (active!=source) return;
      try {
        if (source.tapping.unresolved() || source.scrolling.unresolved()) throw new IllegalStateException("tap_sequence_unresolved");
        source.editing.edit(request,source.views,source.observation,source.catalog,source.observedTime); }
      catch (Exception error) { Log.e("FrontcapViews","Edit unresolved; do not resend",error); }
    })) throw new IllegalStateException("Activity main queue rejected edit");
    Bundle reply=new Bundle(); reply.putBoolean("submitted",true); return reply;
  }
  private Bundle tap(String arg, Bundle extras) {
    if (arg!=null || extras==null || extras.size()!=2) throw new IllegalArgumentException("Unsupported View tap");
    Source source=active;
    if (source==null || !source.clock.generation().equals(extras.getString("generation")))
      throw new IllegalStateException("View tap generation unavailable");
    final JSONObject request;
    try { request=ViewTapping.parse(extras.getString("request_base64")); }
    catch (Exception error) { throw new IllegalArgumentException("Invalid View tap",error); }
    if (!source.clock.generation().equals(request.optString("generation")))
      throw new IllegalStateException("View tap generation unavailable");
    if (!new Handler(Looper.getMainLooper()).post(() -> {
      if (active!=source) return;
      try {
        if (source.editing.unresolved() || source.scrolling.unresolved()) throw new IllegalStateException("edit_sequence_unresolved");
        source.tapping.arm(request,source.views,source.observation,source.catalog,source.observedTime);
      } catch (Exception error) { Log.e("FrontcapViews","Tap unresolved; do not resend",error); }
    })) throw new IllegalStateException("Activity main queue rejected tap");
    Bundle reply=new Bundle();reply.putBoolean("submitted",true);return reply;
  }
  private Bundle scroll(String arg, Bundle extras) {
    if (arg!=null || extras==null || extras.size()!=2) throw new IllegalArgumentException("Unsupported View scroll");
    Source source=active;
    if(source==null || !source.clock.generation().equals(extras.getString("generation")))
      throw new IllegalStateException("View scroll generation unavailable");
    final JSONObject request;
    try {request=ViewScrolling.parse(extras.getString("request_base64"));}
    catch(Exception error){throw new IllegalArgumentException("Invalid View scroll",error);}
    if(!source.clock.generation().equals(request.optString("generation")))throw new IllegalStateException("View scroll generation unavailable");
    if(!new Handler(Looper.getMainLooper()).post(()->{
      if(active!=source)return;
      try {
        if(source.editing.unresolved() || source.tapping.unresolved())throw new IllegalStateException("input_sequence_unresolved");
        source.scrolling.arm(request,source.views,source.observation,source.catalog,source.observedTime);
      }catch(Exception error){Log.e("FrontcapViews","Scroll unresolved; do not resend",error);}
    }))throw new IllegalStateException("Activity main queue rejected scroll");
    Bundle reply=new Bundle();reply.putBoolean("submitted",true);return reply;
  }
  @Override public Cursor query(Uri u,String[] p,String s,String[] a,String o) { throw new UnsupportedOperationException(); }
  @Override public String getType(Uri u) { throw new UnsupportedOperationException(); }
  @Override public Uri insert(Uri u,ContentValues v) { throw new UnsupportedOperationException(); }
  @Override public int delete(Uri u,String s,String[] a) { throw new UnsupportedOperationException(); }
  @Override public int update(Uri u,ContentValues v,String s,String[] a) { throw new UnsupportedOperationException(); }
}
