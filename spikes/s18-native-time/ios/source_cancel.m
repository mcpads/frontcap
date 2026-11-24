// A requested cancellation remains pending before native delivery and through handler retirement.
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
static _Atomic int calls, children;
static dispatch_queue_t descendant;
#include "probe_assert.h"
#include "probe_timing.h"
static void callback(void *context) {
  require(context == &calls);
  atomic_fetch_add(&calls, 1);
  dispatch_async(descendant, ^{ atomic_fetch_add(&children, 1); });
}
struct early_cancellation {
  void *descendant;
  int body_hold, cleanup_hold, autorelease, self_rejected;
  _Atomic int calls, children, entered, released;
};
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static void submit_child(struct early_cancellation *context) {
  atomic_store(&context->entered, 1);
  context->self_rejected &= idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  if (context->descendant) dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{
    atomic_fetch_add(&context->children, 1);
  });
}
static void hold_early(struct early_cancellation *context) {
  context->self_rejected = idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  submit_child(context);
}
@interface CancellationCapture : NSObject {
@public
  struct early_cancellation *context;
}
- (void)invoke;
@end
@implementation CancellationCapture
- (void)invoke { atomic_fetch_add(&context->calls, 1); }
- (void)dealloc {
  if (context->cleanup_hold) hold_early(context);
  else { context->self_rejected = 1; submit_child(context); }
}
@end
static void early_callback(void *raw) {
  struct early_cancellation *context = raw;
  if (context->autorelease) {
    CancellationCapture *capture = [CancellationCapture new]; capture->context = context;
    [capture invoke];
    void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
    require(autorelease_object != NULL);
    autorelease_object((__bridge_retained void *)capture);
  } else {
    atomic_fetch_add(&context->calls, 1);
    if (context->body_hold) hold_early(context);
    else { context->self_rejected = 1; submit_child(context); }
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
  // Queued cancel block/function and event retirement; two entered bodies; three
  // entered cleanup paths; two completed cancels; one dormant and one empty source.
  struct early_cancellation contexts[12] = {0};
  dispatch_queue_t queues[12];
  dispatch_source_t sources[12];
  dispatch_queue_t children = dispatch_queue_create("early-cancel-children", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(children);
  for (int i = 0; i < 12; i++) {
    queues[i] = dispatch_queue_create("early-cancel", DISPATCH_QUEUE_SERIAL);
    if (i < 8) dispatch_suspend(queues[i]);
    sources[i] = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_OR, 0, 0, queues[i]);
    struct early_cancellation *context = &contexts[i];
    if (i < 8) context->descendant = (__bridge void *)children;
    context->body_hold = i == 3 || i == 4;
    context->cleanup_hold = i >= 5 && i <= 7;
    context->autorelease = i == 7;
    dispatch_set_context(sources[i], context);
    if (i == 2 || i == 5 || i == 6) @autoreleasepool {
      CancellationCapture *capture = [CancellationCapture new]; capture->context = context;
      if (i == 5) dispatch_source_set_cancel_handler(sources[i], ^{ [capture invoke]; });
      else dispatch_source_set_event_handler(sources[i], ^{ [capture invoke]; });
    }
    else if (i == 1 || i == 4 || i == 7 || i == 9) dispatch_source_set_cancel_handler_f(sources[i], early_callback);
    else if (i != 11) dispatch_source_set_cancel_handler(sources[i], ^{ early_callback(context); });
    dispatch_resume(sources[i]);
    if (i != 10) { dispatch_source_cancel(sources[i]); dispatch_source_cancel(sources[i]); }
  }
  for (int i = 3; i < 8; i++) dispatch_resume(queues[i]);
  for (int i = 3; i < 8; i++) await_value(&contexts[i].entered, 1);
  for (int i = 8; i < 10; i++) { await_value(&contexts[i].calls, 1); dispatch_sync(queues[i], ^{}); }
  int native_state = !dispatch_source_testcancel(sources[10]);
  for (int i = 0; i < 12; i++) if (i != 10) native_state &= dispatch_source_testcancel(sources[i]) != 0;
  uint64_t before = now(), inactive = read_activity().pending;
  require(!start());
  struct fc_dispatch_activity adopted = read_activity();
  int repeated = start() == EALREADY && read_activity().pending == adopted.pending;
  int held = blocked(before);
  for (int i = 3; i < 5; i++) { atomic_store(&contexts[i].released, 1); dispatch_sync(queues[i], ^{}); }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  struct fc_dispatch_activity cleanup = read_activity();
  int cleanup_held = blocked(before);
  for (int i = 5; i < 8; i++) { atomic_store(&contexts[i].released, 1); dispatch_sync(queues[i], ^{}); }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  struct fc_dispatch_activity queued = read_activity();
  int queued_held = blocked(before);
  for (int i = 0; i < 3; i++) queued_held &= !atomic_load(&contexts[i].calls) && !atomic_load(&contexts[i].children);
  for (int i = 0; i < 3; i++) {
    dispatch_resume(queues[i]); await_value(&contexts[i].entered, 1); dispatch_sync(queues[i], ^{});
  }
  struct fc_dispatch_activity descendants = read_activity();
  int children_held = blocked(before);
  dispatch_resume(children); require(!idle(2000000000));
  struct fc_dispatch_activity finished = read_activity();
  int calls = 0, child_calls = 0, self_rejected = 1, delivered_once = 1;
  for (int i = 0; i < 12; i++) {
    int expected = i < 10 && i != 2 && i != 6;
    calls += atomic_load(&contexts[i].calls); child_calls += atomic_load(&contexts[i].children);
    delivered_once &= atomic_load(&contexts[i].calls) == expected && atomic_load(&contexts[i].children) == (i < 8);
    if (i < 8) self_rejected &= contexts[i].self_rejected;
  }
  int balanced = !finished.pending && !finished.source_cancellations && finished.submitted == finished.completed && now() == before;
  int cancellations = adopted.source_cancellations == 8 && cleanup.source_cancellations == 6 &&
    queued.source_cancellations == 3 && !descendants.source_cancellations;
  int passed = !inactive && adopted.pending == 8 && repeated && held && cleanup.pending == 6 && cleanup_held &&
    queued.pending == 3 && queued_held && descendants.pending == 3 && children_held && cancellations && native_state &&
    calls == 8 && child_calls == 8 && self_rejected && delivered_once && balanced;
  FC_PROBE_TIMING("report");
  printf("{\"phase\":\"activation\",\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
    "\"cleanup_pending\":%llu,\"cleanup_held\":%s,\"queued_pending\":%llu,\"queued_held\":%s,"
    "\"children_pending\":%llu,\"children_held\":%s,\"cancellations\":[%llu,%llu,%llu,%llu],\"native_state\":%s,"
    "\"calls\":%d,\"children\":%d,\"self_rejected\":%s,\"delivered_once\":%s,\"balanced\":%s,\"passed\":%s}\n",
    inactive, adopted.pending, repeated ? "true" : "false", held ? "true" : "false",
    cleanup.pending, cleanup_held ? "true" : "false", queued.pending, queued_held ? "true" : "false",
    descendants.pending, children_held ? "true" : "false", adopted.source_cancellations, cleanup.source_cancellations,
    queued.source_cancellations, descendants.source_cancellations, native_state ? "true" : "false", calls, child_calls,
    self_rejected ? "true" : "false", delivered_once ? "true" : "false", balanced ? "true" : "false", passed ? "true" : "false");
  require(passed);
  dispatch_source_cancel(sources[10]); await_value(&contexts[10].calls, 1); dispatch_sync(queues[10], ^{});
  require(!idle(2000000000));
}
static void cancel_borrowed_source(void) {
  // The native invocation keeps the source alive after its last external owner leaves.
  // Instrumentation must not attempt to resurrect that external ownership on cancellation.
  static _Atomic int entered, released, returned, cancelled;
  dispatch_queue_t queue = dispatch_queue_create("borrowed-source-cancel", DISPATCH_QUEUE_SERIAL);
  dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, queue);
  void *reference = (__bridge void *)source;
  dispatch_source_set_event_handler(source, ^{
    atomic_store(&entered, 1);
    await_value(&released, 1);
    dispatch_source_cancel((__bridge dispatch_source_t)reference);
    atomic_store(&returned, 1);
  });
  dispatch_source_set_cancel_handler(source, ^{ atomic_fetch_add(&cancelled, 1); });
  dispatch_activate(source);
  dispatch_source_merge_data(source, 1);
  await_value(&entered, 1);
  uint64_t before = now();
  source = nil;
  atomic_store(&released, 1);
  await_value(&returned, 1); await_value(&cancelled, 1);
  dispatch_sync(queue, ^{}); require(!idle(2000000000));
  struct fc_dispatch_activity finished = read_activity();
  int passed = !finished.pending && !finished.source_cancellations && now() == before &&
    finished.submitted == finished.completed && atomic_load(&cancelled) == 1;
  printf("{\"phase\":\"borrowed\",\"returned\":%d,\"cancelled\":%d,\"unchanged\":%s,\"balanced\":%s}\n",
    atomic_load(&returned), atomic_load(&cancelled), now() == before ? "true" : "false", passed ? "true" : "false");
  require(passed);
}
static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  @autoreleasepool {
    activate_tracking();
    cancel_borrowed_source();
    for (int function = 0; function < 2; function++) {
      dispatch_queue_t queue = dispatch_queue_create("source-cancel-admission", DISPATCH_QUEUE_SERIAL);
      descendant = dispatch_queue_create("source-cancel-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(queue); dispatch_suspend(descendant);
      atomic_store(&calls, 0); atomic_store(&children, 0);
      dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, queue);
      dispatch_set_context(source, &calls);
      if (function) dispatch_source_set_cancel_handler_f(source, callback);
      else dispatch_source_set_cancel_handler(source, ^{ callback(&calls); });
      dispatch_resume(source);
      int registered_idle = !idle(0);
      uint64_t before = now();
      dispatch_source_cancel(source);
      dispatch_source_cancel(source); // Repeated requests must not duplicate obligations.
      struct fc_dispatch_activity pending, child, finished;
      activity(&pending);
      int idle_blocked = idle(20000000) == ETIMEDOUT;
      int advance_blocked = advance(1, 20000000) == ETIMEDOUT;
      int blocked = idle_blocked && advance_blocked;
      int unchanged = now() == before;
      int not_entered = atomic_load(&calls) == 0;
      dispatch_resume(queue);
      // The cancel callback enqueues its descendant before returning.
      for (int i = 0; i < 2000 && !atomic_load(&calls); i++) usleep(1000);
      require(atomic_load(&calls) == 1);
      dispatch_sync(queue, ^{});
      activity(&child);
      int child_blocked = advance(1, 0) == ETIMEDOUT;
      dispatch_resume(descendant);
      require(!idle(2000000000));
      activity(&finished);
      int passed = registered_idle && pending.pending == 1 && pending.source_cancellations == 1 && blocked && unchanged && not_entered &&
        child.pending == 1 && child_blocked && atomic_load(&calls) == 1 && atomic_load(&children) == 1 &&
        !finished.pending && !finished.source_cancellations && finished.submitted == finished.completed && now() == before;
      FC_PROBE_TIMING("report");
      printf("{\"function\":%s,\"registered_idle\":%s,\"cancel_pending\":%llu,\"blocked\":%s,"
        "\"unchanged\":%s,\"not_entered\":%s,\"descendant_pending\":%llu,\"child_blocked\":%s,\"passed\":%s}\n",
        function ? "true" : "false", registered_idle ? "true" : "false", pending.pending,
        blocked ? "true" : "false", unchanged ? "true" : "false", not_entered ? "true" : "false",
        child.pending, child_blocked ? "true" : "false", passed ? "true" : "false");
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
  require(start && idle && advance && activity && now);
  pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL));
  require(!pthread_join(thread, NULL));
  FC_PROBE_TIMING("main-return");
}
