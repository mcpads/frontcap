// Native timer boundaries with explicit callback acknowledgments. Real sleeps challenge frozen
// behavior only; successful delivery is determined by a pipe, never by sleeping long enough.
#import <Foundation/Foundation.h>
#import <QuartzCore/QuartzCore.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>
#include <poll.h>
#include "dispatch_activity.h"

static int (*advance_when_idle)(uint64_t, uint64_t);
static void advance(uint64_t delta) {
  if (advance_when_idle(delta, 2000000000)) {
    fputs("dispatch work prevented clock advance\n", stderr); _Exit(8);
  }
}
static uint64_t (*changes)(void), (*wakeups)(void);
static int (*start_tracking)(void), (*wait_idle)(uint64_t);
static _Atomic int descendants;
static _Atomic int fired[9];
static int acknowledgments[2], acknowledged;
static pthread_mutex_t output_lock = PTHREAD_MUTEX_INITIALIZER;
static CFTimeInterval start;
static CFAbsoluteTime wall_start;
static useconds_t hold_us;
static dispatch_queue_t queue, wall_queue;
static dispatch_source_t repeating_source;
static NSTimer *cancelled_timer;
static dispatch_source_t cancelled_source;

static void checkpoint(const char *name) {
  pthread_mutex_lock(&output_lock);
  printf("{\"checkpoint\":\"%s\",\"monotonic_s\":%.6f,\"wall_s\":%.6f,\"fired\":[%d,%d,%d,%d],\"extra\":[%d,%d,%d,%d,%d],\"descendants\":%d}\n",
         name, CACurrentMediaTime() - start, CFAbsoluteTimeGetCurrent() - wall_start,
         atomic_load(&fired[0]), atomic_load(&fired[1]), atomic_load(&fired[2]), atomic_load(&fired[3]),
         atomic_load(&fired[4]), atomic_load(&fired[5]), atomic_load(&fired[6]),
         atomic_load(&fired[7]), atomic_load(&fired[8]), atomic_load(&descendants));
  pthread_mutex_unlock(&output_lock);
}

static void fire(int index) {
  pthread_mutex_lock(&output_lock);
  atomic_fetch_add(&fired[index], 1);
  printf("{\"timer\":%d,\"at_s\":%.6f}\n", index, CACurrentMediaTime() - start);
  pthread_mutex_unlock(&output_lock);
  char id = (char)index;
  if (write(acknowledgments[1], &id, 1) != 1) _Exit(4);
}

static void await_callbacks(int total) {
  while (acknowledged < total) {
    struct pollfd fd = {.fd = acknowledgments[0], .events = POLLIN};
    if (poll(&fd, 1, 2000) != 1) {
      fprintf(stderr, "callback acknowledgment timeout: received %d, expected %d\n", acknowledged, total);
      _Exit(5);
    }
    char id;
    if (read(acknowledgments[0], &id, 1) != 1) _Exit(6);
    acknowledged++;
  }
  // These are the fixture's callback queues, not a process-wide ready-task barrier.
  dispatch_sync(dispatch_get_main_queue(), ^{});
  dispatch_sync(queue, ^{});
  if (wall_queue != queue) dispatch_sync(wall_queue, ^{});
  if (wait_idle(2000000000)) { fputs("dispatch work did not finish\n", stderr); _Exit(7); }
}

static void install_repeating_and_cancelled(void) {
  dispatch_sync(dispatch_get_main_queue(), ^{
    [NSTimer scheduledTimerWithTimeInterval:1 repeats:YES block:^(NSTimer *timer) {
      if (atomic_load(&fired[5]) == 2) [timer invalidate];
      fire(5);
    }];
    cancelled_timer = [NSTimer scheduledTimerWithTimeInterval:2 repeats:NO block:^(NSTimer *timer) { fire(7); }];
  });
  repeating_source = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
  dispatch_source_set_timer(repeating_source, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), NSEC_PER_SEC, 0);
  dispatch_source_set_event_handler(repeating_source, ^{
    if (atomic_load(&fired[6]) == 2) dispatch_source_cancel(repeating_source);
    fire(6);
  });
  dispatch_resume(repeating_source);
  cancelled_source = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
  dispatch_source_set_timer(cancelled_source, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), DISPATCH_TIME_FOREVER, 0);
  dispatch_source_set_event_handler(cancelled_source, ^{ fire(8); });
  dispatch_resume(cancelled_source);
}

static void *control(void *unused) {
  @autoreleasepool {
    // Allow fixture setup to reach the run loop, then measure a separate real-time frozen interval.
    usleep(hold_us);
    uint64_t initial_changes = changes(), initial_wakeups = wakeups();
    usleep(hold_us);
    printf("{\"idle\":true,\"timer_changes\":%llu,\"timer_wakeups\":%llu}\n",
           (unsigned long long)(changes() - initial_changes), (unsigned long long)(wakeups() - initial_wakeups));
    checkpoint("frozen");
    advance(4999000000ull);
    usleep(hold_us);
    checkpoint("before_deadline");
    advance(1000000ull);
    await_callbacks(4);
    checkpoint("at_deadline");
    install_repeating_and_cancelled();
    advance(1000000000ull);
    await_callbacks(7);
    checkpoint("after_deadline");
    dispatch_sync(dispatch_get_main_queue(), ^{ [cancelled_timer invalidate]; });
    dispatch_source_cancel(cancelled_source);
    dispatch_sync(queue, ^{});
    advance(1000000000ull);
    await_callbacks(9);
    checkpoint("repeated");
    advance(1000000000ull);
    await_callbacks(11);
    checkpoint("last_repeat");
    advance(1000000000ull);
    usleep(hold_us); // A challenge interval for cancelled sources and invalidated repeaters.
    checkpoint("cancelled");
    if (getenv("FC_WORKLOOP")) {
      uint64_t (*registrations)(void) = dlsym(RTLD_DEFAULT, "fc_workloop_timer_changes");
      uint64_t (*deliveries)(void) = dlsym(RTLD_DEFAULT, "fc_workloop_timer_wakeups");
      if (!registrations || !deliveries || !registrations() || !deliveries()) _Exit(9);
      printf("{\"workloop\":true,\"registrations\":%llu,\"wakeups\":%llu}\n", registrations(), deliveries());
    }
    printf("{\"acknowledged\":%d}\n", acknowledged);
    CFRunLoopStop(CFRunLoopGetMain());
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    advance_when_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    changes = dlsym(RTLD_DEFAULT, "fc_timer_changes");
    wakeups = dlsym(RTLD_DEFAULT, "fc_timer_wakeups");
    start_tracking = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    if (!advance_when_idle || !changes || !wakeups || !start_tracking || !wait_idle || !getenv("FC_MANUAL")) return 2;
    if (start_tracking()) return 2;
    if (pipe(acknowledgments)) return 3;
    hold_us = getenv("FC_HOLD_US") ? (useconds_t)atoi(getenv("FC_HOLD_US")) : 200000;
    if (hold_us < 50000 || hold_us > 2000000) return 2;
    start = CACurrentMediaTime();
    wall_start = CFAbsoluteTimeGetCurrent();
    queue = getenv("FC_WORKLOOP")
      ? dispatch_queue_create_with_target("s18-step", DISPATCH_QUEUE_SERIAL, dispatch_workloop_create("s18-timers"))
      : dispatch_queue_create("s18-step", 0);
    wall_queue = queue;
    [NSTimer scheduledTimerWithTimeInterval:5 repeats:NO block:^(NSTimer *t) {
      [NSTimer scheduledTimerWithTimeInterval:1 repeats:NO block:^(NSTimer *next) { fire(4); }];
      dispatch_async(queue, ^{
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^{
          dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&descendants, 1); });
        });
      });
      fire(0);
    }];
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{ fire(1); });
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC), queue, ^{ fire(2); });
    dispatch_after(dispatch_walltime(NULL, 5 * NSEC_PER_SEC), wall_queue, ^{ fire(3); });
    pthread_t controller;
    if (pthread_create(&controller, NULL, control, NULL)) return 3;
    CFRunLoopRun();
    pthread_join(controller, NULL);
    close(acknowledgments[0]); close(acknowledgments[1]);
  }
  return 0;
}
