package dev.frontcap.s18;

import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.SystemClock;
import java.util.concurrent.atomic.AtomicIntegerArray;

// Fixture callback acknowledgments establish delivery; this is not a process-wide idle detector.
public class Steps {
  public static native String futexStats();
  static native void prepare();
  static native void advance(long nanos);
  static native long advanceLimit();
  static native void hold(int millis);
  static native long realNanos();
  static native long[] clockObservation();
  static native long[] schedulerObservation();
  static native void ack();
  static native void awaitAck();
  static final AtomicIntegerArray fired = new AtomicIntegerArray(5);
  static long uptime, elapsed, nano, wall;
  static Handler main, worker;
  static void fire(int index) { fired.incrementAndGet(index); ack(); }
  static void snapshot(String name) {
    System.out.printf("{\"checkpoint\":\"%s\",\"uptime_ms\":%d,\"elapsed_ms\":%d,\"nano_ns\":%d,\"wall_ms\":%d,\"fired\":[%d,%d,%d,%d,%d]}%n",
      name, SystemClock.uptimeMillis()-uptime, SystemClock.elapsedRealtime()-elapsed,
      System.nanoTime()-nano, System.currentTimeMillis()-wall,
      fired.get(0), fired.get(1), fired.get(2), fired.get(3), fired.get(4));
    System.out.flush();
  }
  static void await(int count) {
    for (int i=0; i<count; i++) awaitAck();
    // Fence the two fixture queues after the callbacks finish their bodies.
    main.post(Steps::ack); worker.post(Steps::ack);
    awaitAck(); awaitAck();
  }
  public static void main(String[] args) {
    System.load("/data/local/tmp/libvclock.so");
    prepare();
    Looper.prepareMainLooper();
    main = new Handler(Looper.getMainLooper());
    HandlerThread background = new HandlerThread("s18-worker");
    background.start();
    worker = new Handler(background.getLooper());
    uptime = SystemClock.uptimeMillis(); elapsed = SystemClock.elapsedRealtime();
    nano = System.nanoTime(); wall = System.currentTimeMillis();
    main.postDelayed(() -> { main.postDelayed(() -> fire(2), 1000); fire(0); }, 5000);
    worker.postDelayed(() -> fire(1), 5000);
    Runnable cancelled = () -> fire(4);
    main.postDelayed(cancelled, 6000);
    main.removeCallbacks(cancelled);
    Runnable repeating = new Runnable() {
      public void run() { if (fired.get(3) < 2) worker.postDelayed(this, 1000); fire(3); }
    };
    worker.postDelayed(repeating, 6000);
    new Thread(() -> {
      int challenge = Integer.parseInt(System.getenv().getOrDefault("FC_HOLD_MS", "200"));
      hold(challenge); snapshot("frozen");
      advance(4999000000L); hold(challenge); snapshot("before_deadline");
      advance(1000000); await(2); snapshot("at_deadline");
      advance(1000000000); await(2); snapshot("chained");
      advance(1000000000); await(1); snapshot("repeated");
      advance(1000000000); await(1); snapshot("last_repeat");
      advance(1000000000); hold(challenge); snapshot("cancelled");
      background.quit();
      System.exit(0);
    }, "s18-controller").start();
    Looper.loop();
  }
}
