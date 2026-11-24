// Coalesced data must remain pending before entry and across a running callback.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
#include "probe_timing.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static _Atomic int calls, entered, released, children;
static unsigned long values[2];
static dispatch_queue_t descendant;
struct context { void *source; int hold; };
static void callback(void *raw) {
  struct context *context = raw;
  dispatch_source_t source = (__bridge dispatch_source_t)context->source;
  int index = atomic_fetch_add(&calls, 1);
  require(index < 2);
  values[index] = dispatch_source_get_data(source);
  if (context->hold && index == 0) {
    atomic_store(&entered, 1);
    while (!atomic_load(&released)) usleep(1000);
  } else if (context->hold) {
    dispatch_async(descendant, ^{ atomic_fetch_add(&children, 1); });
  }
}
static void handler(dispatch_source_t source, struct context *context, int function) {
  dispatch_set_context(source, context);
  if (function) dispatch_source_set_event_handler_f(source, callback);
  else dispatch_source_set_event_handler(source, ^{ callback(context); });
}
static struct fc_dispatch_activity read_activity(void) {
  struct fc_dispatch_activity value; activity(&value); return value;
}
static int blocked(uint64_t before) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == before;
}
static void wait_for(_Atomic int *value, int expected) {
  for (int i = 0; i < 2000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
struct early_context {
  void *source, *descendant;
  unsigned long value;
  int hold, self_rejected;
  _Atomic int calls, entered, released, children;
};
static void hold_early_callback(struct early_context *context) {
  atomic_store(&context->entered, 1);
  while (!atomic_load(&context->released)) usleep(1000);
  context->self_rejected = idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{ atomic_fetch_add(&context->children, 1); });
}
@interface SourceActivationCapture : NSObject {
@public
  struct early_context *context;
}
@end
@implementation SourceActivationCapture
- (void)dealloc { hold_early_callback(context); }
@end
static void early_callback(void *raw) {
  struct early_context *context = raw;
  context->value = dispatch_source_get_data((__bridge dispatch_source_t)context->source);
  atomic_fetch_add(&context->calls, 1);
  if (context->hold == 1) hold_early_callback(context);
  if (context->hold == 2) {
    SourceActivationCapture *capture = [SourceActivationCapture new];
    capture->context = context;
    void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
    require(autorelease_object != NULL);
    autorelease_object((__bridge_retained void *)capture);
  }
}
static void activate_tracking(void) {
  dispatch_queue_t queues[8], descendants[4];
  dispatch_source_t sources[8];
  struct early_context contexts[8] = {0};
  for (int i = 0; i < 8; i++) {
    queues[i] = dispatch_queue_create("source-data-before-tracking", DISPATCH_QUEUE_SERIAL);
    if (i < 3) dispatch_suspend(queues[i]);
    sources[i] = dispatch_source_create(i < 6 ? DISPATCH_SOURCE_TYPE_DATA_ADD : DISPATCH_SOURCE_TYPE_DATA_OR, 0, 0, queues[i]);
    struct early_context *context = &contexts[i];
    context->source = (__bridge void *)sources[i];
    if (i >= 4) {
      descendants[i-4] = dispatch_queue_create("source-activation-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(descendants[i-4]);
      context->descendant = (__bridge void *)descendants[i-4];
      context->hold = 1 + (i % 2);
    }
    dispatch_set_context(sources[i], context);
    if (i == 1 || i == 5 || i == 7) dispatch_source_set_event_handler_f(sources[i], early_callback);
    else dispatch_source_set_event_handler(sources[i], ^{ early_callback(context); });
    dispatch_resume(sources[i]);
    if (i < 2) dispatch_source_merge_data(sources[i], 13);
    if (i == 3) dispatch_source_merge_data(sources[i], 11);
    if (i >= 4) dispatch_source_merge_data(sources[i], i == 4 ? 7 : (i == 5 ? 9 : (i == 6 ? 21 : 23)));
  }
  wait_for(&contexts[3].calls, 1);
  dispatch_sync(queues[3], ^{});
  for (int i = 4; i < 8; i++) wait_for(&contexts[i].entered, 1);
  uint64_t inactive = read_activity().pending;
  uint64_t before = now();
  require(!start());
  uint64_t activated = read_activity().pending;
  for (int i = 0; i < 3; i++) dispatch_source_merge_data(sources[i], 17);
  uint64_t merged = read_activity().pending;
  int repeated = start() == EALREADY && read_activity().pending == merged;
  int held = blocked(before);
  uint64_t remaining[3];
  for (int i = 0; i < 3; i++) {
    dispatch_resume(queues[i]);
    wait_for(&contexts[i].calls, 1);
    dispatch_sync(queues[i], ^{});
    remaining[i] = read_activity().pending;
    if (i < 2) held = blocked(before) && held;
  }
  int running_held = blocked(before);
  for (int i = 4; i < 6; i++) {
    atomic_store(&contexts[i].released, 1);
    dispatch_sync(queues[i], ^{});
    dispatch_resume(descendants[i-4]);
    wait_for(&contexts[i].children, 1);
    dispatch_sync(descendants[i-4], ^{});
  }
  uint64_t generic_pending = read_activity().pending;
  int generic_held = blocked(before);
  for (int i = 6; i < 8; i++) {
    atomic_store(&contexts[i].released, 1);
    dispatch_sync(queues[i], ^{});
  }
  uint64_t children_pending = read_activity().pending;
  int children_held = blocked(before);
  for (int i = 2; i < 4; i++) dispatch_resume(descendants[i]);
  require(!idle(2000000000));
  for (int i = 0; i < 8; i++) dispatch_source_cancel(sources[i]);
  require(!idle(2000000000));
  struct fc_dispatch_activity final = read_activity();
  int self_rejected = 1, children = 0;
  for (int i = 4; i < 8; i++) {
    self_rejected = self_rejected && contexts[i].self_rejected;
    children += atomic_load(&contexts[i].children);
  }
  int balanced = !final.pending && final.submitted == final.completed && now() == before;
  int passed = inactive == 0 && activated == 6 && merged == 7 && repeated && held && running_held &&
    remaining[0] == 6 && remaining[1] == 5 && remaining[2] == 4 && balanced && children_pending == 2 &&
    generic_pending == 2 && generic_held && children_held && children == 4 && self_rejected && contexts[0].value == 30 && contexts[1].value == 30 &&
    contexts[2].value == 17 && contexts[3].value == 11 && contexts[4].value == 7 && contexts[5].value == 9 && contexts[6].value == 21 && contexts[7].value == 23;
  FC_PROBE_TIMING("report");
  printf("{\"mode\":\"activation\",\"inactive\":%llu,\"activated\":%llu,\"merged\":%llu,"
    "\"repeated\":%s,\"held\":%s,\"remaining\":[%llu,%llu,%llu],\"values\":[%lu,%lu,%lu,%lu,%lu,%lu,%lu,%lu],"
    "\"generic_pending\":%llu,\"generic_held\":%s,\"running_held\":%s,\"children_pending\":%llu,\"children_held\":%s,\"children\":%d,\"self_rejected\":%s,"
    "\"balanced\":%s,\"passed\":%s}\n", inactive, activated, merged, repeated ? "true" : "false", held ? "true" : "false",
    remaining[0], remaining[1], remaining[2], contexts[0].value, contexts[1].value, contexts[2].value, contexts[3].value,
    contexts[4].value, contexts[5].value, contexts[6].value, contexts[7].value, generic_pending,
    generic_held ? "true" : "false", running_held ? "true" : "false", children_pending,
    children_held ? "true" : "false", children, self_rejected ? "true" : "false",
    balanced ? "true" : "false", passed ? "true" : "false");
  require(passed);
}
static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  @autoreleasepool {
    activate_tracking();
    for (int function = 0; function < 2; function++) {
      dispatch_queue_t queue = dispatch_queue_create("source-data-admission", DISPATCH_QUEUE_SERIAL);
      descendant = dispatch_queue_create("source-data-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(queue); dispatch_suspend(descendant);
      dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, queue);
      struct context context = {(__bridge void *)source, 1};
      handler(source, &context, function);
      dispatch_resume(source);
      atomic_store(&calls, 0); atomic_store(&entered, 0); atomic_store(&released, 0); atomic_store(&children, 0);
      dispatch_source_merge_data(source, 0);
      int zero_idle = !idle(0);
      uint64_t before = now();
      dispatch_source_merge_data(source, 2); dispatch_source_merge_data(source, 3);
      uint64_t queued = read_activity().pending;
      int before_blocked = blocked(before) && !atomic_load(&calls);
      dispatch_resume(queue); wait_for(&entered, 1);
      dispatch_suspend(queue);
      dispatch_source_merge_data(source, 7); dispatch_source_merge_data(source, 11);
      uint64_t running = read_activity().pending;
      atomic_store(&released, 1);
      for (int i = 0; i < 2000 && read_activity().pending != 1; i++) usleep(1000);
      uint64_t next = read_activity().pending;
      int next_blocked = blocked(before) && atomic_load(&calls) == 1;
      dispatch_resume(queue); wait_for(&calls, 2);
      dispatch_sync(queue, ^{});
      uint64_t child = read_activity().pending;
      int child_blocked = blocked(before);
      dispatch_resume(descendant);
      dispatch_source_cancel(source);
      require(!idle(2000000000));
      struct fc_dispatch_activity final = read_activity();
      int passed = zero_idle && queued == 1 && before_blocked && running == 2 && next == 1 && next_blocked &&
        child == 1 && child_blocked && values[0] == 5 && values[1] == 18 && atomic_load(&children) == 1 &&
        !final.pending && final.submitted == final.completed && now() == before;
      FC_PROBE_TIMING("report");
      printf("{\"mode\":\"delivery\",\"function\":%s,\"zero_idle\":%s,\"queued\":%llu,\"before_blocked\":%s,"
        "\"running\":%llu,\"next\":%llu,\"next_blocked\":%s,\"child\":%llu,\"child_blocked\":%s,"
        "\"values\":[%lu,%lu],\"passed\":%s}\n", function ? "true" : "false", zero_idle ? "true" : "false",
        queued, before_blocked ? "true" : "false", running, next, next_blocked ? "true" : "false", child,
        child_blocked ? "true" : "false", values[0], values[1], passed ? "true" : "false");
      require(passed);

      for (int cancel = 0; cancel < 2; cancel++) {
        queue = dispatch_queue_create("source-data-lifecycle", DISPATCH_QUEUE_SERIAL);
        dispatch_suspend(queue);
        source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, queue);
        struct context lifecycle = {(__bridge void *)source, 0};
        handler(source, &lifecycle, function);
        atomic_store(&calls, 0);
        dispatch_source_merge_data(source, 17);
        if (cancel) {
          dispatch_resume(source);
          dispatch_source_cancel(source);
          dispatch_source_merge_data(source, 23); // Native cancellation ignores later merges.
        } else {
          handler(source, &lifecycle, function); // Replacement cannot retire queued data.
          dispatch_resume(source);
        }
        int held = blocked(before) && !atomic_load(&calls);
        dispatch_resume(queue);
        require(!idle(2000000000));
        if (!cancel) { dispatch_source_cancel(source); require(!idle(2000000000)); }
        dispatch_source_merge_data(source, 29);
        int post_cancel_idle = !idle(0);
        final = read_activity();
        int lifecycle_passed = held && post_cancel_idle && atomic_load(&calls) == !cancel &&
          (cancel || values[0] == 17) && !final.pending && final.submitted == final.completed && now() == before;
        FC_PROBE_TIMING("report");
        printf("{\"mode\":\"%s\",\"function\":%s,\"held\":%s,\"calls\":%d,\"post_cancel_idle\":%s,\"passed\":%s}\n",
          cancel ? "cancel" : "replace", function ? "true" : "false", held ? "true" : "false",
          atomic_load(&calls), post_cancel_idle ? "true" : "false", lifecycle_passed ? "true" : "false");
        require(lifecycle_passed);
      }
    }
    const char *names[] = {"missing", "remove_block", "remove_function", "cancel_empty"};
    for (int mode = 0; mode < 4; mode++) {
      dispatch_queue_t queue = dispatch_queue_create("source-data-empty-handler", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(queue);
      dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, queue);
      struct context context = {(__bridge void *)source, 0};
      atomic_store(&calls, 0);
      if (mode == 1 || mode == 2) handler(source, &context, mode == 2);
      dispatch_resume(source);
      uint64_t before = now();
      dispatch_source_merge_data(source, 17);
      if (mode == 1) dispatch_source_set_event_handler(source, nil);
      if (mode == 2) dispatch_source_set_event_handler_f(source, NULL);
      if (mode == 3) dispatch_source_cancel(source);
      int held = blocked(before) && !atomic_load(&calls);
      dispatch_resume(queue);
      int drained = !idle(2000000000);
      int silent = !atomic_load(&calls);
      int installed = mode == 3;
      if (drained && mode != 3) {
        handler(source, &context, mode == 2);
        dispatch_source_merge_data(source, 29);
        installed = !idle(2000000000) && atomic_load(&calls) == 1 && values[0] == 29;
      }
      dispatch_source_cancel(source);
      require(!idle(2000000000));
      struct fc_dispatch_activity final = read_activity();
      int passed = held && drained && silent && installed && !final.pending &&
        final.submitted == final.completed && now() == before;
      FC_PROBE_TIMING("report");
      printf("{\"mode\":\"%s\",\"held\":%s,\"drained\":%s,\"silent\":%s,\"installed\":%s,\"passed\":%s}\n",
        names[mode], held ? "true" : "false", drained ? "true" : "false", silent ? "true" : "false",
        installed ? "true" : "false", passed ? "true" : "false");
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
