package dev.frontcap.s18;

import android.os.Handler;
import android.os.Looper;
import android.os.Message;
import android.os.SystemClock;
import java.util.concurrent.atomic.AtomicInteger;

/** Isolated clock-domain control, not an application or graceful-shutdown probe. */
public final class ClockHandoff {
  static final AtomicInteger ordinary=new AtomicInteger(), rendering=new AtomicInteger();
  static volatile boolean sent;
  static volatile long scheduled, delivered;
  static long origin;
  static void require(boolean value,String message) { if(!value) throw new AssertionError(message); }
  public static void main(String[] args) {
    System.load(args[0]); Steps.prepare(); Looper.prepareMainLooper();
    Handler receiver=new Handler(Looper.getMainLooper()) {
      @Override public boolean sendMessageAtTime(Message message,long uptimeMillis) {
        if(Thread.currentThread().getName().equals("InsetsAnimation")) scheduled=uptimeMillis;
        return super.sendMessageAtTime(message,uptimeMillis);
      }
    };
    origin=SystemClock.uptimeMillis();
    boolean host=System.getenv("FC_HOST_RENDER")!=null;
    new Thread(() -> {
      try {
        require(receiver.post(() -> ordinary.incrementAndGet()),"ordinary post refused");
        Thread sender=new Thread(() -> {
          Steps.hold(200);
          Message message=Message.obtain(receiver,() -> {
            delivered=SystemClock.uptimeMillis(); rendering.incrementAndGet();
          });
          require(receiver.sendMessage(message),"render post refused");
          sent=true;
        },"InsetsAnimation");
        sender.start();
        long deadline=Steps.realNanos()+5000000000L;
        while(!sent && Steps.realNanos()<deadline) Steps.hold(1);
        require(sent,"sender deadline"); sender.join(); Steps.hold(200);
        long frozen=SystemClock.uptimeMillis();
        int before=rendering.get();
        System.out.printf("{\"phase\":\"held\",\"pid\":%d,\"host_render\":%s,\"origin_ms\":%d,\"receiver_ms\":%d,\"scheduled_ms\":%d,\"ordinary\":%d,\"rendering\":%d}%n",
            android.os.Process.myPid(),host,origin,frozen,scheduled,ordinary.get(),before);
        System.out.flush();
        require(frozen==origin && ordinary.get()==1,"receiver did not remain frozen with ordinary delivery");
        require(host ? scheduled>origin && before==0 : scheduled==origin && before==1,"unexpected cross-thread deadline");
        long delta=host ? scheduled-origin+1 : 1;
        Steps.advance(delta*1000000L);
        deadline=Steps.realNanos()+2000000000L;
        while(rendering.get()!=1 && Steps.realNanos()<deadline) Steps.hold(1);
        require(rendering.get()==1,"advanced receiver did not deliver");
        System.out.printf("{\"phase\":\"advanced\",\"delta_ms\":%d,\"receiver_ms\":%d,\"delivered_ms\":%d,\"rendering\":%d}%n",
            delta,SystemClock.uptimeMillis(),delivered,rendering.get());
        System.out.flush(); System.exit(0);
      } catch(Throwable error) { error.printStackTrace(); System.exit(1); }
    },"s18-controller").start();
    Looper.loop();
  }
}
