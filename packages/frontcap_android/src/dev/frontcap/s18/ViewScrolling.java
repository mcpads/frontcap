package dev.frontcap.s18;

import android.app.Activity;
import android.graphics.Rect;
import android.util.AtomicFile;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewParent;
import android.view.Window;
import android.widget.ScrollView;
import android.widget.HorizontalScrollView;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.function.Supplier;
import org.json.JSONArray;
import org.json.JSONObject;

/** One observed stock View scroll gesture; completion proves delivery, not animation settlement. */
final class ViewScrolling {
  private final Activity app;
  private final ActivityClock clock;
  private final Supplier<String> latest;
  private final Supplier<Boolean> ownsWindow;
  private final AtomicFile receipt;
  private String controller, catalog;
  private long lastId, downTime;
  private JSONObject accepted, completed, pending;
  private ViewObservation views;
  private View target;
  private boolean dispatched, suppress, downHandled, invalidStream;
  private int moves;
  private int[] before;
  ViewScrolling(Activity app, ActivityClock clock, Supplier<String> latest, Supplier<Boolean> ownsWindow) {
    this.app=app;this.clock=clock;this.latest=latest;this.ownsWindow=ownsWindow;
    receipt=new AtomicFile(new File(app.getFilesDir(),"view-scroll.json"));
  }
  boolean unresolved() { return pending!=null || suppress; }
  private static boolean ancestor(View parent, View child) {
    for (View current=child;current!=null;) {
      if (current==parent) return true;
      ViewParent p=current.getParent();current=p instanceof View?(View)p:null;
    }
    return false;
  }
  private static int maximum(View view, boolean vertical) {
    ViewGroup group=(ViewGroup)view;
    int viewport=vertical?view.getHeight()-view.getPaddingTop()-view.getPaddingBottom()
        :view.getWidth()-view.getPaddingLeft()-view.getPaddingRight();
    return group.getChildCount()==0?0:Math.max(0,
        (vertical?group.getChildAt(0).getHeight():group.getChildAt(0).getWidth())-Math.max(0,viewport));
  }
  static boolean ancestorsWithinBounds(View target) {
    for (ViewParent parent=target.getParent();parent instanceof View;parent=parent.getParent()) {
      View view=(View)parent;
      boolean vertical=view.getClass()==ScrollView.class;
      if (!vertical && view.getClass()!=HorizontalScrollView.class) continue;
      int x=view.getScrollX(),y=view.getScrollY(),limit=maximum(view,vertical);
      if (x<0 || y<0 || (vertical?(x!=0 || y>limit):(y!=0 || x>limit))) return false;
    }
    // In-range geometry alone does not establish that a fling has finished.
    return true;
  }
  static JSONObject geometry(View view, ViewObservation views) throws Exception {
    boolean vertical=view.getClass()==ScrollView.class;
    if (!vertical && view.getClass()!=HorizontalScrollView.class) return null;
    int width=Math.max(0,view.getWidth()-view.getPaddingLeft()-view.getPaddingRight());
    int height=Math.max(0,view.getHeight()-view.getPaddingTop()-view.getPaddingBottom());
    int maximum=maximum(view,vertical);
    int x=view.getScrollX(), y=view.getScrollY();
    JSONArray gestures=new JSONArray();
    JSONObject result=new JSONObject().put("offset_px",new JSONArray(new int[]{x,y}))
        .put("maximum_px",new JSONArray(new int[]{vertical?0:maximum,vertical?maximum:0}))
        .put("viewport_px",new JSONArray(new int[]{width,height})).put("gestures",gestures);
    Rect local=ViewObservation.visibleBoundsScreen(view);int[] origin=new int[2];view.getLocationOnScreen(origin);
    if (!view.isEnabled() || !ViewObservation.shown(view) || ViewObservation.transformed(view)
        || x<0 || y<0 || (vertical?(x!=0 || y>maximum):(y!=0 || x>maximum)) || local.isEmpty()) return result;
    if (!local.intersect(origin[0]+view.getPaddingLeft(),origin[1]+view.getPaddingTop(),
        origin[0]+view.getWidth()-view.getPaddingRight(),origin[1]+view.getHeight()-view.getPaddingBottom())) return result;
    int low=vertical?local.top+local.height()/4:local.left+local.width()/4;
    int high=vertical?local.bottom-local.height()/4-1:local.right-local.width()/4-1;
    int fixed=vertical?local.centerX():local.centerY();
    if (low<0 || fixed<0 || high-low<=2*android.view.ViewConfiguration.get(view.getContext()).getScaledTouchSlop()) return result;
    Rect corridor=vertical?new Rect(fixed,low,fixed+1,high+1):new Rect(low,fixed,high+1,fixed+1);
    for (View other:views.targets()) {
      if (ancestor(view,other) || ancestor(other,view) || !ViewObservation.shown(other)) continue;
      if (Rect.intersects(corridor,ViewObservation.visibleBoundsScreen(other))) return result;
    }
    for (int sign:new int[]{-1,1}) {
      int offset=vertical?y:x;
      if (sign<0?offset==0:offset==maximum) continue;
      int start=sign<0?low:high,end=sign<0?high:low;
      gestures.put(new JSONObject().put("direction",vertical?(sign<0?"up":"down"):(sign<0?"left":"right"))
          .put("start_px",new JSONArray(new int[]{vertical?fixed:start,vertical?start:fixed}))
          .put("end_px",new JSONArray(new int[]{vertical?fixed:end,vertical?end:fixed})));
    }
    return result;
  }
  static JSONObject parse(String raw) throws Exception {
    if (raw==null || raw.length()>4096) throw new IllegalArgumentException("invalid_scroll_request");
    JSONObject value=new JSONObject(new String(android.util.Base64.decode(raw,android.util.Base64.NO_WRAP),StandardCharsets.UTF_8));
    if (value.length()!=11) throw new IllegalArgumentException("invalid_scroll_request");
    for (String key:new String[]{"generation","controller_id","observation","target"}) {
      Object id=value.get(key);
      if (!(id instanceof String) || !UUID.fromString((String)id).toString().equals(id)) throw new IllegalArgumentException("invalid_scroll_identity");
    }
    for (String key:new String[]{"request_id","now_ns","x","y","end_x","end_y"}) {
      Object number=value.get(key);
      if (!(number instanceof Long) && !(number instanceof Integer) || value.getLong(key)<0
          || (!key.equals("request_id") && !key.equals("now_ns") && value.getLong(key)>Integer.MAX_VALUE)) throw new IllegalArgumentException("invalid_scroll_integer");
    }
    if (value.getLong("request_id")==0 || !java.util.Arrays.asList("up","down","left","right").contains(value.getString("direction")))
      throw new IllegalArgumentException("invalid_scroll_request");
    return value;
  }
  private static boolean same(JSONObject a,JSONObject b) throws Exception {
    for (String key:new String[]{"generation","controller_id","request_id","observation","target","now_ns","x","y","end_x","end_y","direction"})
      if (!a.get(key).equals(b.get(key))) return false;
    return true;
  }
  private void save(JSONObject result) throws Exception {
    FileOutputStream stream=receipt.startWrite();
    try {stream.write(result.toString().getBytes(StandardCharsets.UTF_8));receipt.finishWrite(stream);}
    catch(Exception error) {receipt.failWrite(stream);throw error;}
  }
  private JSONObject result(String state) throws Exception {
    return new JSONObject().put("request",pending).put("generation",clock.generation()).put("pid",android.os.Process.myPid())
        .put("now_ns",clock.now()).put("last_id",pending.getLong("request_id")).put("state",state);
  }
  private void finish(JSONObject value) throws Exception {
    save(value);accepted=pending;completed=value;lastId=pending.getLong("request_id");pending=null;
  }
  private String validate() throws Exception {
    if (!pending.getString("observation").equals(latest.get()) || clock.now()!=pending.getLong("now_ns")) return "stale_observation";
    JSONObject current=views.read(app);
    if (!current.toString().equals(catalog)) return "changed_view_state";
    if (!current.getBoolean("window_focused") || !ownsWindow.get()) return "unsupported_target";
    target=views.resolve(pending.getString("target"));
    JSONObject state=target==null?null:geometry(target,views);
    if (state==null) return "unsupported_target";
    JSONArray gestures=state.getJSONArray("gestures");
    for (int i=0;i<gestures.length();i++) {
      JSONObject gesture=gestures.getJSONObject(i);JSONArray start=gesture.getJSONArray("start_px"),end=gesture.getJSONArray("end_px");
      if (gesture.getString("direction").equals(pending.getString("direction")) && start.getInt(0)==pending.getInt("x")
          && start.getInt(1)==pending.getInt("y") && end.getInt(0)==pending.getInt("end_x") && end.getInt(1)==pending.getInt("end_y")) return null;
    }
    return "unsupported_target";
  }
  void arm(JSONObject request,ViewObservation views,String observation,String catalog,long observedTime) throws Exception {
    if (!clock.generation().equals(request.getString("generation"))) throw new IllegalStateException("generation_mismatch");
    String owner=request.getString("controller_id");long id=request.getLong("request_id");
    if (controller!=null && !controller.equals(owner)) throw new IllegalStateException("controller_mismatch");
    if (unresolved()) throw new IllegalStateException("scroll_sequence_unresolved");
    if (id==lastId && accepted!=null && same(accepted,request)) {save(completed);return;}
    if (id!=lastId+1) throw new IllegalStateException("scroll_sequence_unresolved");
    controller=owner;pending=request;this.views=views;this.catalog=catalog;dispatched=false;moves=0;invalidStream=false;
    String error=!request.getString("observation").equals(observation) || request.getLong("now_ns")!=observedTime?"stale_observation":validate();
    if (error!=null) finish(result("rejected").put("reason",error));else save(result("armed"));
  }
  private boolean point(MotionEvent e,String x,String y) throws Exception {
    return Math.abs(e.getRawX()-pending.getInt(x))<0.5f && Math.abs(e.getRawY()-pending.getInt(y))<0.5f;
  }
  boolean touch(MotionEvent e,Window.Callback original) throws Exception {
    int action=e.getActionMasked();
    if (suppress) {if(action==MotionEvent.ACTION_UP || action==MotionEvent.ACTION_CANCEL)suppress=false;return true;}
    if (pending==null) return original.dispatchTouchEvent(e);
    boolean single=e.getPointerCount()==1 && e.isFromSource(android.view.InputDevice.SOURCE_TOUCHSCREEN);
    if (!dispatched && action==MotionEvent.ACTION_DOWN) {
      // Held app frame time must not defer the rest of this touch stream behind batched MOVE delivery.
      app.getWindow().getDecorView().requestUnbufferedDispatch(e);
      String error=validate();if(error==null && (!single || !point(e,"x","y")))error="unexpected_touch";
      if(error!=null){suppress=true;finish(result("rejected").put("reason",error));return true;}
      before=new int[]{target.getScrollX(),target.getScrollY()};dispatched=true;downTime=e.getDownTime();save(result("pending"));
      downHandled=original.dispatchTouchEvent(e);return downHandled;
    }
    if (invalidStream) return true;
    if (dispatched && single && e.getDownTime()==downTime && clock.now()==pending.getLong("now_ns")) {
      if (action==MotionEvent.ACTION_MOVE) {
        float x=e.getRawX(),y=e.getRawY();
        if (x<Math.min(pending.getInt("x"),pending.getInt("end_x"))-.5f || x>Math.max(pending.getInt("x"),pending.getInt("end_x"))+.5f
            || y<Math.min(pending.getInt("y"),pending.getInt("end_y"))-.5f || y>Math.max(pending.getInt("y"),pending.getInt("end_y"))+.5f) {invalidStream=true;return true;}
        moves++;return original.dispatchTouchEvent(e);
      }
      if (action==MotionEvent.ACTION_UP && moves>0 && point(e,"end_x","end_y")) {
        boolean handled=original.dispatchTouchEvent(e);
        if(clock.now()!=pending.getLong("now_ns"))throw new IllegalStateException("clock_changed_during_scroll");
        finish(result("completed").put("down_handled",downHandled).put("up_handled",handled).put("moves",moves)
            .put("before_offset_px",new JSONArray(before)).put("after_offset_px",new JSONArray(new int[]{target.getScrollX(),target.getScrollY()})));
        return handled;
      }
    }
    invalidStream=true;
    return true; // Uncertain/cancelled delivery remains fenced, never a no-effect rejection.
  }
}
