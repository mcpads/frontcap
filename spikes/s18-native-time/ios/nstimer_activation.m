// Tracking must adopt entered timer bodies, autorelease cleanup and native return.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <limits.h>
#include <unistd.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static void (*raw_advance)(uint64_t), (*activity)(struct fc_dispatch_activity *);
static uint64_t (*now)(void);
struct invocation {
  int kind, self_rejected, invalid;
  void *descendant;
  _Atomic int ready, entered, released, armed, calls, children;
};
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static void hold_invocation(struct invocation *context) {
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  context->self_rejected = idle(0) == EDEADLK && advance(1, 0) == EDEADLK;
  dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{
    atomic_fetch_add(&context->children, 1);
  });
}
@interface TimerActivationCapture : NSObject {
@public
  struct invocation *context;
}
@end
@implementation TimerActivationCapture
- (void)dealloc { hold_invocation(context); }
@end
static void callback(struct invocation *context) {
  atomic_fetch_add(&context->calls, 1);
  CFRunLoopStop(CFRunLoopGetCurrent());
  if (context->kind == 0) hold_invocation(context);
  if (context->kind == 1) {
    TimerActivationCapture *capture = [TimerActivationCapture new];
    capture->context = context;
    void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
    require(autorelease_object != NULL);
    autorelease_object((__bridge_retained void *)capture);
  }
  if (context->kind == 2) atomic_store(&context->armed, 1);
}
@interface TimerActivationTarget : NSObject {
@public
  struct invocation *context;
}
- (void)fire:(NSTimer *)timer;
@end
@implementation TimerActivationTarget
- (void)fire:(NSTimer *)timer { callback(context); }
@end

struct registrations {
  _Atomic int ready, release_due, due_finished, release_future, due_calls, future_calls, unexpected;
  int native_state;
};
@interface TimerRegistrationTarget : NSObject
@property(nonatomic, copy) void (^body)(void);
- (void)fire:(NSTimer *)timer;
@end
@implementation TimerRegistrationTarget
- (void)fire:(NSTimer *)timer { self.body(); }
@end
static void *register_timers(void *raw) {
  struct registrations *context = raw;
  @autoreleasepool {
    NSTimer *timers[5];
    for (int i = 0; i < 5; i++) {
      void (^body)(void) = ^{
        if (i == 4) atomic_fetch_add(&context->future_calls, 1);
        int stop = i == 4 || atomic_fetch_add(&context->due_calls, 1) == 3;
        if (stop) CFRunLoopStop(CFRunLoopGetCurrent());
      };
      TimerRegistrationTarget *target = [TimerRegistrationTarget new]; target.body = body;
      if (i == 0 || i == 4)
        timers[i] = [NSTimer scheduledTimerWithTimeInterval:i == 4 ? 2 : 1 repeats:NO block:^(NSTimer *value) { body(); }];
      if (i == 1)
        timers[i] = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { body(); }];
      if (i == 2)
        timers[i] = [NSTimer scheduledTimerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
      if (i == 3)
        timers[i] = [NSTimer timerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
      if (i == 1 || i == 3) [NSRunLoop.currentRunLoop addTimer:timers[i] forMode:NSDefaultRunLoopMode];
    }
    void (^unexpected)(NSTimer *) = ^(NSTimer *value) { atomic_fetch_add(&context->unexpected, 1); };
    NSTimer *unscheduled = [NSTimer timerWithTimeInterval:1 repeats:NO block:unexpected];
    NSTimer *invalidated = [NSTimer scheduledTimerWithTimeInterval:1 repeats:NO block:unexpected];
    NSTimer *removed = [NSTimer scheduledTimerWithTimeInterval:1 repeats:NO block:unexpected];
    [invalidated invalidate];
    CFRunLoopRemoveTimer(CFRunLoopGetCurrent(), (__bridge CFRunLoopTimerRef)removed, kCFRunLoopDefaultMode);
    atomic_store(&context->ready, 1);
    await_value(&context->release_due, 1);
    @autoreleasepool { CFRunLoopRun(); }
    atomic_store(&context->due_finished, 1);
    await_value(&context->release_future, 1);
    @autoreleasepool { CFRunLoopRun(); }
    context->native_state = unscheduled.valid && removed.valid && !invalidated.valid;
    for (int i = 0; i < 5; i++) context->native_state &= !timers[i].valid;
    [unscheduled invalidate]; [removed invalidate];
  }
  return NULL;
}
static void *run_timer(void *raw) {
  struct invocation *context = raw;
  @autoreleasepool {
    TimerActivationTarget *target = [TimerActivationTarget new];
    target->context = context;
    NSTimer *timer;
    if (context->kind == 0)
      timer = [NSTimer scheduledTimerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { callback(context); }];
    else if (context->kind == 1)
      timer = [NSTimer timerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
    else if (context->kind == 2)
      timer = [NSTimer scheduledTimerWithTimeInterval:1 target:target selector:@selector(fire:) userInfo:nil repeats:NO];
    else
      timer = [NSTimer timerWithTimeInterval:1 repeats:NO block:^(NSTimer *value) { callback(context); }];
    if (context->kind == 1 || context->kind == 3)
      [NSRunLoop.currentRunLoop addTimer:timer forMode:NSDefaultRunLoopMode];
    CFRunLoopObserverRef gate = CFRunLoopObserverCreateWithHandler(NULL,
      kCFRunLoopBeforeSources | kCFRunLoopBeforeWaiting | kCFRunLoopExit, true, LONG_MIN,
      ^(CFRunLoopObserverRef observer, CFRunLoopActivity phase) {
        if (!atomic_exchange(&context->armed, 0)) return;
        atomic_store(&context->entered, 1);
        await_value(&context->released, 1);
      });
    require(gate != NULL);
    CFRunLoopAddObserver(CFRunLoopGetCurrent(), gate, kCFRunLoopDefaultMode);
    atomic_store(&context->ready, 1);
    @autoreleasepool { CFRunLoopRun(); }
    context->invalid = !timer.valid;
    CFRunLoopRemoveObserver(CFRunLoopGetCurrent(), gate, kCFRunLoopDefaultMode);
    CFRelease(gate);
  }
  return NULL;
}
static struct fc_dispatch_activity read_activity(void) {
  struct fc_dispatch_activity result; activity(&result); return result;
}
static uint64_t non_cancellation_work(struct fc_dispatch_activity value) {
  require(value.pending >= value.source_cancellations);
  return value.pending - value.source_cancellations;
}
static int blocked(uint64_t before) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == before;
}
static void *control(void *unused) {
  @autoreleasepool {
    // A joined run-loop thread need not have retired its asynchronous source work.
    // Hold one cancellation deterministically, while still requiring aggregate idle.
    _Atomic int cancellation_calls = 0;
    _Atomic int *cancellation_counter = &cancellation_calls;
    dispatch_queue_t cancellation_queue = dispatch_queue_create("timer-activation-cancellation", DISPATCH_QUEUE_SERIAL);
    dispatch_suspend(cancellation_queue);
    dispatch_source_t cancellation = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_OR, 0, 0, cancellation_queue);
    dispatch_source_set_cancel_handler(cancellation, ^{ atomic_fetch_add(cancellation_counter, 1); });
    dispatch_resume(cancellation);
    dispatch_source_cancel(cancellation);
    struct invocation contexts[4] = {0};
    pthread_t threads[4];
    struct registrations registrations = {0};
    pthread_t registration_thread;
    require(!pthread_create(&registration_thread, NULL, register_timers, &registrations));
    dispatch_queue_t descendants[2];
    for (int i = 0; i < 4; i++) {
      contexts[i].kind = i;
      if (i < 2) {
        descendants[i] = dispatch_queue_create("timer-activation-descendant", DISPATCH_QUEUE_SERIAL);
        dispatch_suspend(descendants[i]);
        contexts[i].descendant = (__bridge void *)descendants[i];
      }
      require(!pthread_create(&threads[i], NULL, run_timer, &contexts[i]));
    }
    for (int i = 0; i < 4; i++) await_value(&contexts[i].ready, 1);
    await_value(&registrations.ready, 1);
    raw_advance(NSEC_PER_SEC); // Deliver before tracking; no controller idle claim yet.
    for (int i = 0; i < 3; i++) await_value(&contexts[i].entered, 1);
    require(!pthread_join(threads[3], NULL)); // Completed work must not be adopted.
    uint64_t before = now(), inactive = read_activity().pending;
    require(!start());
    struct fc_dispatch_activity checkpoints[6];
    checkpoints[0] = read_activity();
    uint64_t adopted = non_cancellation_work(checkpoints[0]);
    int repeated = start() == EALREADY && non_cancellation_work(read_activity()) == adopted;
    int held = blocked(before);
    for (int i = 0; i < 2; i++) {
      atomic_store(&contexts[i].released, 1);
      require(!pthread_join(threads[i], NULL));
    }
    checkpoints[1] = read_activity();
    uint64_t with_children = non_cancellation_work(checkpoints[1]);
    int children_held = blocked(before);
    for (int i = 0; i < 2; i++) {
      dispatch_resume(descendants[i]);
      dispatch_sync(descendants[i], ^{});
    }
    checkpoints[2] = read_activity();
    uint64_t native_pending = non_cancellation_work(checkpoints[2]);
    int native_held = blocked(before);
    atomic_store(&contexts[2].released, 1);
    require(!pthread_join(threads[2], NULL));
    struct fc_dispatch_activity due = read_activity();
    checkpoints[3] = due;
    uint64_t due_pending = non_cancellation_work(due);
    fprintf(stderr, "timer-activation due pending=%llu source_cancellations=%llu work=%llu\n",
      due.pending, due.source_cancellations, due_pending);
    int due_held = blocked(before) && !atomic_load(&registrations.due_calls);
    atomic_store(&registrations.release_due, 1);
    await_value(&registrations.due_finished, 1);
    checkpoints[4] = read_activity();
    int cancellation_held = !non_cancellation_work(checkpoints[4]) &&
      !atomic_load(&cancellation_calls) && blocked(before);
    dispatch_resume(cancellation_queue);
    await_value(&cancellation_calls, 1);
    dispatch_sync(cancellation_queue, ^{});
    int future_idle = !idle(2000000000) && !read_activity().pending && !atomic_load(&registrations.future_calls);
    require(!advance(NSEC_PER_SEC, 2000000000));
    checkpoints[5] = read_activity();
    uint64_t future_pending = non_cancellation_work(checkpoints[5]);
    int future_held = blocked(now()) && !atomic_load(&registrations.future_calls);
    atomic_store(&registrations.release_future, 1);
    require(!pthread_join(registration_thread, NULL));
    int complete = !idle(2000000000);
    struct fc_dispatch_activity final = read_activity();
    int native_invalid = 1, calls = 0, children = 0;
    for (int i = 0; i < 4; i++) {
      native_invalid = native_invalid && contexts[i].invalid;
      calls += atomic_load(&contexts[i].calls);
      children += atomic_load(&contexts[i].children);
    }
    int self_rejected = contexts[0].self_rejected && contexts[1].self_rejected;
    int balanced = !final.pending && !final.source_cancellations && final.submitted == final.completed && now() == before + NSEC_PER_SEC;
    int schedules = due_pending == 4 && due_held && future_idle && future_pending == 1 && future_held &&
      atomic_load(&registrations.due_calls) == 4 && atomic_load(&registrations.future_calls) == 1 &&
      !atomic_load(&registrations.unexpected) && registrations.native_state;
    int cancellations = 1;
    for (int i = 0; i < 5; i++) cancellations &= checkpoints[i].source_cancellations >= 1;
    int passed = !inactive && adopted == 7 && repeated && held && with_children == 7 && children_held &&
      native_pending == 5 && native_held && complete && balanced && native_invalid && self_rejected && calls == 4 && children == 2 && schedules &&
      cancellations && cancellation_held && atomic_load(&cancellation_calls) == 1;
    printf("{\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
      "\"with_children\":%llu,\"children_held\":%s,\"native_pending\":%llu,\"native_held\":%s,"
      "\"due_pending\":%llu,\"due_held\":%s,\"future_idle\":%s,\"future_pending\":%llu,\"future_held\":%s,"
      "\"due_calls\":%d,\"future_calls\":%d,\"unexpected\":%d,\"schedule_state\":%s,"
      "\"complete\":%s,\"balanced\":%s,\"native_invalid\":%s,\"self_rejected\":%s,\"calls\":%d,\"children\":%d,"
      "\"cancellation_held\":%s,\"cancellation_calls\":%d,\"aggregate_pending\":[%llu,%llu,%llu,%llu,%llu,%llu],"
      "\"source_cancellations\":[%llu,%llu,%llu,%llu,%llu,%llu],\"passed\":%s}\n",
      inactive, adopted, repeated ? "true" : "false", held ? "true" : "false", with_children,
      children_held ? "true" : "false", native_pending, native_held ? "true" : "false",
      due_pending, due_held ? "true" : "false", future_idle ? "true" : "false", future_pending,
      future_held ? "true" : "false", atomic_load(&registrations.due_calls), atomic_load(&registrations.future_calls),
      atomic_load(&registrations.unexpected), registrations.native_state ? "true" : "false",
      complete ? "true" : "false", balanced ? "true" : "false", native_invalid ? "true" : "false",
      self_rejected ? "true" : "false", calls, children, cancellation_held ? "true" : "false", atomic_load(&cancellation_calls),
      checkpoints[0].pending, checkpoints[1].pending, checkpoints[2].pending, checkpoints[3].pending, checkpoints[4].pending, checkpoints[5].pending,
      checkpoints[0].source_cancellations, checkpoints[1].source_cancellations, checkpoints[2].source_cancellations,
      checkpoints[3].source_cancellations, checkpoints[4].source_cancellations, checkpoints[5].source_cancellations,
      passed ? "true" : "false");
    require(passed);
  }
  return NULL;
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    raw_advance = dlsym(RTLD_DEFAULT, "fc_advance_ns");
    activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(start && idle && advance && raw_advance && activity && now);
    pthread_t thread;
    require(!pthread_create(&thread, NULL, control, NULL));
    require(!pthread_join(thread, NULL));
  }
}
