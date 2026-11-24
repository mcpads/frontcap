// Entry acknowledgments establish delivery, not completion. Completion comes from activity tracking.
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
static dispatch_queue_t queue;
static CFTimeInterval origin;
static _Atomic int effects, calls, self_rejected, wrong_context, unexpected_events;
struct callback_context { int marker; };
static struct callback_context old_context = {0}, current_context = {73};

static void signal_pipe(int fd) { FC_REQUIRE(write(fd, "c", 1) == 1, 3); }
static void await_signal(int fd) {
  struct pollfd descriptor = {.fd = fd, .events = POLLIN};
  char value;
  FC_REQUIRE(poll(&descriptor, 1, 2000) == 1 && read(fd, &value, 1) == 1, 4);
}

static void callback(void *opaque) {
  struct callback_context *context = opaque;
  if (context != &current_context || context->marker != 73) atomic_fetch_add(&wrong_context, 1);
  atomic_fetch_add(&calls, 1);
  if (wait_idle(0) == EDEADLK && advance_when_idle(1000000000, 0) == EDEADLK)
    atomic_fetch_add(&self_rejected, 1);
  signal_pipe(entered[1]);
  await_signal(release_work[0]);
  dispatch_async(dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^{
    dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&effects, 1); });
  });
}

struct early_registration {
  int kind, self_rejected;
  void *descendant;
  _Atomic int entered, released, calls, children;
};
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  FC_REQUIRE(atomic_load(value) == expected, 3);
}
static void hold_registration(struct early_registration *context) {
  context->self_rejected = wait_idle(0) == EDEADLK && advance_when_idle(1, 0) == EDEADLK;
  atomic_store(&context->entered, 1);
  await_value(&context->released, 1);
  context->self_rejected &= wait_idle(0) == EDEADLK && advance_when_idle(1, 0) == EDEADLK;
  dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{ atomic_fetch_add(&context->children, 1); });
}
@interface RegistrationCapture : NSObject {
@public
  struct early_registration *context;
}
- (void)invoke;
@end
@implementation RegistrationCapture
- (void)invoke { atomic_fetch_add(&context->calls, 1); }
- (void)dealloc { hold_registration(context); }
@end
static void early_registration(void *raw) {
  struct early_registration *context = raw;
  if (context->kind == 3) {
    RegistrationCapture *capture = [RegistrationCapture new]; capture->context = context;
    [capture invoke];
    void *(*autorelease_object)(void *) = dlsym(RTLD_DEFAULT, "objc_autorelease");
    FC_REQUIRE(autorelease_object != NULL, 3);
    autorelease_object((__bridge_retained void *)capture);
  } else {
    atomic_fetch_add(&context->calls, 1);
    if (context->kind < 2) hold_registration(context);
    else if (context->kind >= 5) {
      context->self_rejected = wait_idle(0) == EDEADLK && advance_when_idle(1, 0) == EDEADLK;
      dispatch_async((__bridge dispatch_queue_t)context->descendant, ^{ atomic_fetch_add(&context->children, 1); });
    }
  }
}
static uint64_t pending(void) {
  struct fc_dispatch_activity state; read_activity(&state); return state.pending;
}
static int registration_blocked(void) {
  int waited = wait_idle(20000000) == ETIMEDOUT;
  int advanced = advance_when_idle(1, 20000000) == ETIMEDOUT;
  return waited && advanced && CACurrentMediaTime() == origin;
}
static void activate_tracking(void) {
  struct early_registration contexts[7] = {0};
  dispatch_source_t sources[7];
  dispatch_queue_t queues[7];
  dispatch_queue_t children = dispatch_queue_create("registration-children", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(children);
  for (int i = 0; i < 7; i++) {
    struct early_registration *context = &contexts[i]; context->kind = i;
    context->descendant = (__bridge void *)children;
    queues[i] = dispatch_queue_create("registration-activation", dispatch_queue_attr_make_with_autorelease_frequency(
      DISPATCH_QUEUE_SERIAL, DISPATCH_AUTORELEASE_FREQUENCY_WORK_ITEM));
    if (i >= 5) dispatch_suspend(queues[i]);
    sources[i] = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_OR, 0, 0, queues[i]);
    dispatch_set_context(sources[i], context);
    if (i == 2) @autoreleasepool {
      RegistrationCapture *capture = [RegistrationCapture new]; capture->context = context;
      dispatch_source_set_registration_handler(sources[i], ^{ [capture invoke]; });
    }
    else if (i == 0 || i == 5) dispatch_source_set_registration_handler(sources[i], ^{ early_registration(context); });
    else dispatch_source_set_registration_handler_f(sources[i], early_registration);
    dispatch_resume(sources[i]);
  }
  for (int i = 0; i < 4; i++) await_value(&contexts[i].entered, 1);
  await_value(&contexts[4].calls, 1); dispatch_sync(queues[4], ^{});
  uint64_t inactive = pending();
  FC_REQUIRE(!start_tracking(), 3);
  uint64_t adopted = pending();
  int repeated = start_tracking() == EALREADY && pending() == adopted;
  int held = registration_blocked();
  for (int i = 0; i < 2; i++) { atomic_store(&contexts[i].released, 1); dispatch_sync(queues[i], ^{}); }
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t cleanup_pending = pending();
  int cleanup_held = registration_blocked();
  for (int i = 2; i < 4; i++) { atomic_store(&contexts[i].released, 1); dispatch_sync(queues[i], ^{}); }
  uint64_t children_pending = pending();
  int children_held = registration_blocked();
  dispatch_resume(children); dispatch_sync(children, ^{}); dispatch_suspend(children);
  uint64_t queued_pending = pending();
  int queued_held = registration_blocked();
  for (int i = 5; i < 7; i++) { dispatch_resume(queues[i]); dispatch_sync(queues[i], ^{}); }
  uint64_t queued_children = pending();
  int queued_children_held = registration_blocked();
  dispatch_resume(children); FC_REQUIRE(!wait_idle(2000000000), 3);
  for (int i = 0; i < 7; i++) dispatch_source_cancel(sources[i]);
  FC_REQUIRE(!wait_idle(2000000000), 3);
  struct fc_dispatch_activity finished; read_activity(&finished);
  int calls = 0, child_calls = 0, self_rejected = 1, delivered_once = 1;
  for (int i = 0; i < 7; i++) {
    calls += atomic_load(&contexts[i].calls); child_calls += atomic_load(&contexts[i].children);
    delivered_once &= atomic_load(&contexts[i].calls) == 1 && atomic_load(&contexts[i].children) == (i != 4);
    if (i != 4) self_rejected &= contexts[i].self_rejected;
  }
  int balanced = !finished.pending && finished.submitted == finished.completed && CACurrentMediaTime() == origin;
  int passed = !inactive && adopted == 6 && repeated && held && cleanup_pending == 4 && cleanup_held &&
    children_pending == 4 && children_held && queued_pending == 2 && queued_held &&
    queued_children == 2 && queued_children_held && calls == 7 && child_calls == 6 && self_rejected && delivered_once && balanced;
  printf("{\"phase\":\"registration_activation\",\"inactive\":%llu,\"adopted\":%llu,\"repeated\":%s,\"held\":%s,"
    "\"cleanup_pending\":%llu,\"cleanup_held\":%s,\"children_pending\":%llu,\"children_held\":%s,"
    "\"queued_pending\":%llu,\"queued_held\":%s,\"queued_children\":%llu,\"queued_children_held\":%s,"
    "\"calls\":%d,\"children\":%d,\"self_rejected\":%s,\"delivered_once\":%s,\"balanced\":%s,\"passed\":%s}\n",
    inactive, adopted, repeated ? "true" : "false", held ? "true" : "false", cleanup_pending, cleanup_held ? "true" : "false",
    children_pending, children_held ? "true" : "false", queued_pending, queued_held ? "true" : "false",
    queued_children, queued_children_held ? "true" : "false", calls, child_calls, self_rejected ? "true" : "false",
    delivered_once ? "true" : "false", balanced ? "true" : "false", passed ? "true" : "false");
  FC_REQUIRE(passed, 3);
}

static void *control(void *unused) {
  @autoreleasepool {
    activate_tracking();
    const char *names[] = {"after_block", "after_function", "event_block", "event_function",
                           "cancel_block", "cancel_function", "registration_block", "registration_function"};
    for (int i = 0; i < 8; i++) {
      dispatch_source_t source = nil;
      if (i == 0) dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), queue, ^{ callback(&current_context); });
      else if (i == 1) dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), queue, &current_context, callback);
      else {
        source = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
        dispatch_set_context(source, &old_context);
        if (i == 2) dispatch_source_set_event_handler(source, ^{ callback(&current_context); });
        else if (i == 3) dispatch_source_set_event_handler_f(source, callback);
        else {
          dispatch_source_set_event_handler(source, ^{ atomic_fetch_add(&unexpected_events, 1); });
          if (i == 4) dispatch_source_set_cancel_handler(source, ^{ callback(&current_context); });
          else if (i == 5) dispatch_source_set_cancel_handler_f(source, callback);
          else if (i == 6) dispatch_source_set_registration_handler(source, ^{ callback(&current_context); });
          else dispatch_source_set_registration_handler_f(source, callback);
        }
        // Function handlers must receive the context at invocation, not at handler registration.
        dispatch_set_context(source, &current_context);
        dispatch_source_set_timer(source, dispatch_time(DISPATCH_TIME_NOW, (i < 4 ? 1 : 10) * NSEC_PER_SEC),
                                  DISPATCH_TIME_FOREVER, 0);
        if (i < 6) dispatch_resume(source);
      }
      int future_idle = wait_idle(0);
      FC_REQUIRE(!advance_when_idle(NSEC_PER_SEC, 2000000000), 5);
      if (i >= 6) dispatch_resume(source);
      else if (i >= 4) dispatch_source_cancel(source);
      await_signal(entered[0]);
      struct fc_dispatch_activity running;
      read_activity(&running);
      int idle_result = wait_idle(20000000);
      int advance_result = advance_when_idle(NSEC_PER_SEC, 20000000);
      double blocked_at = CACurrentMediaTime() - origin;
      signal_pipe(release_work[1]);
      int complete = wait_idle(2000000000);
      struct fc_dispatch_activity finished;
      read_activity(&finished);
      printf("{\"callback\":\"%s\",\"future_idle\":%s,\"running_pending\":%llu,"
             "\"idle_rejected\":%s,\"advance_rejected\":%s,\"blocked_at\":%.6f,"
             "\"complete\":%s,\"finished_pending\":%llu,\"finished_at\":%.6f,"
             "\"calls\":%d,\"effects\":%d,\"self_rejected\":%d,\"wrong_context\":%d,\"unexpected_events\":%d}\n",
             names[i], future_idle == 0 ? "true" : "false", (unsigned long long)running.pending,
             idle_result == ETIMEDOUT ? "true" : "false", advance_result == ETIMEDOUT ? "true" : "false", blocked_at,
             complete == 0 ? "true" : "false", (unsigned long long)finished.pending, CACurrentMediaTime() - origin,
             atomic_load(&calls), atomic_load(&effects), atomic_load(&self_rejected), atomic_load(&wrong_context),
             atomic_load(&unexpected_events));
      FC_REQUIRE(!complete, 5);
      if (i == 2 || i == 3 || i >= 6) {
        dispatch_source_cancel(source);
        FC_REQUIRE(!wait_idle(2000000000), 5);
      }
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
    advance_when_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    if (!start_tracking || !wait_idle || !advance_when_idle || !read_activity) return 2;
    if (pipe(entered) || pipe(release_work)) return 2;
    queue = dispatch_queue_create("s18-callbacks", DISPATCH_QUEUE_SERIAL);
    origin = CACurrentMediaTime();
    pthread_t controller;
    if (pthread_create(&controller, NULL, control, NULL)) return 2;
    CFRunLoopRun();
    pthread_join(controller, NULL);
    close(entered[0]); close(entered[1]); close(release_work[0]); close(release_work[1]);
  }
  return 0;
}
