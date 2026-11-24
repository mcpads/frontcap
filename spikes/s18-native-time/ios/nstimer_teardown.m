// Native loop destruction retires only that loop's observed timer memberships.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <pthread.h>
#include <dlfcn.h>
#include <stdatomic.h>
#include <unistd.h>
#include <errno.h>
static NSTimer *timer;
static CFRunLoopRef retained_loop;
static _Atomic int calls;
static int retain_loop;
static int (*idle)(uint64_t), (*advance)(uint64_t,uint64_t);
static uint64_t (*now_ns)(void);
static void (*activity)(struct fc_dispatch_activity *);
static void require_at(int ok, int line) {
  if (ok) return;
  struct fc_dispatch_activity state = {0};
  if (activity) activity(&state);
  fprintf(stderr, "teardown-failure line=%d pending=%llu submitted=%llu completed=%llu source_cancellations=%llu now=%llu\n",
    line, state.pending, state.submitted, state.completed, state.source_cancellations, now_ns ? now_ns() : 0);
  _Exit(3);
}
#define require(ok) require_at((ok), __LINE__)
static void *owner(void *unused) {
  @autoreleasepool {
    timer = [NSTimer scheduledTimerWithTimeInterval:1 repeats:YES block:^(NSTimer *value) {
      atomic_fetch_add(&calls, 1);
    }];
    if (retain_loop) retained_loop = (CFRunLoopRef)CFRetain(CFRunLoopGetCurrent());
    require(timer.valid);
  }
  return NULL;
}
static void *control(void *unused) {
  @autoreleasepool {
    const char *names[] = {"destroyed", "retained", "other_loop"};
    for (int kind = 0; kind < 3; kind++) {
      retain_loop = kind != 0;
      pthread_t thread; require(!pthread_create(&thread, NULL, owner, NULL));
      require(!pthread_join(thread, NULL));
      BOOL native_state = timer.valid;
      if (retained_loop)
        native_state &= CFRunLoopContainsTimer(retained_loop, (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
      if (kind == 2) {
        // Native timers belong to only one loop at a time; move, do not duplicate.
        CFRunLoopRemoveTimer(retained_loop, (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
        CFRunLoopAddTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
        require(CFRunLoopContainsTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode));
      }
      require(!idle(2000000000) && !advance(1000000000, 2000000000));
      struct fc_dispatch_activity state; activity(&state);
      uint64_t before = state.pending, instant = now_ns();
      BOOL blocked = kind == 0 ? !idle(0) : idle(0) == ETIMEDOUT;
      if (kind != 0) blocked &= advance(1, 20000000) == ETIMEDOUT && now_ns() == instant;
      if (retained_loop) { CFRelease(retained_loop); retained_loop = NULL; }
      activity(&state);
      uint64_t after_pending = state.pending, source_cancellations = state.source_cancellations;
      require(after_pending >= source_cancellations);
      // Loop teardown can asynchronously cancel its native sources. Keep those
      // real obligations visible, while checking timer membership independently.
      uint64_t after = after_pending - source_cancellations;
      BOOL other_loop = YES;
      if (kind == 2) {
        other_loop = CFRunLoopContainsTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode)
          && idle(0) == ETIMEDOUT && advance(1, 20000000) == ETIMEDOUT && now_ns() == instant;
        CFRunLoopRemoveTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
      }
      BOOL complete = !idle(2000000000) && !advance(1, 2000000000);
      native_state &= timer.valid;
      [timer invalidate]; timer = nil;
      activity(&state);
      BOOL balanced = !state.pending && state.submitted == state.completed;
      printf("{\"case\":\"%s\",\"before\":%llu,\"after\":%llu,\"after_pending\":%llu,\"source_cancellations\":%llu,\"blocked\":%s,\"other_loop\":%s,\"complete\":%s,\"native_state\":%s,\"balanced\":%s,\"calls\":%d}\n",
        names[kind], before, after, after_pending, source_cancellations, blocked ? "true" : "false", other_loop ? "true" : "false",
        complete ? "true" : "false", native_state ? "true" : "false", balanced ? "true" : "false", atomic_load(&calls));
      require(before == (kind != 0) && after == (kind == 2) && blocked && other_loop && complete && native_state && balanced && !atomic_load(&calls));
    }
  }
  _Exit(0);
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  int (*start)(void) = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
  idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
  advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
  activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
  now_ns = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
  require(start && idle && advance && activity && now_ns && !start());
  pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL));
  for (;;) pause();
}
