package dev.frontcap.ime;
import android.inputmethodservice.InputMethodService;
import android.view.View;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import java.io.FileWriter;
public final class FrameIme extends InputMethodService {
  private final Handler handler=new Handler(Looper.getMainLooper());
  private View panel;
  private int height=126;
  private void event(String phase) {
    try (FileWriter out=new FileWriter(new java.io.File(getFilesDir(),"events.jsonl"),true)) {
      out.write("{\"phase\":\""+phase+"\",\"height\":"+height+",\"uptime_ms\":"+SystemClock.uptimeMillis()+"}\n");
    } catch (Exception error) { android.util.Log.e("FrameIme","record",error); }
  }
  @Override public View onCreateInputView() {
    panel=new View(this) {
      @Override protected void onMeasure(int widthSpec,int heightSpec) {
        setMeasuredDimension(View.MeasureSpec.getSize(widthSpec),height);
      }
    };
    panel.setBackgroundColor(0xffdd7733);
    return panel;
  }
  @Override public void onStartInputView(android.view.inputmethod.EditorInfo info,boolean restarting) {
    super.onStartInputView(info,restarting);
    handler.removeCallbacksAndMessages(null);
    height=126;panel.requestLayout();event("small-request");
    handler.postDelayed(() -> {height=883;panel.requestLayout();event("large-request");},500);
  }
  @Override public boolean onEvaluateFullscreenMode() { return false; }
  @Override public void onDestroy() { handler.removeCallbacksAndMessages(null);super.onDestroy(); }
}
