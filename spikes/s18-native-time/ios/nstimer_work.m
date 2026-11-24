#import <Foundation/Foundation.h>
#import <QuartzCore/QuartzCore.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>
#include <poll.h>
#include <errno.h>

static int (*start_tracking)(void), (*wait_idle)(uint64_t);
static int (*advance_when_idle)(uint64_t, uint64_t);
static void (*read_activity)(struct fc_dispatch_activity *);
static int entered[2], release_work[2];
static _Atomic int calls, effects, identity_errors;
static NSTimer *current_timer;
static id info;
static CFTimeInterval origin;
static void signal_pipe(int fd) { FC_REQUIRE(write(fd, "t", 1) == 1, 3); }
static void await_signal(int fd) {
  struct pollfd descriptor = {.fd = fd, .events = POLLIN};
  char value;
  FC_REQUIRE(poll(&descriptor, 1, 2000) == 1 && read(fd, &value, 1) == 1, 4);
}
static void callback(NSTimer *timer, BOOL target_form) {
  if (timer != current_timer || (target_form && timer.userInfo != info)) atomic_fetch_add(&identity_errors, 1);
  int count = atomic_fetch_add(&calls, 1) + 1;
  if (count % 2 == 0) [timer invalidate];
  signal_pipe(entered[1]);
  await_signal(release_work[0]);
  dispatch_async(dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^{
    dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&effects, 1); });
  });
}
@interface TimerReceiver : NSObject
- (void)fire:(NSTimer *)timer;
@end
@implementation TimerReceiver
- (void)fire:(NSTimer *)timer { callback(timer, YES); }
@end

static void *control(void *unused) {
  @autoreleasepool {
    const char *names[] = {"scheduled_block", "block", "scheduled_target", "target"};
    for (int kind = 0; kind < 4; kind++) {
      dispatch_sync(dispatch_get_main_queue(), ^{
        TimerReceiver *target = [TimerReceiver new];
        if (kind == 0) current_timer = [NSTimer scheduledTimerWithTimeInterval:1 repeats:YES block:^(NSTimer *timer) { callback(timer, NO); }];
        if (kind == 1) current_timer = [NSTimer timerWithTimeInterval:1 repeats:YES block:^(NSTimer *timer) { callback(timer, NO); }];
        if (kind == 2) current_timer = [NSTimer scheduledTimerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:info repeats:YES];
        if (kind == 3) current_timer = [NSTimer timerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:info repeats:YES];
        if (kind % 2) [NSRunLoop.mainRunLoop addTimer:current_timer forMode:NSDefaultRunLoopMode];
      });
      for (int iteration = 0; iteration < 2; iteration++) {
        int future_idle = wait_idle(0);
        FC_REQUIRE(!advance_when_idle(NSEC_PER_SEC, 2000000000), 5);
        await_signal(entered[0]);
        struct fc_dispatch_activity state;
        read_activity(&state);
        int blocked = advance_when_idle(NSEC_PER_SEC, 20000000);
        double instant = CACurrentMediaTime() - origin;
        signal_pipe(release_work[1]);
        int complete = wait_idle(2000000000);
        printf("{\"factory\":\"%s\",\"iteration\":%d,\"future_idle\":%s,\"pending\":%llu,"
               "\"blocked\":%s,\"instant\":%.6f,\"complete\":%s,\"calls\":%d,\"effects\":%d,\"identity_errors\":%d}\n",
               names[kind], iteration + 1, future_idle == 0 ? "true" : "false", (unsigned long long)state.pending,
               blocked == ETIMEDOUT ? "true" : "false", instant, complete == 0 ? "true" : "false",
               atomic_load(&calls), atomic_load(&effects), atomic_load(&identity_errors));
        FC_REQUIRE(!complete, 5);
      }
      dispatch_sync(dispatch_get_main_queue(), ^{ current_timer = nil; });
    }
    FC_REQUIRE(!advance_when_idle(NSEC_PER_SEC, 2000000000), 5);
    usleep(200000); // Challenge invalidation; not a completion wait.
    printf("{\"cancelled_calls\":%d,\"instant\":%.6f}\n", atomic_load(&calls), CACurrentMediaTime() - origin);
    CFRunLoopStop(CFRunLoopGetMain());
  }
  return NULL;
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start_tracking = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance_when_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    if (!start_tracking || !wait_idle || !advance_when_idle || !read_activity) return 2;
    if (pipe(entered) || pipe(release_work)) return 2;
    info = [NSObject new];
    origin = CACurrentMediaTime();
    if (start_tracking()) return 2;
    pthread_t controller;
    if (pthread_create(&controller, NULL, control, NULL)) return 2;
    CFRunLoopRun();
    pthread_join(controller, NULL);
    close(entered[0]); close(entered[1]); close(release_work[0]); close(release_work[1]);
  }
  return 0;
}
