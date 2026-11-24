// S18: what a simulator process reads and when its timers fire, in real seconds.
#import <Foundation/Foundation.h>
#import <QuartzCore/QuartzCore.h>
#include <dlfcn.h>
#include <mach/mach_time.h>

static uint64_t (*real_abs)(void);
static double real_s(void) {
  static mach_timebase_info_data_t tb;
  if (!tb.denom) mach_timebase_info(&tb);
  return (double)(real_abs ? real_abs() : mach_absolute_time()) * tb.numer / tb.denom / 1e9;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    real_abs = dlsym(RTLD_DEFAULT, "fc_real_abs");
    printf("{\"vclock\": %s,\n", real_abs ? "true" : "false");
    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    NSDate *d0 = [NSDate date]; CFAbsoluteTime c0 = CFAbsoluteTimeGetCurrent(); CFTimeInterval m0 = CACurrentMediaTime();
    uint64_t a0 = mach_absolute_time(); NSTimeInterval p0 = NSProcessInfo.processInfo.systemUptime;
    dispatch_time_t t0 = dispatch_time(DISPATCH_TIME_NOW, 0);
    double r0 = real_s(); usleep(200000); double r1 = real_s();
    printf(" \"reads_after_real_s\": %.3f, \"reads_s\": {\"NSDate\": %.3f, \"CFAbsoluteTimeGetCurrent\": %.3f, "
           "\"CACurrentMediaTime\": %.3f, \"mach_absolute_time\": %.3f, \"systemUptime\": %.3f, \"dispatch_time\": %.3f},\n",
           r1 - r0, -[d0 timeIntervalSinceNow], CFAbsoluteTimeGetCurrent() - c0, CACurrentMediaTime() - m0,
           (double)(mach_absolute_time() - a0) * tb.numer / tb.denom / 1e9, NSProcessInfo.processInfo.systemUptime - p0,
           (double)(dispatch_time(DISPATCH_TIME_NOW, 0) - t0) * tb.numer / tb.denom / 1e9);
    double s0 = real_s();
    __block int fired = 0;
    void (^report)(const char *) = ^(const char *name) {
      @synchronized (NSProcessInfo.processInfo) { printf(" \"%s\": %.3f,\n", name, real_s() - s0); fired++; }
    };
    dispatch_queue_t q = getenv("FC_WORKLOOP")
      ? dispatch_queue_create_with_target("s18", DISPATCH_QUEUE_SERIAL, dispatch_workloop_create("s18-clock-timers"))
      : dispatch_queue_create("s18", 0);
    dispatch_queue_t wall_queue = getenv("FC_WORKLOOP") && !getenv("FC_WORKLOOP_WALL")
      ? dispatch_queue_create("s18-wall", 0) : q;
    printf(" \"timers_5s_fired_at_real_s\": {\n");
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{ report("dispatch_after_main"); });
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC), q, ^{ report("dispatch_after_queue"); });
    dispatch_source_t wall_source;
    if (getenv("FC_WORKLOOP_WALL_SOURCE")) {
      wall_source = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, wall_queue);
      dispatch_source_set_timer(wall_source, dispatch_walltime(NULL, 5 * NSEC_PER_SEC), DISPATCH_TIME_FOREVER, 0);
      __unsafe_unretained dispatch_source_t borrowed = wall_source;
      dispatch_source_set_event_handler(wall_source, ^{
        dispatch_source_cancel(borrowed);
        report("dispatch_source_walltime");
      });
      dispatch_resume(wall_source);
    } else {
      dispatch_after(dispatch_walltime(NULL, 5 * NSEC_PER_SEC), wall_queue, ^{ report("dispatch_after_walltime"); });
    }
    [NSTimer scheduledTimerWithTimeInterval:5 repeats:NO block:^(NSTimer *t) { report("NSTimer"); }];
    while (fired < 4 && real_s() - s0 < 8) [NSRunLoop.mainRunLoop runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
    printf(" \"fired\": %d}}\n", fired);
    if (wall_source) dispatch_source_cancel(wall_source);
  }
}
