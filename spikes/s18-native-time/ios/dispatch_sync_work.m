// Synchronous dispatch borrows caller state and stays pending through native return.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>
#include "probe_assert.h"
#include "probe_timing.h"

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static void *(*autorelease_object)(void *);
static const char *names[] = {"block", "function", "barrier_block", "barrier_function"};
struct invocation {
  void *queue, *descendant;
  int mode, phase, nested, self_rejected, same_thread, pool_preserved;
  pthread_t caller;
  _Atomic int calls, children, entered, released, destroyed;
};
static struct fc_dispatch_activity read_activity(void) {
  struct fc_dispatch_activity value; activity(&value); return value;
}
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static int self_rejected(void) {
  uint64_t before = now();
  return idle(0) == EDEADLK && advance(1, 0) == EDEADLK && now() == before;
}
static int blocked(uint64_t before) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == before;
}
static void hold(struct invocation *context) {
  context->self_rejected &= self_rejected();
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{
    atomic_fetch_add(&context->children, 1);
  });
}
@interface SyncCleanup : NSObject { @public struct invocation *context; }
@end
@implementation SyncCleanup
- (void)dealloc { hold(context); }
@end
@interface BorrowedPoolObject : NSObject { @public struct invocation *context; }
@end
@implementation BorrowedPoolObject
- (void)dealloc { atomic_fetch_add(&context->destroyed, 1); }
@end
static void body(void *raw) {
  struct invocation *context = raw;
  atomic_fetch_add(&context->calls, 1);
  context->same_thread = pthread_equal(context->caller, pthread_self());
  context->self_rejected = self_rejected();
  BorrowedPoolObject *borrowed = [BorrowedPoolObject new]; borrowed->context = context;
  autorelease_object((__bridge_retained void *)borrowed);
  if (context->phase == 0) {
    if (context->nested) {
      dispatch_queue_t inner = dispatch_queue_create("sync-inner", DISPATCH_QUEUE_SERIAL);
      dispatch_sync(inner, ^{ hold(context); });
    } else hold(context);
  } else if (context->phase == 1) @autoreleasepool {
    SyncCleanup *capture = [SyncCleanup new]; capture->context = context;
    autorelease_object((__bridge_retained void *)capture);
  }
}
static void *invoke(void *raw) {
  struct invocation *context = raw;
  context->caller = pthread_self();
  @autoreleasepool {
    dispatch_queue_t queue = (__bridge dispatch_queue_t)context->queue;
    switch (context->mode) {
      case 0: dispatch_sync(queue, ^{ body(context); }); break;
      case 1: dispatch_sync_f(queue, context, body); break;
      case 2: dispatch_barrier_sync(queue, ^{ body(context); }); break;
      case 3: dispatch_barrier_sync_f(queue, context, body); break;
      default: abort();
    }
    context->pool_preserved = atomic_load(&context->destroyed) == 0;
  }
  return NULL;
}
static dispatch_queue_t destination(int mode) {
  return dispatch_queue_create("sync-destination", dispatch_queue_attr_make_with_autorelease_frequency(
    mode < 2 ? DISPATCH_QUEUE_SERIAL : DISPATCH_QUEUE_CONCURRENT,
    DISPATCH_AUTORELEASE_FREQUENCY_WORK_ITEM));
}
static void activate_tracking(void) {
  // Four APIs in entered body, app-local cleanup and already-returned states.
  struct invocation contexts[12] = {0};
  dispatch_queue_t queues[12]; pthread_t threads[12];
  dispatch_queue_t children = dispatch_queue_create("early-sync-children", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(children);
  for (int i = 0; i < 12; i++) {
    queues[i] = destination(i % 4);
    contexts[i].queue = (__bridge void *)queues[i];
    contexts[i].descendant = (__bridge void *)children;
    contexts[i].mode = i % 4; contexts[i].phase = i / 4;
    require(!pthread_create(&threads[i], NULL, invoke, &contexts[i]));
  }
  for (int i = 0; i < 8; i++) await_value(&contexts[i].entered, 1);
  for (int i = 8; i < 12; i++) require(!pthread_join(threads[i], NULL));
  uint64_t before = now(), inactive = read_activity().pending;
  require(!start());
  uint64_t adopted = read_activity().pending;
  int repeated = start() == EALREADY && read_activity().pending == adopted;
  int held = blocked(before);
  for (int i = 0; i < 4; i++) {
    atomic_store(&contexts[i].released, 1); require(!pthread_join(threads[i], NULL));
  }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t cleanup_pending = read_activity().pending;
  int cleanup_held = blocked(before);
  for (int i = 4; i < 8; i++) {
    atomic_store(&contexts[i].released, 1); require(!pthread_join(threads[i], NULL));
  }
  uint64_t children_pending = read_activity().pending;
  int children_held = blocked(before);
  dispatch_resume(children); require(!idle(2000000000));
  struct fc_dispatch_activity finished = read_activity();
  int calls = 0, child_calls = 0, guarded = 1, delivered_once = 1, caller_state = 1;
  for (int i = 0; i < 12; i++) {
    calls += atomic_load(&contexts[i].calls); child_calls += atomic_load(&contexts[i].children);
    delivered_once &= atomic_load(&contexts[i].calls) == 1 && atomic_load(&contexts[i].children) == (i < 8);
    guarded &= contexts[i].self_rejected;
    caller_state &= contexts[i].same_thread && contexts[i].pool_preserved && atomic_load(&contexts[i].destroyed) == 1;
  }
  int balanced = !finished.pending && finished.submitted == finished.completed && now() == before;
  int passed = !inactive && adopted == 8 && repeated && held && cleanup_pending == 4 && cleanup_held &&
    children_pending == 4 && children_held && calls == 12 && child_calls == 8 && guarded &&
    delivered_once && caller_state && balanced;
  FC_PROBE_TIMING("report");
  printf("{\"mode\":\"activation\",\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
    "\"cleanup_pending\":%llu,\"cleanup_held\":%s,\"children_pending\":%llu,\"children_held\":%s,"
    "\"calls\":%d,\"children\":%d,\"self_rejected\":%s,\"delivered_once\":%s,\"caller_state\":%s,"
    "\"balanced\":%s,\"passed\":%s}\n", inactive, adopted, repeated ? "true" : "false", held ? "true" : "false",
    cleanup_pending, cleanup_held ? "true" : "false", children_pending, children_held ? "true" : "false",
    calls, child_calls, guarded ? "true" : "false", delivered_once ? "true" : "false", caller_state ? "true" : "false",
    balanced ? "true" : "false", passed ? "true" : "false");
  require(passed);
}
static void queued_calls(void) {
  for (int nested = 0; nested < 2; nested++) for (int mode = 0; mode < 4; mode++) {
    dispatch_queue_t queue = destination(mode);
    dispatch_queue_t children = dispatch_queue_create("sync-children", DISPATCH_QUEUE_SERIAL);
    dispatch_suspend(queue); dispatch_suspend(children);
    struct invocation context = {.queue = (__bridge void *)queue, .descendant = (__bridge void *)children,
      .mode = mode, .nested = nested};
    uint64_t before = now(); pthread_t thread;
    require(!pthread_create(&thread, NULL, invoke, &context));
    // Pending admission itself acknowledges submission; no scheduling-delay assertion.
    for (int i = 0; i < 5000 && read_activity().pending != 1; i++) usleep(1000);
    uint64_t queued = read_activity().pending;
    int queued_held = blocked(before), not_entered = !atomic_load(&context.calls);
    dispatch_resume(queue); await_value(&context.entered, 1);
    uint64_t entered = read_activity().pending;
    int body_held = blocked(before);
    atomic_store(&context.released, 1); require(!pthread_join(thread, NULL));
    uint64_t child_pending = read_activity().pending;
    int child_held = blocked(before);
    dispatch_resume(children); require(!idle(2000000000));
    struct fc_dispatch_activity finished = read_activity();
    int caller_state = context.same_thread && context.pool_preserved && atomic_load(&context.destroyed) == 1;
    int balanced = !finished.pending && finished.submitted == finished.completed && now() == before;
    int passed = queued == 1 && queued_held && not_entered && entered == 1 + nested && body_held &&
      child_pending == 1 && child_held && context.self_rejected && caller_state &&
      atomic_load(&context.calls) == 1 && atomic_load(&context.children) == 1 && balanced;
    FC_PROBE_TIMING("report");
    printf("{\"mode\":\"%s\",\"nested\":%s,\"queued\":%llu,\"queued_held\":%s,\"not_entered\":%s,"
      "\"entered\":%llu,\"body_held\":%s,\"child_pending\":%llu,\"child_held\":%s,"
      "\"self_rejected\":%s,\"caller_state\":%s,\"calls\":%d,\"children\":%d,\"balanced\":%s,\"passed\":%s}\n",
      names[mode], nested ? "true" : "false", queued, queued_held ? "true" : "false", not_entered ? "true" : "false",
      entered, body_held ? "true" : "false", child_pending, child_held ? "true" : "false",
      context.self_rejected ? "true" : "false", caller_state ? "true" : "false", atomic_load(&context.calls),
      atomic_load(&context.children), balanced ? "true" : "false", passed ? "true" : "false");
    require(passed);
  }
}
static void barrier_order(void) {
  for (int mode = 2; mode < 4; mode++) {
    dispatch_queue_t queue = destination(mode);
    dispatch_queue_t children = dispatch_queue_create("sync-barrier-children", DISPATCH_QUEUE_SERIAL);
    dispatch_suspend(children);
    __block _Atomic int prior_entered = 0, prior_released = 0, later = 0;
    dispatch_async(queue, ^{
      atomic_store(&prior_entered, 1); await_value(&prior_released, 1);
    });
    await_value(&prior_entered, 1);
    struct invocation context = {.queue = (__bridge void *)queue, .descendant = (__bridge void *)children, .mode = mode};
    pthread_t thread; uint64_t before = now();
    require(!pthread_create(&thread, NULL, invoke, &context));
    for (int i = 0; i < 5000 && read_activity().pending != 2; i++) usleep(1000);
    // Admission precedes native enqueue. Verify prior-work exclusion now, and submit
    // later work only after callback entry has acknowledged the native barrier.
    uint64_t queued = read_activity().pending;
    int prior_excluded = !atomic_load(&context.calls) && blocked(before);
    atomic_store(&prior_released, 1); await_value(&context.entered, 1);
    dispatch_async(queue, ^{ atomic_fetch_add(&later, 1); });
    uint64_t entered = read_activity().pending;
    int later_excluded = !atomic_load(&later) && blocked(before) && !atomic_load(&later);
    atomic_store(&context.released, 1); require(!pthread_join(thread, NULL));
    dispatch_barrier_sync(queue, ^{});
    uint64_t child_pending = read_activity().pending;
    int child_held = blocked(before);
    dispatch_resume(children); require(!idle(2000000000));
    struct fc_dispatch_activity finished = read_activity();
    int balanced = !finished.pending && finished.submitted == finished.completed && now() == before;
    int passed = queued == 2 && prior_excluded && entered == 2 && later_excluded &&
      child_pending == 1 && child_held && atomic_load(&context.calls) == 1 && atomic_load(&later) == 1 &&
      atomic_load(&context.children) == 1 && context.self_rejected && context.same_thread &&
      context.pool_preserved && atomic_load(&context.destroyed) == 1 && balanced;
    FC_PROBE_TIMING("report");
    printf("{\"mode\":\"order_%s\",\"queued\":%llu,\"prior_excluded\":%s,\"entered\":%llu,"
      "\"later_excluded\":%s,\"child_pending\":%llu,\"child_held\":%s,\"calls\":%d,\"later\":%d,"
      "\"children\":%d,\"balanced\":%s,\"passed\":%s}\n", names[mode], queued, prior_excluded ? "true" : "false",
      entered, later_excluded ? "true" : "false", child_pending, child_held ? "true" : "false",
      atomic_load(&context.calls), atomic_load(&later), atomic_load(&context.children),
      balanced ? "true" : "false", passed ? "true" : "false");
    require(passed);
  }
}
static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  @autoreleasepool { activate_tracking(); queued_calls(); barrier_order(); }
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
    autorelease_object = dlsym(RTLD_DEFAULT, "objc_autorelease");
    require(start && idle && advance && activity && now && autorelease_object);
    pthread_t thread;
    require(!pthread_create(&thread, NULL, control, NULL));
    require(!pthread_join(thread, NULL));
  }
  FC_PROBE_TIMING("main-return");
}
