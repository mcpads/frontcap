// Completion includes owned one-shot captures and source callback autorelease cleanup.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static dispatch_semaphore_t entered, release;
static dispatch_queue_t descendant;
static _Atomic int calls, children, self_rejected;
#include "probe_assert.h"
#include "probe_timing.h"

@interface DispatchCapture : NSObject
- (void)invoke;
@end
@implementation DispatchCapture
- (void)invoke { atomic_fetch_add(&calls, 1); }
- (void)dealloc {
  atomic_store(&self_rejected, idle(0) == EDEADLK && advance(1, 0) == EDEADLK);
  dispatch_semaphore_signal(entered);
  dispatch_semaphore_wait(release, DISPATCH_TIME_FOREVER);
  dispatch_async(descendant, ^{ atomic_fetch_add(&children, 1); });
}
@end

static void source_callback(void *unused) {
  DispatchCapture *capture = [DispatchCapture new];
  [capture invoke];
  // Explicitly enqueue a +1 object in the caller's pool without ARC return-value
  // optimization. This models an autoreleased object produced by an API call.
  void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
  require(autorelease_object != NULL);
  autorelease_object((__bridge_retained void *)capture);
}

struct early_invocation {
  void *descendant;
  int phase, self_rejected;
  _Atomic int calls, children, entered, released;
};
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static void submit_early_child(struct early_invocation *context) {
  context->self_rejected &= idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  if (context->descendant) dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{
    atomic_fetch_add(&context->children, 1);
  });
}
static void hold_early(struct early_invocation *context) {
  context->self_rejected = idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  submit_early_child(context);
}
@interface AsyncActivationCapture : NSObject {
@public
  struct early_invocation *context;
}
- (void)invoke;
@end
@implementation AsyncActivationCapture
- (void)invoke { atomic_fetch_add(&context->calls, 1); }
- (void)dealloc { hold_early(context); }
@end
static void early_callback(void *raw) {
  struct early_invocation *context = raw;
  if (context->phase == 2) {
    AsyncActivationCapture *capture = [AsyncActivationCapture new]; capture->context = context;
    [capture invoke];
    void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
    require(autorelease_object != NULL);
    autorelease_object((__bridge_retained void *)capture);
  } else {
    atomic_fetch_add(&context->calls, 1);
    if (context->phase == 1) hold_early(context);
    else { context->self_rejected = 1; submit_early_child(context); }
  }
}
static struct fc_dispatch_activity read_activity(void) {
  struct fc_dispatch_activity value; activity(&value); return value;
}
static int blocked(uint64_t before) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == before;
}
static void activate_tracking(void) {
  // Six APIs in each state: queued, entered body, entered cleanup, already completed.
  struct early_invocation contexts[24] = {0};
  dispatch_queue_t queues[24];
  dispatch_group_t group = dispatch_group_create();
  dispatch_queue_t children = dispatch_queue_create("early-async-children", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(children);
  for (int i = 0; i < 24; i++) {
    int mode = i % 6;
    queues[i] = dispatch_queue_create("early-async", dispatch_queue_attr_make_with_autorelease_frequency(
      mode / 2 == 1 ? DISPATCH_QUEUE_CONCURRENT : DISPATCH_QUEUE_SERIAL,
      DISPATCH_AUTORELEASE_FREQUENCY_WORK_ITEM));
    if (i < 18) dispatch_suspend(queues[i]);
    struct early_invocation *context = &contexts[i];
    context->phase = i / 6;
    if (i < 18) context->descendant = (__bridge void *)children;
    if (mode % 2) {
      if (mode == 1) dispatch_async_f(queues[i], context, early_callback);
      else if (mode == 3) dispatch_barrier_async_f(queues[i], context, early_callback);
      else dispatch_group_async_f(group, queues[i], context, early_callback);
    } else @autoreleasepool {
      dispatch_block_t block;
      if (context->phase == 2) {
        AsyncActivationCapture *capture = [AsyncActivationCapture new]; capture->context = context;
        block = ^{ [capture invoke]; };
      } else block = ^{ early_callback(context); };
      if (mode == 0) dispatch_async(queues[i], block);
      else if (mode == 2) dispatch_barrier_async(queues[i], block);
      else dispatch_group_async(group, queues[i], block);
    }
  }
  for (int i = 6; i < 18; i++) dispatch_resume(queues[i]);
  for (int i = 6; i < 18; i++) await_value(&contexts[i].entered, 1);
  for (int i = 18; i < 24; i++) {
    await_value(&contexts[i].calls, 1); dispatch_barrier_sync(queues[i], ^{});
  }
  uint64_t before = now(), inactive = read_activity().pending;
  require(!start());
  uint64_t adopted = read_activity().pending;
  int repeated = start() == EALREADY && read_activity().pending == adopted;
  int held = blocked(before);
  for (int i = 6; i < 12; i++) {
    atomic_store(&contexts[i].released, 1); dispatch_barrier_sync(queues[i], ^{});
  }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t cleanup_pending = read_activity().pending;
  int cleanup_held = blocked(before);
  for (int i = 12; i < 18; i++) {
    atomic_store(&contexts[i].released, 1); dispatch_barrier_sync(queues[i], ^{});
  }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t queued_pending = read_activity().pending;
  int queued_held = blocked(before);
  for (int i = 0; i < 6; i++) queued_held &= !atomic_load(&contexts[i].calls);
  for (int i = 0; i < 6; i++) {
    dispatch_resume(queues[i]); dispatch_barrier_sync(queues[i], ^{});
  }
  int group_finished = !dispatch_group_wait(group, DISPATCH_TIME_NOW);
  uint64_t children_pending = read_activity().pending;
  int children_held = blocked(before);
  dispatch_resume(children); require(!idle(2000000000));
  struct fc_dispatch_activity finished = read_activity();
  int calls = 0, child_calls = 0, self_rejected = 1, delivered_once = 1;
  for (int i = 0; i < 24; i++) {
    calls += atomic_load(&contexts[i].calls); child_calls += atomic_load(&contexts[i].children);
    delivered_once &= atomic_load(&contexts[i].calls) == 1 && atomic_load(&contexts[i].children) == (i < 18);
    if (i < 18) self_rejected &= contexts[i].self_rejected;
  }
  int balanced = !finished.pending && finished.submitted == finished.completed && now() == before;
  int passed = !inactive && adopted == 18 && repeated && held && cleanup_pending == 12 && cleanup_held &&
    queued_pending == 6 && queued_held && children_pending == 6 && children_held && group_finished &&
    calls == 24 && child_calls == 18 && self_rejected && delivered_once && balanced;
  FC_PROBE_TIMING("report");
  printf("{\"mode\":\"activation\",\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
    "\"cleanup_pending\":%llu,\"cleanup_held\":%s,\"queued_pending\":%llu,\"queued_held\":%s,"
    "\"children_pending\":%llu,\"children_held\":%s,\"group_finished\":%s,\"calls\":%d,\"children\":%d,"
    "\"self_rejected\":%s,\"delivered_once\":%s,\"balanced\":%s,\"passed\":%s}\n",
    inactive, adopted, repeated ? "true" : "false", held ? "true" : "false",
    cleanup_pending, cleanup_held ? "true" : "false", queued_pending, queued_held ? "true" : "false",
    children_pending, children_held ? "true" : "false", group_finished ? "true" : "false", calls, child_calls,
    self_rejected ? "true" : "false", delivered_once ? "true" : "false", balanced ? "true" : "false", passed ? "true" : "false");
  require(passed);
}

static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  @autoreleasepool {
    activate_tracking();
    const char *names[] = {"async", "barrier", "group", "after", "event_block", "event_function", "cancel_block", "cancel_function",
                           "registration_block", "registration_function", "after_wall_workloop"};
    for (int mode = 0; mode < 11; mode++) {
      entered = dispatch_semaphore_create(0);
      release = dispatch_semaphore_create(0);
      descendant = dispatch_queue_create("capture-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_queue_attr_t attr = dispatch_queue_attr_make_with_autorelease_frequency(
        DISPATCH_QUEUE_SERIAL, DISPATCH_AUTORELEASE_FREQUENCY_WORK_ITEM);
      dispatch_queue_t destination = mode == 10
        ? dispatch_queue_create_with_target("capture-destination", attr, dispatch_workloop_create("capture-wall"))
        : dispatch_queue_create("capture-destination", attr);
      dispatch_group_t group = dispatch_group_create();
      dispatch_suspend(descendant);
      dispatch_suspend(destination);
      atomic_store(&calls, 0); atomic_store(&children, 0); atomic_store(&self_rejected, 0);
      dispatch_source_t source = nil;
      if (mode < 4 || mode == 10) @autoreleasepool {
        DispatchCapture *capture = [DispatchCapture new];
        dispatch_block_t block = ^{ [capture invoke]; };
        if (mode == 0) dispatch_async(destination, block);
        else if (mode == 1) dispatch_barrier_async(destination, block);
        else if (mode == 2) dispatch_group_async(group, destination, block);
        else dispatch_after(mode == 10 ? dispatch_walltime(NULL, NSEC_PER_SEC)
                                      : dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), destination, block);
      }
      else {
        source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, destination);
        if (mode == 4) dispatch_source_set_event_handler(source, ^{ source_callback(NULL); });
        if (mode == 5) dispatch_source_set_event_handler_f(source, source_callback);
        if (mode == 6) dispatch_source_set_cancel_handler(source, ^{ source_callback(NULL); });
        if (mode == 7) dispatch_source_set_cancel_handler_f(source, source_callback);
        if (mode == 8) @autoreleasepool {
          DispatchCapture *capture = [DispatchCapture new];
          dispatch_source_set_registration_handler(source, ^{ [capture invoke]; });
        }
        if (mode == 9) dispatch_source_set_registration_handler_f(source, source_callback);
        dispatch_resume(source);
        if (mode < 6) dispatch_source_merge_data(source, 1);
        else if (mode < 8) dispatch_source_cancel(source);
      }
      if (mode == 3 || mode == 10) require(!advance(NSEC_PER_SEC, 2000000000));
      uint64_t before = now();
      dispatch_resume(destination);
      dispatch_semaphore_wait(entered, DISPATCH_TIME_FOREVER);
      struct fc_dispatch_activity held, child, finished;
      activity(&held);
      int blocked = idle(20000000) == ETIMEDOUT;
      int advance_blocked = advance(1, 20000000) == ETIMEDOUT;
      int unchanged = now() == before;
      dispatch_semaphore_signal(release);
      dispatch_barrier_sync(destination, ^{});
      require(!dispatch_group_wait(group, DISPATCH_TIME_FOREVER));
      activity(&child);
      int child_blocked = advance(1, 0) == ETIMEDOUT;
      dispatch_resume(descendant);
      require(!idle(2000000000));
      activity(&finished);
      int valid = held.pending == 1 && blocked && advance_blocked && unchanged &&
        atomic_load(&self_rejected) && child.pending == 1 && child_blocked &&
        atomic_load(&calls) == 1 && atomic_load(&children) == 1 &&
        !finished.pending && finished.submitted == finished.completed && now() == before;
      FC_PROBE_TIMING("report");
      printf("{\"mode\":\"%s\",\"held_pending\":%llu,\"blocked\":%s,\"advance_blocked\":%s,"
        "\"unchanged\":%s,\"self_rejected\":%s,\"descendant_pending\":%llu,\"child_blocked\":%s,"
        "\"calls\":%d,\"children\":%d,\"passed\":%s}\n", names[mode], held.pending,
        blocked ? "true" : "false", advance_blocked ? "true" : "false", unchanged ? "true" : "false",
        atomic_load(&self_rejected) ? "true" : "false", child.pending, child_blocked ? "true" : "false",
        atomic_load(&calls), atomic_load(&children), valid ? "true" : "false");
      require(valid);
      if (source && (mode < 6 || mode >= 8)) {
        dispatch_source_cancel(source);
        require(!idle(2000000000));
      }
    }
  }
  FC_PROBE_TIMING("control-return");
  return NULL;
}
int main(void) {
  FC_PROBE_TIMING("main-entry");
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(start && idle && advance && activity && now);
    pthread_t thread;
    require(!pthread_create(&thread, NULL, control, NULL));
    require(!pthread_join(thread, NULL));
  }
  FC_PROBE_TIMING("main-return");
}
