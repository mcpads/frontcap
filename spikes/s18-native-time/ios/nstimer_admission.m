// Hold the native run loop so admission cannot be confused with callback entry.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <poll.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <math.h>
static int (*start_tracking)(void), (*wait_idle)(uint64_t), (*advance)(uint64_t,uint64_t);
static void (*read_activity)(struct fc_dispatch_activity *);
static uint64_t (*now_ns)(void);
static int ready[2], commands[2], acknowledged[2], fired[2], read_paused[2], resume_read[2];
static void (*read_gate)(CFRunLoopTimerRef, CFAbsoluteTime, int, int);
static _Atomic double first_date;
static _Atomic int trial, calls;
static NSTimer *timer;
static CFRunLoopSourceRef source;
#include "probe_assert.h"
static void send_byte(int fd, char value) { require(write(fd, &value, 1) == 1); }
static char receive_byte(int fd) {
  struct pollfd p = {.fd = fd, .events = POLLIN}; char value;
  require(poll(&p, 1, 5000) == 1 && read(fd, &value, 1) == 1); return value;
}
static void *change_first_date(void *unused) {
  @autoreleasepool { timer.fireDate = [NSDate dateWithTimeIntervalSinceReferenceDate:atomic_load(&first_date)]; }
  return NULL;
}
static void fired_timer(NSTimer *value) { require(value == timer); atomic_fetch_add(&calls, 1); send_byte(fired[1], 'f'); }
@interface AdmissionTarget : NSObject
- (void)fire:(NSTimer *)value;
@end
@implementation AdmissionTarget
- (void)fire:(NSTimer *)value { fired_timer(value); }
@end
static void hold_loop(void *unused) {
  int kind = atomic_load(&trial);
  AdmissionTarget *target = [AdmissionTarget new];
  if (kind == 0) timer = [NSTimer scheduledTimerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { fired_timer(value); }];
  else if (kind == 2) timer = [NSTimer scheduledTimerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
  else if (kind == 3) timer = [NSTimer timerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
  else timer = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { fired_timer(value); }];
  if (kind != 0 && kind != 2) [NSRunLoop.mainRunLoop addTimer:timer forMode:NSDefaultRunLoopMode];
  if (kind == 6) CFRunLoopAddTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopCommonModes);
  send_byte(ready[1], 'r');
  for (;;) {
    char command = receive_byte(commands[0]);
    if (command == 'x') break;
    if (command == 'c') [timer invalidate];
    if (command == 'p') timer.fireDate = [NSDate dateWithTimeIntervalSinceNow:1];
    if (command == 'r') {
      CFRunLoopRemoveTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopCommonModes);
      send_byte(acknowledged[1], CFRunLoopContainsTimer(CFRunLoopGetMain(),
        (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode) ? '1' : '0');
      continue;
    }
    if (command == 'R') CFRunLoopRemoveTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
    if (command == 'a') CFRunLoopAddTimer(CFRunLoopGetMain(), (__bridge CFRunLoopTimerRef)timer, kCFRunLoopDefaultMode);
    send_byte(acknowledged[1], 'a');
  }
}
static char command(char value) { send_byte(commands[1], value); return receive_byte(acknowledged[0]); }
static void *control(void *unused) {
  @autoreleasepool {
    @autoreleasepool {
      NSTimer *unscheduled = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { _Exit(4); }];
      require(!advance(1000000000, 2000000000));
      struct fc_dispatch_activity state; read_activity(&state);
      int idle = wait_idle(0);
      printf("{\"unscheduled\":true,\"idle\":%s,\"pending\":%llu}\n", !idle ? "true" : "false", state.pending);
      require(!idle && !state.pending); [unscheduled invalidate];
    }
    const char *names[] = {"scheduled_block", "block", "scheduled_target", "target", "invalidate", "reschedule", "remove_add", "stale_read"};
    for (int kind = 0; kind < 8; kind++) {
      atomic_store(&trial, kind); atomic_store(&calls, 0);
      CFRunLoopSourceSignal(source); CFRunLoopWakeUp(CFRunLoopGetMain()); receive_byte(ready[0]);
      int transition = 1;
      if (kind == 7) {
        double latest = CFAbsoluteTimeGetCurrent() + 3;
        atomic_store(&first_date, latest - 1);
        read_gate((__bridge CFRunLoopTimerRef)timer, atomic_load(&first_date), read_paused[1], resume_read[0]);
        pthread_t writer; require(!pthread_create(&writer, NULL, change_first_date, NULL));
        fprintf(stderr, "stale-read: awaiting metadata read\n");
        receive_byte(read_paused[0]);
        fprintf(stderr, "stale-read: older read held\n");
        timer.fireDate = [NSDate dateWithTimeIntervalSinceReferenceDate:latest];
        send_byte(resume_read[1], 'x'); require(!pthread_join(writer, NULL));
        read_gate(NULL, 0, -1, -1);
        transition = fabs(timer.fireDate.timeIntervalSinceReferenceDate - latest) < .000001;
        fprintf(stderr, "stale-read: native=%.12f latest=%.12f\n", timer.fireDate.timeIntervalSinceReferenceDate, latest);
        require(!advance(2000000000, 2000000000));
        transition &= !wait_idle(0);
      }
      int future = wait_idle(0);
      require(!advance(1000000000, 2000000000));
      struct fc_dispatch_activity due; read_activity(&due);
      uint64_t instant = now_ns();
      int blocked = wait_idle(0), rejected = advance(1000000000, 20000000);
      int unchanged = now_ns() == instant, before_calls = atomic_load(&calls);
      if (kind >= 4 && kind <= 6) {
        char membership = command(kind == 4 ? 'c' : kind == 5 ? 'p' : 'r');
        struct fc_dispatch_activity released; read_activity(&released);
        if (kind == 6) {
          transition = membership == '1' ? released.pending == 1 && wait_idle(0) == ETIMEDOUT
                                        : !released.pending && !wait_idle(0);
          command('R'); read_activity(&released);
        }
        transition &= !wait_idle(0) && !released.pending;
        if (kind == 5) {
          require(!advance(1000000000, 2000000000));
          read_activity(&released); transition &= released.pending == 1 && wait_idle(0) == ETIMEDOUT;
        }
        if (kind == 6) {
          command('a'); read_activity(&released);
          transition &= released.pending == 1 && wait_idle(0) == ETIMEDOUT;
        }
      }
      send_byte(commands[1], 'x');
      if (kind != 4) receive_byte(fired[0]);
      int complete = wait_idle(2000000000);
      // Synchronize with the source's return even when invalidation removed its timer.
      dispatch_sync(dispatch_get_main_queue(), ^{});
      struct fc_dispatch_activity finished; read_activity(&finished);
      printf("{\"case\":\"%s\",\"future_idle\":%s,\"due_pending\":%llu,\"before_calls\":%d,"
             "\"blocked\":%s,\"advance_blocked\":%s,\"unchanged\":%s,\"transition\":%s,"
             "\"complete\":%s,\"remaining\":%llu,\"calls\":%d,\"balanced\":%s}\n",
        names[kind], !future ? "true" : "false", due.pending, before_calls,
        blocked == ETIMEDOUT ? "true" : "false", rejected == ETIMEDOUT ? "true" : "false",
        unchanged ? "true" : "false", transition ? "true" : "false", !complete ? "true" : "false",
        finished.pending, atomic_load(&calls), finished.submitted == finished.completed ? "true" : "false");
      require(!future && due.pending == 1 && !before_calls && blocked == ETIMEDOUT && rejected == ETIMEDOUT &&
              unchanged && transition && !complete && !finished.pending && atomic_load(&calls) == (kind == 4 ? 0 : 1) &&
              finished.submitted == finished.completed);
      dispatch_sync(dispatch_get_main_queue(), ^{ [timer invalidate]; timer = nil; });
    }
    CFRunLoopStop(CFRunLoopGetMain());
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
    read_gate = dlsym(RTLD_DEFAULT, "fc_nstimer_read_gate");
    require(start_tracking && wait_idle && advance && read_activity && now_ns && read_gate);
    require(!pipe(ready) && !pipe(commands) && !pipe(acknowledged) && !pipe(fired) && !pipe(read_paused) && !pipe(resume_read) && !start_tracking());
    CFRunLoopSourceContext context = {.version = 0, .perform = hold_loop};
    source = CFRunLoopSourceCreate(NULL, 0, &context); require(source != NULL);
    CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
    pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL));
    CFRunLoopRun(); require(!pthread_join(thread, NULL));
    CFRunLoopRemoveSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode); CFRelease(source);
    for (int i = 0; i < 2; i++) { close(ready[i]); close(commands[i]); close(acknowledged[i]); close(fired[i]); close(read_paused[i]); close(resume_read[i]); }
  }
  return 0;
}
