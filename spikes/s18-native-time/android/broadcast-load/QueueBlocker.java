package dev.frontcap.queue;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.Process;
import android.os.SystemClock;
import android.util.AtomicFile;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import org.json.JSONObject;

/** Explicit diagnostic workload: four independent processes, no Activity and no virtual clock. */
public class QueueBlocker extends BroadcastReceiver {
  public static class R0 extends QueueBlocker {}
  public static class R1 extends QueueBlocker {}
  public static class R2 extends QueueBlocker {}
  public static class R3 extends QueueBlocker {}

  private void save(Context context, String name, JSONObject row) throws Exception {
    AtomicFile file = new AtomicFile(new File(context.getFilesDir(),name));
    FileOutputStream stream=file.startWrite();
    try { stream.write((row.toString()+"\n").getBytes(StandardCharsets.UTF_8)); file.finishWrite(stream); }
    catch (Exception error) { file.failWrite(stream); throw error; }
  }

  @Override public void onReceive(Context context, Intent intent) {
    try {
      String token=intent.getStringExtra("token");
      if (token==null || !UUID.fromString(token).toString().equals(token)) throw new IllegalArgumentException("Invalid token");
      int hold=intent.getIntExtra("hold_ms",-1);
      if (hold!=0 && hold!=8000) throw new IllegalArgumentException("Expected warmup or bounded hold");
      String receiver=getClass().getSimpleName();
      String name=receiver+"-"+token+".json";
      if (new File(context.getFilesDir(),name).exists()) throw new IllegalStateException("Reused token");
      long started=System.nanoTime();
      JSONObject row=new JSONObject().put("token",token).put("receiver",receiver)
          .put("pid",Process.myPid()).put("tid",Process.myTid()).put("hold_ms",hold)
          .put("started_monotonic_ns",started).put("state","holding");
      save(context,name,row);
      while (System.nanoTime()-started < hold*1000000L) SystemClock.sleep(10);
      save(context,name,row.put("state","finished").put("finished_monotonic_ns",System.nanoTime()));
      setResultCode(-1);
    } catch (Exception error) { throw new IllegalStateException("Broadcast workload failed",error); }
  }
}
