package dev.frontcap.s18;

import android.animation.Animator;
import android.animation.AnimatorListenerAdapter;
import android.animation.ValueAnimator;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.view.Choreographer;

// S18: what an ART process reads and when its timers fire, run through app_process. The host stamps
// each stdout line with its own clock; logcat stamps come from the process and are virtual under vclock.
public class Clocks {
  static int frames;
  static long firstFrame = -1, lastFrame;
  static boolean clockObservationEnabled, observationsRequested;
  static long[] beforeArm, afterArm, loopEntry;

  static final class TimerObservation {
    final String name;
    volatile long[] beforeRegistration, afterRegistration, entry, beforeOutput, afterOutput;
    TimerObservation(String name) { this.name = name; }
    void registering() { beforeRegistration = observeClock(); }
    void registered() { afterRegistration = observeClock(); }
    void entered() { entry = observeClock(); }
    void output(String message) {
      beforeOutput = observeClock();
      out(message);
      afterOutput = observeClock();
    }
    void report() {
      if (!observationsRequested) return;
      // Defer formatting/output until exit; do not print diagnostics between timer callbacks.
      long[][] samples = {beforeArm, afterArm, beforeRegistration, afterRegistration,
                         loopEntry, entry, beforeOutput, afterOutput};
      out("TIMER_OBSERVATION {\"pid\":" + android.os.Process.myPid() + ",\"timer\":\"" + name
          + "\",\"delay_ms\":5000,\"columns\":[\"monotonic_before_ns\",\"thread_cpu_ns\",\"monotonic_after_ns\",\"tid\"]"
          + ",\"phases\":[\"before-arm-output\",\"after-arm-output\",\"before-registration\",\"after-registration\","
          + "\"loop-entry\",\"callback-entry\",\"before-output\",\"after-output\"]"
          + ",\"samples\":" + java.util.Arrays.deepToString(samples) + "}");
    }
  }
  static native long[] clockObservation();

  static long[] observeClock() {
    if (!clockObservationEnabled) return null;
    try { return clockObservation(); }
    catch (LinkageError | RuntimeException error) {
      System.err.println("CLOCK_OBSERVATION_ERROR " + error);
      clockObservationEnabled = false;
      return null;
    }
  }

  static void out(String s) { System.out.println(s); System.out.flush(); }

  public static void main(String[] args) {
    Looper.prepareMainLooper();
    observationsRequested = "1".equals(System.getenv("FC_CLOCK_OBSERVATIONS"));
    if (observationsRequested) {
      try { System.load("/data/local/tmp/libclock_observation.so"); clockObservationEnabled = true; }
      catch (LinkageError error) { System.err.println("CLOCK_OBSERVATION_ERROR " + error); }
    }
    long[][] marks = new long[8][];
    marks[0] = observeClock();
    out("READS_START");
    marks[1] = observeClock();
    marks[2] = observeClock();
    long u0 = SystemClock.uptimeMillis(), e0 = SystemClock.elapsedRealtime(), n0 = System.nanoTime(), w0 = System.currentTimeMillis();
    marks[3] = observeClock();
    try { Thread.sleep(200); } catch (InterruptedException x) { }
    marks[4] = observeClock();
    long uptime = SystemClock.uptimeMillis() - u0, elapsed = SystemClock.elapsedRealtime() - e0,
        nano = System.nanoTime() - n0, wall = System.currentTimeMillis() - w0;
    marks[5] = observeClock();
    // Stamp the sampling boundary before formatting/reporting can delay stdout delivery.
    marks[6] = observeClock();
    out("READS_END");
    marks[7] = observeClock();
    out(String.format("READS after Thread.sleep(200): uptimeMillis=%d elapsedRealtime=%d nanoTime=%.1f currentTimeMillis=%d (ms)",
        uptime, elapsed, nano / 1e6, wall));
    if (marks[0] != null) out("CLOCK_OBSERVATION {\"pid\":" + android.os.Process.myPid()
        + ",\"columns\":[\"monotonic_before_ns\",\"thread_cpu_ns\",\"monotonic_after_ns\",\"tid\"]"
        + ",\"phases\":[\"before-start\",\"after-start\",\"before-initial\",\"after-initial\","
        + "\"before-final\",\"after-final\",\"before-end\",\"after-end\"]"
        + ",\"samples\":" + java.util.Arrays.deepToString(marks) + "}");
    TimerObservation handler = new TimerObservation("Handler"), timer = new TimerObservation("Timer"),
        animator = new TimerObservation("ValueAnimator");
    beforeArm = observeClock();
    out("ARM 5000 ms timers");
    afterArm = observeClock();
    Handler main = new Handler(Looper.getMainLooper());
    handler.registering();
    main.postDelayed(() -> { handler.entered(); handler.output("FIRE Handler.postDelayed"); }, 5000);
    handler.registered();
    java.util.Timer scheduled = new java.util.Timer(true);
    timer.registering();
    scheduled.schedule(new java.util.TimerTask() {
      public void run() { timer.entered(); timer.output("FIRE java.util.Timer"); }
    }, 5000);
    timer.registered();
    ValueAnimator a = ValueAnimator.ofFloat(0, 1);
    a.setDuration(5000);
    a.addListener(new AnimatorListenerAdapter() {
      public void onAnimationEnd(Animator x) { animator.entered(); animator.output("FIRE ValueAnimator end; frames=" + frames + " frame time span ms=" + (lastFrame - firstFrame) / 1000000); }
    });
    animator.registering();
    a.start();
    animator.registered();
    Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
      public void doFrame(long t) {
        if (firstFrame < 0) firstFrame = t;
        lastFrame = t;
        frames++;
        Choreographer.getInstance().postFrameCallback(this);
      }
    });
    main.postDelayed(() -> {
      out("EXIT at 6000 ms");
      handler.report(); timer.report(); animator.report();
      System.exit(0);
    }, 6000);
    loopEntry = observeClock();
    Looper.loop();
  }
}
