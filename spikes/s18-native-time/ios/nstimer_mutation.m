// A native mutation can call application code before it finishes. Time must wait.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <poll.h>
#include <unistd.h>
#include <errno.h>
#include <math.h>
static int (*start_tracking)(void), (*wait_idle)(uint64_t), (*advance)(uint64_t,uint64_t);
static void (*read_activity)(struct fc_dispatch_activity *);
static uint64_t (*now_ns)(void);
static int entered[2], release_work[2];
static _Atomic int armed, callouts, self_rejected;
static NSTimer *timer;
static NSDate *date;
#include "probe_assert.h"
static void send_signal(int fd) { require(write(fd, "m", 1) == 1); }
static void receive_signal(int fd) {
  struct pollfd p = {.fd = fd, .events = POLLIN}; char value;
  require(poll(&p, 1, 5000) == 1 && read(fd, &value, 1) == 1);
}
static void application_callout(void) {
  if (!atomic_exchange(&armed, 0)) return;
  struct fc_dispatch_activity state;
  read_activity(&state); // Reentry must not encounter the activity mutex held by native code.
  atomic_fetch_add(&callouts, 1);
  atomic_store(&self_rejected, wait_idle(0) == EDEADLK);
  send_signal(entered[1]); receive_signal(release_work[0]);
}
@interface MutationTarget : NSObject
- (void)fire:(NSTimer *)timer;
@end
@implementation MutationTarget
- (void)fire:(NSTimer *)timer { _Exit(4); }
- (void)dealloc { application_callout(); }
@end
@interface MutationDate : NSDate
@property(nonatomic) NSTimeInterval instant;
@end
@implementation MutationDate
- (instancetype)initWithTimeIntervalSinceReferenceDate:(NSTimeInterval)instant {
  self.instant = instant; return self;
}
- (NSTimeInterval)timeIntervalSinceReferenceDate { application_callout(); return self.instant; }
@end
static void *mutate(void *context) {
  @autoreleasepool {
    switch ((intptr_t)context) {
      case 0: [timer invalidate]; break;
      case 1: CFRunLoopTimerInvalidate((__bridge CFRunLoopTimerRef)timer); break;
      case 2: timer.fireDate = date; break;
    }
  }
  return NULL;
}
static void *control(void *unused) {
  @autoreleasepool {
    const char *names[] = {"objc_invalidate", "cf_invalidate", "fire_date"};
    for (int trial = 0; trial < 3; trial++) {
      NSTimeInterval desired = CFAbsoluteTimeGetCurrent() + 2;
      @autoreleasepool {
        if (trial < 2) {
          MutationTarget *target = [MutationTarget new];
          timer = [NSTimer timerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:YES];
        } else {
          timer = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { _Exit(4); }];
          date = [[MutationDate alloc] initWithTimeIntervalSinceReferenceDate:desired];
        }
      }
      atomic_store(&armed, 1);
      pthread_t thread; require(!pthread_create(&thread, NULL, mutate, (void *)(intptr_t)trial));
      receive_signal(entered[0]);
      struct fc_dispatch_activity state; read_activity(&state);
      uint64_t instant = now_ns();
      int idle = wait_idle(0), step = advance(1000000000, 20000000);
      int unchanged = now_ns() == instant;
      send_signal(release_work[1]); require(!pthread_join(thread, NULL));
      int complete = wait_idle(2000000000);
      struct fc_dispatch_activity finished; read_activity(&finished);
      int native_state = trial < 2 ? !timer.valid : fabs(timer.fireDate.timeIntervalSinceReferenceDate - desired) < .000001;
      printf("{\"mutation\":\"%s\",\"pending\":%llu,\"blocked\":%s,\"advance_blocked\":%s,"
             "\"unchanged\":%s,\"self_rejected\":%s,\"complete\":%s,\"remaining\":%llu,\"native_state\":%s,\"callouts\":%d}\n",
        names[trial], state.pending, idle == ETIMEDOUT ? "true" : "false", step == ETIMEDOUT ? "true" : "false",
        unchanged ? "true" : "false", atomic_load(&self_rejected) ? "true" : "false", !complete ? "true" : "false",
        finished.pending, native_state ? "true" : "false", atomic_load(&callouts));
      require(state.pending > 0 && idle == ETIMEDOUT && step == ETIMEDOUT && unchanged &&
              atomic_load(&self_rejected) && !complete && !finished.pending && native_state);
      [timer invalidate]; timer = nil; date = nil;
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
    require(!pipe(entered) && !pipe(release_work) && !start_tracking());
    pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL)); require(!pthread_join(thread, NULL));
    for (int i = 0; i < 2; i++) { close(entered[i]); close(release_work[i]); }
  }
  return 0;
}
