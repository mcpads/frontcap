package dev.frontcap.s18;

import android.app.Activity;
import android.util.AtomicFile;
import android.widget.EditText;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import org.json.JSONObject;

/** Main-thread committed text edits. An uncertain mutation fences this Activity generation. */
final class ViewEditing {
  private final Activity app;
  private final ActivityClock clock;
  private final AtomicFile receipt;
  private String controller;
  private long lastId;
  private JSONObject accepted, completed;
  private boolean uncertain;
  ViewEditing(Activity app, ActivityClock clock) {
    this.app=app; this.clock=clock;
    receipt=new AtomicFile(new File(app.getFilesDir(),"view-edit.json"));
  }
  boolean unresolved() { return uncertain; }
  static JSONObject parse(Activity app, String digest) throws Exception {
    if (digest==null || !digest.matches("[0-9a-f]{64}")) throw new IllegalArgumentException("invalid_edit_digest");
    File path=new File(app.getFilesDir(),"view-edit-"+digest+".json");
    if (java.nio.file.Files.isSymbolicLink(path.toPath())) throw new IllegalArgumentException("linked_edit_request");
    byte[] bytes;
    try (java.io.FileInputStream stream=new java.io.FileInputStream(path)) { bytes=stream.readNBytes(196609); }
    if (bytes.length>196608) throw new IllegalArgumentException("edit_budget_exceeded");
    byte[] hash=java.security.MessageDigest.getInstance("SHA-256").digest(bytes);
    StringBuilder actual=new StringBuilder();
    for (byte value:hash) actual.append(String.format(java.util.Locale.ROOT,"%02x",value & 255));
    if (!actual.toString().equals(digest)) throw new IllegalArgumentException("edit_digest_mismatch");
    JSONObject value=new JSONObject(new String(bytes,StandardCharsets.UTF_8));
    if (value.length()!=9) throw new IllegalArgumentException("invalid_edit_request");
    for (String name:new String[]{"generation","controller_id","observation","target"}) {
      Object raw=value.get(name);
      if (!(raw instanceof String) || !UUID.fromString((String)raw).toString().equals(raw))
        throw new IllegalArgumentException("invalid_edit_identity");
    }
    for (String name:new String[]{"request_id","now_ns","start","end"}) {
      Object raw=value.get(name);
      if (!(raw instanceof Long) && !(raw instanceof Integer)) throw new IllegalArgumentException("invalid_edit_integer");
    }
    for (String name:new String[]{"start","end"})
      if (value.getLong(name)<0 || value.getLong(name)>0xffffffffL) throw new IllegalArgumentException("invalid_edit_range_type");
    if (value.getLong("request_id")<=0 || value.getLong("now_ns")<0 || !(value.get("text") instanceof String)
        || value.getString("text").length()>16384) throw new IllegalArgumentException("invalid_edit_request");
    return value;
  }
  private static boolean same(JSONObject a, JSONObject b) throws Exception {
    for (String key:new String[]{"generation","controller_id","request_id","observation","target","now_ns","start","end","text"})
      if (!a.get(key).equals(b.get(key))) return false;
    return true;
  }
  private void save(JSONObject value) throws Exception {
    byte[] bytes=value.toString().getBytes(StandardCharsets.UTF_8);
    FileOutputStream stream=receipt.startWrite();
    try { stream.write(bytes); receipt.finishWrite(stream); }
    catch (Exception error) { receipt.failWrite(stream); throw error; }
  }
  private static boolean unicode(String text) {
    for (int i=0;i<text.length();i++) {
      char ch=text.charAt(i);
      if (Character.isHighSurrogate(ch)) {
        if (++i>=text.length() || !Character.isLowSurrogate(text.charAt(i))) return false;
      } else if (Character.isLowSurrogate(ch)) return false;
    }
    return true;
  }
  private static boolean boundary(String text, int at) {
    return at==0 || at==text.length() || !(Character.isHighSurrogate(text.charAt(at-1)) && Character.isLowSurrogate(text.charAt(at)));
  }
  void edit(JSONObject request, ViewObservation views, String observation, String catalog, long observedTime) throws Exception {
    if (!clock.generation().equals(request.getString("generation"))) throw new IllegalStateException("generation_mismatch");
    String owner=request.getString("controller_id"); long id=request.getLong("request_id");
    if (controller!=null && !controller.equals(owner)) throw new IllegalStateException("controller_mismatch");
    if (uncertain) throw new IllegalStateException("edit_sequence_unresolved");
    if (id==lastId && accepted!=null && same(accepted,request) && completed!=null) { save(completed); return; }
    if (id!=lastId+1) throw new IllegalStateException("edit_sequence_unresolved");
    controller=owner;
    JSONObject result=new JSONObject().put("request",request).put("generation",clock.generation())
        .put("pid",android.os.Process.myPid()).put("now_ns",clock.now()).put("last_id",id);
    String error=null; EditText field=null; String before=null;
    if (!request.getString("observation").equals(observation) || request.getLong("now_ns")!=observedTime
        || clock.now()!=observedTime) error="stale_observation";
    else {
      JSONObject current=views.read(app);
      if (!current.toString().equals(catalog)) error="changed_view_state";
      else {
        android.view.View target=views.resolve(request.getString("target"));
        if (!(target instanceof EditText) || !current.getBoolean("window_focused") || !target.isFocused()
            || !ViewObservation.editable(target)) error="unsupported_target";
        else { field=(EditText)target; before=field.getText().toString(); }
      }
    }
    long start=request.getLong("start"), end=request.getLong("end"); String text=request.getString("text");
    if (error==null && !unicode(text)) error="invalid_unicode";
    if (error==null && (start<0 || end<start || end>before.length())) error="invalid_range";
    if (error==null && (!boundary(before,(int)start) || !boundary(before,(int)end))) error="split_surrogate";
    if (error==null && before.length()-(end-start)+text.length()>16384) error="text_budget_exceeded";
    if (error==null && before.length()-(end-start)+text.length()>ViewObservation.editLimit(field)) error="field_limit_exceeded";
    if (error!=null) {
      result.put("state","rejected").put("reason",error);
    } else {
      // Publish intent before invoking EditText/watchers. A thrown callback cannot authorize retry.
      uncertain=true;
      save(new JSONObject(result.toString()).put("state","pending"));
      field.getText().replace((int)start,(int)end,text);
      if (clock.now()!=request.getLong("now_ns")) throw new IllegalStateException("clock_changed_during_edit");
      result.put("state","completed").put("before",before).put("after",field.getText().toString());
    }
    save(result);
    accepted=request; completed=result; lastId=id; uncertain=false;
  }
}
