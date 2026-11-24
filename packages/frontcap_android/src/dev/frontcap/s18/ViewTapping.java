package dev.frontcap.s18;

import android.app.Activity;
import android.graphics.Rect;
import android.util.AtomicFile;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewParent;
import android.view.Window;
import android.widget.Button;
import android.widget.EditText;
import java.io.File;
import java.io.FileOutputStream;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Proxy;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.function.Supplier;
import org.json.JSONObject;

/** Register a fresh source target, then witness real window touch delivery without replacing listeners. */
final class ViewTapping implements AutoCloseable {
  private final Activity app;
  private final ViewScrolling scrolling;
  private final ActivityClock clock;
  private final Supplier<String> latest;
  private final Window.Callback original, wrapper;
  private final AtomicFile receipt;
  private String controller, catalog;
  private long lastId, downTime;
  private JSONObject accepted, completed, pending;
  private ViewObservation views;
  private boolean dispatched, suppress;
  private boolean downHandled;
  ViewTapping(Activity app, ActivityClock clock, Supplier<String> latest, ViewScrolling scrolling) {
    this.app=app; this.clock=clock; this.latest=latest; this.scrolling=scrolling;
    receipt=new AtomicFile(new File(app.getFilesDir(),"view-tap.json"));
    original=app.getWindow().getCallback();
    wrapper=(Window.Callback)Proxy.newProxyInstance(Window.Callback.class.getClassLoader(),
        new Class<?>[]{Window.Callback.class},(proxy,method,args)-> {
          if (method.getName().equals("dispatchTouchEvent")) return touch((MotionEvent)args[0]);
          try { return method.invoke(original,args); }
          catch (InvocationTargetException error) { throw error.getCause(); }
        });
    app.getWindow().setCallback(wrapper);
  }
  @Override public void close() {
    if (app.getWindow().getCallback()==wrapper) app.getWindow().setCallback(original);
  }
  boolean ownsWindow() { return app.getWindow().getCallback()==wrapper; }
  boolean unresolved() { return pending!=null || suppress; }
  static JSONObject parse(String raw) throws Exception {
    if (raw==null || raw.length()>4096) throw new IllegalArgumentException("invalid_tap_request");
    JSONObject value=new JSONObject(new String(android.util.Base64.decode(raw,android.util.Base64.NO_WRAP),StandardCharsets.UTF_8));
    if (value.length()!=8) throw new IllegalArgumentException("invalid_tap_request");
    for (String name:new String[]{"generation","controller_id","observation","target"}) {
      Object id=value.get(name);
      if (!(id instanceof String) || !UUID.fromString((String)id).toString().equals(id))
        throw new IllegalArgumentException("invalid_tap_identity");
    }
    for (String name:new String[]{"request_id","now_ns","x","y"}) {
      Object number=value.get(name);
      if (!(number instanceof Long) && !(number instanceof Integer)) throw new IllegalArgumentException("invalid_tap_integer");
      if (value.getLong(name)<0) throw new IllegalArgumentException("invalid_tap_integer");
    }
    if (value.getLong("request_id")==0 || value.getLong("x")>Integer.MAX_VALUE || value.getLong("y")>Integer.MAX_VALUE)
      throw new IllegalArgumentException("invalid_tap_integer");
    return value;
  }
  private static boolean same(JSONObject a, JSONObject b) throws Exception {
    for (String key:new String[]{"generation","controller_id","request_id","observation","target","now_ns","x","y"})
      if (!a.get(key).equals(b.get(key))) return false;
    return true;
  }
  private void save(JSONObject value) throws Exception {
    byte[] bytes=value.toString().getBytes(StandardCharsets.UTF_8);
    FileOutputStream stream=receipt.startWrite();
    try { stream.write(bytes); receipt.finishWrite(stream); }
    catch (Exception error) { receipt.failWrite(stream); throw error; }
  }
  private JSONObject result(String state) throws Exception {
    return new JSONObject().put("request",pending).put("generation",clock.generation())
        .put("pid",android.os.Process.myPid()).put("now_ns",clock.now())
        .put("last_id",pending.getLong("request_id")).put("state",state);
  }
  private void finish(JSONObject result) throws Exception {
    save(result); accepted=pending; completed=result; lastId=pending.getLong("request_id"); pending=null;
  }
  private static boolean ancestor(View parent, View child) {
    for (View current=child;current!=null;) {
      if (current==parent) return true;
      ViewParent p=current.getParent(); current=p instanceof View?(View)p:null;
    }
    return false;
  }
  static boolean actionable(View target, ViewObservation views, int x, int y) {
    if (!(target instanceof Button || target instanceof EditText) || !target.isEnabled() || !target.isClickable()
        || !ViewObservation.shown(target) || ViewObservation.transformed(target)
        || !ViewScrolling.ancestorsWithinBounds(target)
        || !ViewObservation.visibleBoundsScreen(target).contains(x,y)) return false;
    // Refuse overlapping branches instead of guessing custom drawing order or interception.
    for (View other:views.targets()) {
      if (other==target || ancestor(other,target) || ancestor(target,other)) continue;
      if (ViewObservation.shown(other) && ViewObservation.visibleBoundsScreen(other).contains(x,y)) return false;
    }
    return true;
  }
  static org.json.JSONArray point(View target, ViewObservation views) throws Exception {
    Rect rect=ViewObservation.visibleBoundsScreen(target);
    int x=rect.centerX(), y=rect.centerY();
    return !rect.isEmpty() && x>=0 && y>=0 && actionable(target,views,x,y)
        ? new org.json.JSONArray(new int[]{x,y}) : null;
  }
  private String validate() throws Exception {
    if (!pending.getString("observation").equals(latest.get()) || clock.now()!=pending.getLong("now_ns"))
      return "stale_observation";
    JSONObject current=views.read(app);
    if (!current.toString().equals(catalog)) return "changed_view_state";
    if (!current.getBoolean("window_focused") || app.getWindow().getCallback()!=wrapper
        || !actionable(views.resolve(pending.getString("target")),views,pending.getInt("x"),pending.getInt("y")))
      return "unsupported_target";
    return null;
  }
  void arm(JSONObject request, ViewObservation views, String observation, String catalog, long observedTime) throws Exception {
    if (!clock.generation().equals(request.getString("generation"))) throw new IllegalStateException("generation_mismatch");
    String owner=request.getString("controller_id");long id=request.getLong("request_id");
    if (controller!=null && !controller.equals(owner)) throw new IllegalStateException("controller_mismatch");
    if (pending!=null || suppress) throw new IllegalStateException("tap_sequence_unresolved");
    if (id==lastId && accepted!=null && same(accepted,request)) { save(completed);return; }
    if (id!=lastId+1) throw new IllegalStateException("tap_sequence_unresolved");
    controller=owner;pending=request;this.views=views;this.catalog=catalog;dispatched=false;
    String error=(!request.getString("observation").equals(observation) || request.getLong("now_ns")!=observedTime)
        ? "stale_observation" : validate();
    if (error!=null) finish(result("rejected").put("reason",error));
    else save(result("armed"));
  }
  private boolean matches(MotionEvent event) throws Exception {
    return event.getPointerCount()==1 && event.isFromSource(android.view.InputDevice.SOURCE_TOUCHSCREEN)
        && Math.abs(event.getRawX()-pending.getInt("x"))<0.5f && Math.abs(event.getRawY()-pending.getInt("y"))<0.5f;
  }
  // Diagnostic only: public state does not prove dispatch to a child or scroller settlement.
  private void traceTouch(String phase, MotionEvent event) {
    try {
      org.json.JSONArray chain=new org.json.JSONArray();
      for (View view=views.resolve(pending.getString("target"));view!=null;) {
        chain.put(new JSONObject().put("class",view.getClass().getName())
            .put("pressed",view.isPressed()).put("focused",view.isFocused())
            .put("attached",view.isAttachedToWindow()).put("transient_state",view.hasTransientState())
            .put("scroll_x",view.getScrollX()).put("scroll_y",view.getScrollY()));
        ViewParent parent=view.getParent();view=parent instanceof View?(View)parent:null;
      }
      android.util.Log.i("FrontcapTap",new JSONObject().put("phase",phase)
          .put("request",pending).put("pid",android.os.Process.myPid())
          .put("event_time_ms",event.getEventTime()).put("down_time_ms",event.getDownTime())
          .put("chain",chain).toString());
    } catch (Exception error) {
      android.util.Log.w("FrontcapTap","Touch diagnostic unavailable",error);
    }
  }
  private boolean touch(MotionEvent event) throws Exception {
    if (scrolling.unresolved()) return scrolling.touch(event,original);
    int action=event.getActionMasked();
    if (suppress) {
      if (action==MotionEvent.ACTION_UP || action==MotionEvent.ACTION_CANCEL) suppress=false;
      return true;
    }
    if (pending==null) return original.dispatchTouchEvent(event);
    if (!dispatched && action==MotionEvent.ACTION_DOWN) {
      String error=validate();
      if (error==null && !matches(event)) error="unexpected_touch";
      if (error!=null) {
        suppress=true;finish(result("rejected").put("reason",error));return true;
      }
      // Persist uncertainty before any application callback can change state.
      dispatched=true;downTime=event.getDownTime();save(result("pending"));
      traceTouch("before_down",event);
      downHandled=original.dispatchTouchEvent(event);
      traceTouch("after_down",event);return downHandled;
    }
    if (dispatched && action==MotionEvent.ACTION_UP && matches(event) && event.getDownTime()==downTime
        && clock.now()==pending.getLong("now_ns")) {
      traceTouch("before_up",event);
      boolean upHandled=original.dispatchTouchEvent(event);
      traceTouch("after_up",event);
      if (clock.now()!=pending.getLong("now_ns")) throw new IllegalStateException("clock_changed_during_tap");
      finish(result("completed").put("down_handled",downHandled).put("up_handled",upHandled));return upHandled;
    }
    // A malformed/cancelled stream after DOWN cannot claim no effect. Retain the pending fence.
    return true;
  }
}
