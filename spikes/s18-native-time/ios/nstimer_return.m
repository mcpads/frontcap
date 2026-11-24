// Completion includes Foundation's timer rearm/invalidation after the user body.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <poll.h>
#include <unistd.h>
#include <errno.h>
#include <limits.h>
#include <math.h>

static int (*start_tracking)(void), (*wait_idle)(uint64_t), (*advance)(uint64_t,uint64_t);
static void (*read_activity)(struct fc_dispatch_activity *);
static uint64_t (*now_ns)(void);
static int at_return[2], release_return[2], loop_exited[2];
static _Atomic int calls, armed, valid_after_return;
static _Atomic double next_interval;
static NSTimer *timer;
#include "probe_assert.h"
static void send_signal(int fd) { require(write(fd, "t", 1) == 1); }
static void receive_signal(int fd) {
  struct pollfd descriptor = {.fd = fd, .events = POLLIN}; char value;
  require(poll(&descriptor, 1, 5000) == 1 && read(fd, &value, 1) == 1);
}
static void *control(void *unused) {
  @autoreleasepool {
    for (int trial = 0; trial < 3; trial++) {
      dispatch_sync(dispatch_get_main_queue(), ^{
        timer = [NSTimer scheduledTimerWithTimeInterval:1 repeats:trial == 0 block:^(NSTimer *value) {
          atomic_fetch_add(&calls, 1);
          atomic_store(&armed, 1);
          if (trial == 2) CFRunLoopStop(CFRunLoopGetMain());
        }];
      });
      require(!advance(1000000000, 2000000000));
      receive_signal(at_return[0]);
      struct fc_dispatch_activity state; read_activity(&state);
      uint64_t instant = now_ns();
      int blocked = wait_idle(0);
      int blocked_advance = advance(1000000000, 20000000);
      int unchanged = now_ns() == instant;
      send_signal(release_return[1]);
      int complete = wait_idle(2000000000);
      struct fc_dispatch_activity finished; read_activity(&finished);
      if (trial == 2) receive_signal(loop_exited[0]);
      int native_state = trial == 0
        ? atomic_load(&valid_after_return) && fabs(atomic_load(&next_interval) - 1) < .000001
        : !atomic_load(&valid_after_return);
      printf("{\"trial\":%d,\"native_state\":%s,\"pending\":%llu,\"blocked\":%s,"
             "\"advance_blocked\":%s,\"unchanged\":%s,\"complete\":%s,\"remaining\":%llu,\"calls\":%d}\n",
        trial, native_state ? "true" : "false", state.pending,
        blocked == ETIMEDOUT ? "true" : "false", blocked_advance == ETIMEDOUT ? "true" : "false",
        unchanged ? "true" : "false", complete == 0 ? "true" : "false", finished.pending, atomic_load(&calls));
      require(!complete && state.pending == 1 && blocked == ETIMEDOUT &&
              blocked_advance == ETIMEDOUT && unchanged && native_state && !finished.pending &&
              atomic_load(&calls) == trial + 1);
      if (trial != 2) dispatch_sync(dispatch_get_main_queue(), ^{ [timer invalidate]; timer = nil; });
    }
  }
  return NULL;
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start_tracking = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    now_ns = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(start_tracking && wait_idle && advance && read_activity && now_ns);
    require(!pipe(at_return) && !pipe(release_return) && !pipe(loop_exited) && !start_tracking());
    __block int direct_calls = 0;
    NSTimer *direct = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { direct_calls++; }];
    [direct fire]; [direct invalidate];
    int caught = 0;
    NSTimer *throwing = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) {
      @throw [NSException exceptionWithName:@"FixtureException" reason:nil userInfo:nil];
    }];
    @try { [throwing fire]; }
    @catch (NSException *exception) { caught = [exception.name isEqualToString:@"FixtureException"]; }
    [throwing invalidate];
    struct fc_dispatch_activity state; read_activity(&state);
    printf("{\"direct_calls\":%d,\"caught\":%s,\"pending\":%llu}\n", direct_calls, caught ? "true" : "false", state.pending);
    // This deliberately earlier observer holds the post-native phase before the
    // runtime's completion acknowledgment; no scheduling sleep can fake this boundary.
    CFRunLoopObserverRef gate = CFRunLoopObserverCreateWithHandler(NULL,
      kCFRunLoopBeforeSources | kCFRunLoopBeforeWaiting | kCFRunLoopExit, true, LONG_MIN,
      ^(CFRunLoopObserverRef observer, CFRunLoopActivity phase) {
        if (!atomic_exchange(&armed, 0)) return;
        atomic_store(&valid_after_return, timer.valid);
        atomic_store(&next_interval, timer.fireDate.timeIntervalSinceNow);
        send_signal(at_return[1]); receive_signal(release_return[0]);
      });
    require(gate != NULL);
    CFRunLoopAddObserver(CFRunLoopGetMain(), gate, kCFRunLoopDefaultMode);
    pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL));
    CFRunLoopRun(); send_signal(loop_exited[1]); require(!pthread_join(thread, NULL));
    CFRunLoopRemoveObserver(CFRunLoopGetMain(), gate, kCFRunLoopDefaultMode); CFRelease(gate);
    timer = nil;
    for (int i = 0; i < 2; i++) { close(at_return[i]); close(release_return[i]); close(loop_exited[i]); }
  }
  return 0;
}
