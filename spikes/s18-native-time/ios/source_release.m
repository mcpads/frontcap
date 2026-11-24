// Handler retirement must retain capture destruction and its descendants.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static dispatch_semaphore_t entered, release;
static dispatch_queue_t descendant;
static _Atomic int calls, children, self_rejected;
#include "probe_assert.h"

@interface SourceCapture : NSObject
- (void)invoke;
@end
@implementation SourceCapture
- (void)invoke { atomic_fetch_add(&calls, 1); }
- (void)dealloc {
  atomic_store(&self_rejected, idle(0) == EDEADLK && advance(1, 0) == EDEADLK);
  dispatch_semaphore_signal(entered);
  dispatch_semaphore_wait(release, DISPATCH_TIME_FOREVER);
  dispatch_async(descendant, ^{ atomic_fetch_add(&children, 1); });
}
@end

static dispatch_semaphore_t registered, mutate;
static dispatch_queue_t destination;
static int mode;

static void *replace_or_cancel(void *unused) {
  @autoreleasepool {
    dispatch_source_t source = dispatch_source_create(DISPATCH_SOURCE_TYPE_DATA_ADD, 0, 0, destination);
    @autoreleasepool {
      SourceCapture *capture = [SourceCapture new];
      if (mode >= 4) dispatch_source_set_registration_handler(source, ^{ [capture invoke]; });
      else if (mode == 0 || mode == 2) dispatch_source_set_event_handler(source, ^{ [capture invoke]; });
      else dispatch_source_set_cancel_handler(source, ^{ [capture invoke]; });
    }
    if (mode == 6) { dispatch_suspend(destination); dispatch_resume(source); }
    else if (mode < 4) dispatch_resume(source);
    dispatch_semaphore_signal(registered);
    dispatch_semaphore_wait(mutate, DISPATCH_TIME_FOREVER);
    if (mode == 0) dispatch_source_set_event_handler(source, nil);
    else if (mode == 1) dispatch_source_set_cancel_handler(source, nil);
    else if (mode == 4) dispatch_source_set_registration_handler_f(source, NULL);
    dispatch_source_cancel(source);
    if (mode == 6) dispatch_resume(destination);
    else if (mode >= 4) dispatch_resume(source);
  }
  return NULL;
}

static void *control(void *unused) {
  @autoreleasepool {
    require(!start());
    const char *names[] = {"event_replace", "cancel_replace", "event_cancel", "cancel_release", "registration_replace", "registration_cancel", "registration_queued_cancel"};
    for (mode = 0; mode < 7; mode++) {
      entered = dispatch_semaphore_create(0);
      release = dispatch_semaphore_create(0);
      registered = dispatch_semaphore_create(0);
      mutate = dispatch_semaphore_create(0);
      descendant = dispatch_queue_create("source-release-descendant", DISPATCH_QUEUE_SERIAL);
      destination = dispatch_queue_create("source-release-destination", DISPATCH_QUEUE_SERIAL);
      dispatch_suspend(descendant);
      atomic_store(&calls, 0); atomic_store(&children, 0); atomic_store(&self_rejected, 0);
      pthread_t mutator;
      require(!pthread_create(&mutator, NULL, replace_or_cancel, NULL));
      dispatch_semaphore_wait(registered, DISPATCH_TIME_FOREVER);
      int registered_idle = !idle(0);
      struct fc_dispatch_activity registered_state; activity(&registered_state);
      uint64_t before = now();
      dispatch_semaphore_signal(mutate);
      dispatch_semaphore_wait(entered, DISPATCH_TIME_FOREVER);
      struct fc_dispatch_activity held, child, finished;
      activity(&held);
      int blocked = idle(20000000) == ETIMEDOUT;
      int advance_blocked = advance(1, 20000000) == ETIMEDOUT;
      int unchanged = now() == before;
      dispatch_semaphore_signal(release);
      require(!pthread_join(mutator, NULL));
      dispatch_sync(destination, ^{});
      activity(&child);
      int child_blocked = advance(1, 0) == ETIMEDOUT;
      dispatch_resume(descendant);
      require(!idle(2000000000));
      activity(&finished);
      int valid = registered_idle == (mode != 6) && registered_state.pending == (mode == 6) && held.pending == 1 && blocked && advance_blocked && unchanged &&
        atomic_load(&self_rejected) && child.pending == 1 && child_blocked &&
        atomic_load(&calls) == (mode == 3) && atomic_load(&children) == 1 &&
        !finished.pending && finished.submitted == finished.completed && now() == before;
      printf("{\"mode\":\"%s\",\"registered_idle\":%s,\"registered_pending\":%llu,\"held_pending\":%llu,\"blocked\":%s,"
        "\"advance_blocked\":%s,\"unchanged\":%s,\"self_rejected\":%s,\"descendant_pending\":%llu,"
        "\"child_blocked\":%s,\"calls\":%d,\"children\":%d,\"passed\":%s}\n",
        names[mode], registered_idle ? "true" : "false", registered_state.pending, held.pending,
        blocked ? "true" : "false", advance_blocked ? "true" : "false", unchanged ? "true" : "false",
        atomic_load(&self_rejected) ? "true" : "false", child.pending, child_blocked ? "true" : "false",
        atomic_load(&calls), atomic_load(&children), valid ? "true" : "false");
      require(valid);
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
    activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(start && idle && advance && activity && now);
    pthread_t thread;
    require(!pthread_create(&thread, NULL, control, NULL));
    require(!pthread_join(thread, NULL));
  }
}
