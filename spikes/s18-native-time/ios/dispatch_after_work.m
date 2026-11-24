// Native scheduling remains intact; due work must count before a suspended queue enters it.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*raw_advance)(uint64_t);
static void (*activity)(struct fc_dispatch_activity *);
static _Atomic int calls, children;
static dispatch_queue_t descendant;
#include "probe_assert.h"
#include "probe_timing.h"
static void callback(void *context) {
  require(context == &calls);
  require(dispatch_get_specific(&calls) == &calls);
  atomic_fetch_add(&calls, 1);
  dispatch_async(descendant, ^{ atomic_fetch_add(&children, 1); });
}
static dispatch_time_t native_deadline(int clock, int64_t delay) {
  if (clock == 0) return dispatch_time(DISPATCH_TIME_NOW, delay);
  if (clock == 1) return dispatch_time(1ull << 63, delay);
  if (clock == 2 || clock == 5) return dispatch_walltime(NULL, delay);
  if (clock == 6) return dispatch_walltime(NULL, -NSEC_PER_SEC);
  if (clock == 3) return DISPATCH_TIME_NOW;
  return dispatch_time(DISPATCH_TIME_NOW, -NSEC_PER_SEC);
}
struct early_invocation {
  void *descendant;
  int hold, self_rejected;
  _Atomic int calls, children, entered, released;
};
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static void submit_child(struct early_invocation *context) {
  context->self_rejected = idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  if (context->descendant) dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{
    atomic_fetch_add(&context->children, 1);
  });
}
static void hold_early(struct early_invocation *context) {
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  submit_child(context);
}
static void early_callback(void *raw) {
  struct early_invocation *context = raw;
  atomic_fetch_add(&context->calls, 1);
  if (context->hold) hold_early(context);
  else submit_child(context);
}
@interface DelayedActivationCapture : NSObject {
@public
  struct early_invocation *context;
}
- (void)invoke;
@end
@implementation DelayedActivationCapture
- (void)invoke { atomic_fetch_add(&context->calls, 1); }
- (void)dealloc { hold_early(context); }
@end
static struct fc_dispatch_activity read_activity(void) {
  struct fc_dispatch_activity value; activity(&value); return value;
}
static int blocked(uint64_t before) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == before;
}
static void activate_tracking(void) {
  struct early_invocation contexts[19] = {0};
  dispatch_queue_t queues[19];
  dispatch_queue_t children = dispatch_queue_create("early-after-children", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(children);
  for (int i = 0; i < 19; i++) {
    queues[i] = (i == 4 || i == 5 || i == 14 || i == 15)
      ? dispatch_queue_create_with_target("early-after-wall", DISPATCH_QUEUE_SERIAL,
                                          dispatch_workloop_create("early-wall"))
      : dispatch_queue_create("early-after", DISPATCH_QUEUE_SERIAL);
    if (i < 18) dispatch_suspend(queues[i]);
    struct early_invocation *context = &contexts[i];
    if (i < 18) context->descendant = (__bridge void *)children;
    context->hold = i == 16;
    dispatch_time_t when = i < 10 ? native_deadline(i / 2, NSEC_PER_SEC) :
      i < 16 ? native_deadline((i - 10) / 2, 2 * NSEC_PER_SEC) : DISPATCH_TIME_NOW;
    if (i == 17) @autoreleasepool {
      DelayedActivationCapture *capture = [DelayedActivationCapture new]; capture->context = context;
      dispatch_after(when, queues[i], ^{ [capture invoke]; });
    }
    else if (i % 2 || i >= 16) dispatch_after_f(when, queues[i], context, early_callback);
    else dispatch_after(when, queues[i], ^{ early_callback(context); });
  }
  dispatch_resume(queues[16]); dispatch_resume(queues[17]);
  for (int i = 16; i < 18; i++) await_value(&contexts[i].entered, 1);
  await_value(&contexts[18].calls, 1); dispatch_sync(queues[18], ^{});
  raw_advance(NSEC_PER_SEC);
  uint64_t before = now(), inactive = read_activity().pending;
  require(!start());
  uint64_t adopted = read_activity().pending;
  int repeated = start() == EALREADY && read_activity().pending == adopted;
  int held = blocked(before);
  for (int i = 16; i < 18; i++) {
    atomic_store(&contexts[i].released, 1);
    dispatch_sync(queues[i], ^{});
  }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t queued_pending = read_activity().pending;
  int queued_held = blocked(before);
  for (int i = 0; i < 16; i++) queued_held &= !atomic_load(&contexts[i].calls);
  for (int i = 0; i < 10; i++) {
    dispatch_resume(queues[i]); await_value(&contexts[i].calls, 1); dispatch_sync(queues[i], ^{});
  }
  uint64_t children_pending = read_activity().pending;
  int children_held = blocked(before);
  dispatch_resume(children);
  int future_idle = !idle(2000000000) && !read_activity().pending;
  dispatch_suspend(children);
  require(!advance(NSEC_PER_SEC, 2000000000));
  uint64_t future_pending = read_activity().pending;
  int future_held = blocked(now());
  for (int i = 10; i < 16; i++) future_held &= !atomic_load(&contexts[i].calls);
  for (int i = 10; i < 16; i++) {
    dispatch_resume(queues[i]); await_value(&contexts[i].calls, 1); dispatch_sync(queues[i], ^{});
  }
  uint64_t future_children = read_activity().pending;
  int future_children_held = blocked(now());
  dispatch_resume(children); require(!idle(2000000000));
  struct fc_dispatch_activity finished = read_activity();
  int calls = 0, child_calls = 0, self_rejected = 1, delivered_once = 1;
  for (int i = 0; i < 19; i++) {
    calls += atomic_load(&contexts[i].calls); child_calls += atomic_load(&contexts[i].children);
    delivered_once &= atomic_load(&contexts[i].calls) == 1 && atomic_load(&contexts[i].children) == (i < 18);
    if (i < 18) self_rejected &= contexts[i].self_rejected;
  }
  int balanced = !finished.pending && finished.submitted == finished.completed && now() == before + NSEC_PER_SEC;
  int passed = !inactive && adopted == 12 && repeated && held && queued_pending == 10 && queued_held &&
    children_pending == 10 && children_held && future_idle && future_pending == 6 && future_held &&
    future_children == 6 && future_children_held && calls == 19 && child_calls == 18 && self_rejected && delivered_once && balanced;
  FC_PROBE_TIMING("report");
  printf("{\"phase\":\"activation\",\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
    "\"queued_pending\":%llu,\"queued_held\":%s,\"children_pending\":%llu,\"children_held\":%s,"
    "\"future_idle\":%s,\"future_pending\":%llu,\"future_held\":%s,\"future_children\":%llu,\"future_children_held\":%s,"
    "\"calls\":%d,\"children\":%d,\"self_rejected\":%s,\"delivered_once\":%s,\"balanced\":%s,\"passed\":%s}\n",
    inactive, adopted, repeated ? "true" : "false", held ? "true" : "false", queued_pending,
    queued_held ? "true" : "false", children_pending, children_held ? "true" : "false", future_idle ? "true" : "false",
    future_pending, future_held ? "true" : "false", future_children, future_children_held ? "true" : "false",
    calls, child_calls, self_rejected ? "true" : "false", delivered_once ? "true" : "false",
    balanced ? "true" : "false", passed ? "true" : "false");
  require(passed);
}
static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  @autoreleasepool {
    activate_tracking();
    const char *clocks[] = {"uptime", "continuous", "wall", "now", "past", "workloop_wall", "workloop_wall_past"};
    for (int clock = 0; clock < 7; clock++) for (int function = 0; function < 2; function++) {
      dispatch_queue_t queue = clock >= 5
        ? dispatch_queue_create_with_target("after-admission", DISPATCH_QUEUE_SERIAL,
                                            dispatch_workloop_create("after-wall"))
        : dispatch_queue_create("after-admission", DISPATCH_QUEUE_SERIAL);
      dispatch_queue_set_specific(queue, &calls, &calls, NULL);
      descendant = dispatch_queue_create("after-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(queue); dispatch_suspend(descendant);
      atomic_store(&calls, 0); atomic_store(&children, 0);
      dispatch_time_t when = native_deadline(clock, NSEC_PER_SEC);
      uint64_t before = now();
      if (function) dispatch_after_f(when, queue, &calls, callback);
      else dispatch_after(when, queue, ^{ callback(&calls); });
      struct fc_dispatch_activity pending, child, finished;
      int future_idle = 1, before_due = 1;
      if (clock < 3 || clock == 5) {
        future_idle = idle(0) == 0;
        require(!advance(NSEC_PER_SEC - NSEC_PER_MSEC, 0));
        activity(&pending);
        before_due = pending.pending == 0 && atomic_load(&calls) == 0;
        require(!advance(NSEC_PER_MSEC, 0));
        before += NSEC_PER_SEC;
      }
      activity(&pending);
      int blocked = idle(0) == ETIMEDOUT && advance(1, 0) == ETIMEDOUT;
      int unchanged = now() == before;
      int not_entered = atomic_load(&calls) == 0;
      dispatch_resume(queue);
      // A synchronous fence may run before a timer is enqueued, so wait for its child submission.
      for (int i = 0; i < 2000; i++) {
        activity(&child);
        if (atomic_load(&calls) && child.submitted > pending.submitted) break;
        struct timespec delay = {0, 1000000}; nanosleep(&delay, NULL);
      }
      dispatch_sync(queue, ^{});
      activity(&child);
      int child_blocked = child.pending == 1 && advance(1, 0) == ETIMEDOUT;
      dispatch_resume(descendant);
      require(!idle(2000000000));
      activity(&finished);
      int passed = future_idle && before_due && pending.pending == 1 && blocked && unchanged &&
        not_entered && child_blocked && atomic_load(&calls) == 1 && atomic_load(&children) == 1 &&
        !finished.pending && finished.submitted == finished.completed && now() == before;
      FC_PROBE_TIMING("report");
      printf("{\"clock\":\"%s\",\"function\":%s,\"future_idle\":%s,\"before_due\":%s,"
        "\"due_pending\":%llu,\"blocked\":%s,\"unchanged\":%s,\"not_entered\":%s,"
        "\"child_blocked\":%s,\"passed\":%s}\n", clocks[clock], function ? "true" : "false",
        future_idle ? "true" : "false", before_due ? "true" : "false", pending.pending,
        blocked ? "true" : "false", unchanged ? "true" : "false", not_entered ? "true" : "false",
        child_blocked ? "true" : "false", passed ? "true" : "false");
      require(passed);
    }
  }
  FC_PROBE_TIMING("control-return");
  return NULL;
}
int main(void) {
  FC_PROBE_TIMING("main-entry");
  setvbuf(stdout, NULL, _IONBF, 0);
  start = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
  idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
  advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
  activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
  now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
  raw_advance = dlsym(RTLD_DEFAULT, "fc_advance_ns");
  require(start && idle && advance && activity && now && raw_advance);
  pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL));
  require(!pthread_join(thread, NULL));
  FC_PROBE_TIMING("main-return");
}
