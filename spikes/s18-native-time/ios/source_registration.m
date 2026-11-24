// Activated registration work remains pending before native callback entry.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static dispatch_queue_t children;
static _Atomic int calls, child_calls, self_rejected, wrong_context, bootstrap;
static int old_context, current_context;
static void await_value(_Atomic int *value, int expected) {
  for (int i = 0; i < 5000 && atomic_load(value) != expected; i++) usleep(1000);
  require(atomic_load(value) == expected);
}
static struct fc_dispatch_activity snapshot(void) {
  struct fc_dispatch_activity state; activity(&state); return state;
}
static int blocked(uint64_t instant) {
  int waited = idle(20000000) == ETIMEDOUT;
  int advanced = advance(1, 20000000) == ETIMEDOUT;
  return waited && advanced && now() == instant;
}
static void callback(void *context) {
  if (context != &current_context) atomic_fetch_add(&wrong_context, 1);
  atomic_fetch_add(&calls, 1);
  atomic_store(&self_rejected, idle(0) == EDEADLK && advance(1, 0) == EDEADLK);
  dispatch_async(children, ^{ atomic_fetch_add(&child_calls, 1); });
}
static void *control(void *unused) {
  @autoreleasepool {
    require(!start());
    const char *names[] = {"activate_block", "resume_function", "resume_suspended_block",
                           "activate_suspended_function", "late_block", "late_function"};
    for (int mode = 0; mode < 6; mode++) {
      atomic_store(&calls, 0); atomic_store(&child_calls, 0); atomic_store(&self_rejected, 0);
      atomic_store(&wrong_context, 0); atomic_store(&bootstrap, 0);
      dispatch_queue_t destination = dispatch_queue_create("registration-destination", DISPATCH_QUEUE_SERIAL);
      children = dispatch_queue_create("registration-descendant", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(children);
      dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, destination);
      require(!idle(0));
      if (mode >= 4) {
        dispatch_source_set_registration_handler(source, ^{ atomic_store(&bootstrap, 1); });
        dispatch_activate(source);
        await_value(&bootstrap, 1); dispatch_sync(destination, ^{});
        require(!idle(2000000000));
      }
      dispatch_suspend(destination);
      dispatch_set_context(source, &old_context);
      if (mode % 2) dispatch_source_set_registration_handler_f(source, callback);
      else dispatch_source_set_registration_handler(source, ^{ callback(&current_context); });
      dispatch_set_context(source, &current_context);
      uint64_t before_pending = snapshot().pending, instant = now();
      int inactive_idle = mode >= 4 || !idle(0);
      int suspended_idle = 1;
      if (mode == 2 || mode == 3) dispatch_suspend(source);
      if (mode == 2) { dispatch_resume(source); suspended_idle = !idle(0) && !snapshot().pending; }
      if (mode == 1 || mode == 2) dispatch_resume(source);
      else dispatch_activate(source);
      uint64_t queued = snapshot().pending;
      dispatch_activate(source); // Repeated activation cannot duplicate the obligation.
      int repeated = snapshot().pending == queued;
      int queued_held = blocked(instant), not_entered = !atomic_load(&calls);
      if (mode == 3) dispatch_resume(source);
      dispatch_resume(destination);
      await_value(&calls, 1); dispatch_sync(destination, ^{});
      uint64_t child_pending = snapshot().pending;
      int child_held = blocked(instant);
      dispatch_resume(children); require(!idle(2000000000));
      dispatch_source_cancel(source); require(!idle(2000000000));
      struct fc_dispatch_activity finished = snapshot();
      int balanced = !finished.pending && !finished.source_cancellations && finished.submitted == finished.completed;
      int delivered_once = atomic_load(&calls) == 1 && atomic_load(&child_calls) == 1 && !atomic_load(&wrong_context);
      int passed = before_pending == (mode >= 4) && inactive_idle && suspended_idle && queued == 1 && repeated &&
        queued_held && not_entered && child_pending == 1 && child_held && atomic_load(&self_rejected) &&
        delivered_once && balanced && now() == instant;
      printf("{\"mode\":\"%s\",\"before_pending\":%llu,\"inactive_idle\":%s,\"suspended_idle\":%s,"
        "\"queued\":%llu,\"repeated\":%s,\"held\":%s,\"not_entered\":%s,\"child_pending\":%llu,"
        "\"child_held\":%s,\"self_rejected\":%s,\"delivered_once\":%s,\"balanced\":%s,\"passed\":%s}\n",
        names[mode], before_pending, inactive_idle ? "true" : "false", suspended_idle ? "true" : "false",
        queued, repeated ? "true" : "false", queued_held ? "true" : "false", not_entered ? "true" : "false",
        child_pending, child_held ? "true" : "false", atomic_load(&self_rejected) ? "true" : "false",
        delivered_once ? "true" : "false", balanced ? "true" : "false", passed ? "true" : "false");
      require(passed);
    }
  }
  return NULL;
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    require(start && idle && advance && now && activity);
    pthread_t thread; require(!pthread_create(&thread, NULL, control, NULL)); require(!pthread_join(thread, NULL));
  }
}
