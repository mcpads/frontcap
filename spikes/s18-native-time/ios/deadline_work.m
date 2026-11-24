// A suspended destination makes due-but-not-entered work reproducible.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include <dlfcn.h>
#include <pthread.h>
#include <errno.h>
#include <stdatomic.h>

static int (*start)(void), (*idle)(uint64_t), (*advance)(uint64_t, uint64_t);
static int (*after)(uint64_t, dispatch_queue_t, dispatch_block_t);
static int (*schedule)(uint64_t, dispatch_queue_t, dispatch_block_t, uint64_t *);
static int (*cancel)(uint64_t);
static uint64_t (*now)(void);
static void (*activity)(struct fc_dispatch_activity *);
static dispatch_queue_t destination;
static _Atomic int calls, children, cancelled_calls, surviving_calls, released_captures;
static _Atomic int queue_calls[2];
static dispatch_semaphore_t release_entered, release_allowed;
static dispatch_queue_t release_descendant;
static _Atomic int release_self_rejected, release_children;
static _Atomic int completion_calls;

@interface DeadlineReleaseCapture : NSObject
- (void)invoke;
@end
@implementation DeadlineReleaseCapture
- (void)invoke { atomic_fetch_add(&cancelled_calls, 1); }
- (void)dealloc {
  atomic_store(&release_self_rejected, idle(0) == EDEADLK && advance(1, 0) == EDEADLK);
  dispatch_semaphore_signal(release_entered);
  dispatch_semaphore_wait(release_allowed, DISPATCH_TIME_FOREVER);
  dispatch_async(release_descendant, ^{ atomic_fetch_add(&release_children, 1); });
}
@end

@interface DeadlineCompletionCapture : DeadlineReleaseCapture
@end
@implementation DeadlineCompletionCapture
- (void)invoke { atomic_fetch_add(&completion_calls, 1); }
@end

struct CancelRequest { uint64_t token; int result; };
static void *cancel_on_thread(void *context) {
  @autoreleasepool {
    struct CancelRequest *request = context;
    request->result = cancel(request->token);
  }
  return NULL;
}

@interface DeadlineCapture : NSObject
- (void)invoke;
@end
@implementation DeadlineCapture
- (void)invoke { atomic_fetch_add(&cancelled_calls, 1); }
- (void)dealloc {
  // Re-entering observation during capture release must not deadlock cancellation.
  struct fc_dispatch_activity state;
  activity(&state);
  atomic_fetch_add(&released_captures, 1);
}
@end

#include "probe_assert.h"
static void report(const char *phase, int result) {
  struct fc_dispatch_activity state;
  activity(&state);
  printf("{\"phase\":\"%s\",\"now_ns\":%llu,\"pending\":%llu,\"submitted\":%llu,"
         "\"completed\":%llu,\"calls\":%d,\"children\":%d,\"timed_out\":%s}\n",
         phase, now(), state.pending, state.submitted, state.completed,
         atomic_load(&calls), atomic_load(&children), result == ETIMEDOUT ? "true" : "false");
}
static void verify_cancellation(void) {
  uint64_t token = 0;
  require(schedule(0, destination, ^{}, NULL) == EINVAL);
  require(schedule(UINT64_MAX, destination, ^{}, &token) == EOVERFLOW && token == 0);
  require(cancel(0) == ENOENT);
  @autoreleasepool {
    DeadlineCapture *capture = [DeadlineCapture new];
    require(!schedule(1000000000, destination, ^{ [capture invoke]; }, &token));
  }
  require(token != 0 && !cancel(token) && cancel(token) == ENOENT);
  require(atomic_load(&released_captures) == 1);
  require(!advance(1000000000, 0));
  require(!idle(0));

  dispatch_suspend(destination);
  require(!schedule(0, destination, ^{ atomic_fetch_add(&cancelled_calls, 1); }, &token));
  require(idle(0) == ETIMEDOUT);
  require(!cancel(token));
  // Removing a queued obligation must allow advancement before its queue is resumed.
  require(!idle(0) && !advance(1000000000, 0));
  require(cancel(token) == ENOENT);
  dispatch_resume(destination);
  dispatch_sync(destination, ^{});
  require(!idle(0) && atomic_load(&cancelled_calls) == 0);

  // Cancelling one token must not erase a different admitted task.
  dispatch_suspend(destination);
  uint64_t survivor = 0;
  require(!schedule(0, destination, ^{ atomic_fetch_add(&cancelled_calls, 1); }, &token));
  require(!schedule(0, destination, ^{ atomic_fetch_add(&surviving_calls, 1); }, &survivor));
  require(token != survivor && !cancel(token));
  require(advance(1, 0) == ETIMEDOUT && now() == 7000000000);
  dispatch_resume(destination);
  require(!idle(2000000000) && atomic_load(&surviving_calls) == 1);
  require(cancel(survivor) == ENOENT);

  dispatch_semaphore_t entered = dispatch_semaphore_create(0);
  dispatch_semaphore_t release = dispatch_semaphore_create(0);
  require(!schedule(0, destination, ^{
    dispatch_semaphore_signal(entered);
    dispatch_semaphore_wait(release, DISPATCH_TIME_FOREVER);
    atomic_fetch_add(&surviving_calls, 1);
  }, &token));
  dispatch_semaphore_wait(entered, DISPATCH_TIME_FOREVER);
  require(cancel(token) == EBUSY);
  require(advance(1, 0) == ETIMEDOUT && now() == 7000000000);
  dispatch_semaphore_signal(release);
  require(!idle(2000000000) && cancel(token) == ENOENT);
  require(!advance(1000000000, 0) && !idle(0));
  struct fc_dispatch_activity state;
  activity(&state);
  require(!state.pending && state.submitted == state.completed);
  printf("{\"cancellation\":true,\"now_ns\":%llu,\"cancelled_calls\":%d,"
         "\"surviving_calls\":%d,\"pending\":%llu,\"balanced\":true}\n",
         now(), atomic_load(&cancelled_calls), atomic_load(&surviving_calls), state.pending);
}

static void verify_cross_queue_completion(void) {
  for (int first = 0; first < 2; first++) {
    dispatch_queue_t queues[2] = {
      dispatch_queue_create("deadline-left", DISPATCH_QUEUE_SERIAL),
      dispatch_queue_create("deadline-right", DISPATCH_QUEUE_SERIAL)
    };
    uint64_t tokens[2];
    for (int index = 0; index < 2; index++) {
      atomic_store(&queue_calls[index], 0);
      dispatch_suspend(queues[index]);
      require(!schedule(1000000000, queues[index], ^{
        atomic_fetch_add(&queue_calls[index], 1);
      }, &tokens[index]));
    }
    require(!idle(0));
    require(!advance(1000000000, 0));
    struct fc_dispatch_activity admitted, remaining, finished;
    activity(&admitted);
    require(admitted.pending == 2);
    uint64_t due = now();
    require(advance(1, 0) == ETIMEDOUT && now() == due);
    dispatch_resume(queues[first]);
    dispatch_sync(queues[first], ^{});
    activity(&remaining);
    require(remaining.pending == 1 && atomic_load(&queue_calls[first]) == 1);
    require(atomic_load(&queue_calls[1 - first]) == 0);
    require(advance(1, 0) == ETIMEDOUT && now() == due);
    require(!cancel(tokens[1 - first]) && !idle(0));
    require(!advance(1000000000, 0));
    dispatch_resume(queues[1 - first]);
    dispatch_sync(queues[1 - first], ^{});
    activity(&finished);
    require(!finished.pending && finished.submitted == finished.completed);
    printf("{\"cross_queue\":true,\"first\":%d,\"now_ns\":%llu,"
           "\"admitted\":%llu,\"remaining\":%llu,\"pending\":%llu,\"calls\":[%d,%d]}\n",
           first, now(), admitted.pending, remaining.pending, finished.pending,
           atomic_load(&queue_calls[0]), atomic_load(&queue_calls[1]));
  }
}

static void verify_capture_release(void) {
  for (int due = 0; due < 2; due++) {
    release_entered = dispatch_semaphore_create(0);
    release_allowed = dispatch_semaphore_create(0);
    release_descendant = dispatch_queue_create("cancel-release-descendant", DISPATCH_QUEUE_SERIAL);
    dispatch_suspend(release_descendant);
    dispatch_suspend(destination);
    atomic_store(&release_children, 0);
    atomic_store(&release_self_rejected, 0);
    struct CancelRequest request = {0};
    @autoreleasepool {
      DeadlineReleaseCapture *capture = [DeadlineReleaseCapture new];
      require(!schedule(due ? 0 : 1000000000, destination, ^{ [capture invoke]; }, &request.token));
    }
    uint64_t before = now();
    pthread_t thread;
    require(!pthread_create(&thread, NULL, cancel_on_thread, &request));
    dispatch_semaphore_wait(release_entered, DISPATCH_TIME_FOREVER);
    struct fc_dispatch_activity held, descendant, finished;
    activity(&held);
    int blocked = idle(20000000) == ETIMEDOUT;
    int advance_blocked = advance(1, 20000000) == ETIMEDOUT;
    int unchanged = now() == before;
    dispatch_semaphore_signal(release_allowed);
    require(!pthread_join(thread, NULL));
    activity(&descendant);
    int child_blocked = advance(1, 0) == ETIMEDOUT;
    dispatch_resume(release_descendant);
    dispatch_resume(destination);
    require(!idle(2000000000));
    dispatch_sync(destination, ^{});
    activity(&finished);
    int valid = !request.result && held.pending == 1 && blocked && advance_blocked && unchanged &&
      atomic_load(&release_self_rejected) && descendant.pending == 1 && child_blocked &&
      atomic_load(&release_children) == 1 && !atomic_load(&cancelled_calls) &&
      !finished.pending && finished.submitted == finished.completed && now() == before;
    printf("{\"capture_release\":true,\"due\":%s,\"held_pending\":%llu,\"blocked\":%s,"
      "\"advance_blocked\":%s,\"unchanged\":%s,\"self_rejected\":%s,\"descendant_pending\":%llu,"
      "\"child_blocked\":%s,\"children\":%d,\"passed\":%s}\n", due ? "true" : "false", held.pending,
      blocked ? "true" : "false", advance_blocked ? "true" : "false", unchanged ? "true" : "false",
      atomic_load(&release_self_rejected) ? "true" : "false", descendant.pending,
      child_blocked ? "true" : "false", atomic_load(&release_children), valid ? "true" : "false");
    require(valid);
  }
}

static void verify_completion_release(void) {
  release_entered = dispatch_semaphore_create(0);
  release_allowed = dispatch_semaphore_create(0);
  release_descendant = dispatch_queue_create("completion-release-descendant", DISPATCH_QUEUE_SERIAL);
  dispatch_suspend(release_descendant);
  dispatch_suspend(destination);
  atomic_store(&release_children, 0);
  atomic_store(&release_self_rejected, 0);
  uint64_t token = 0;
  @autoreleasepool {
    DeadlineCompletionCapture *capture = [DeadlineCompletionCapture new];
    require(!schedule(0, destination, ^{ [capture invoke]; }, &token));
  }
  uint64_t before = now();
  dispatch_resume(destination);
  dispatch_semaphore_wait(release_entered, DISPATCH_TIME_FOREVER);
  struct fc_dispatch_activity held, descendant, finished;
  activity(&held);
  int blocked = idle(20000000) == ETIMEDOUT;
  int advance_blocked = advance(1, 20000000) == ETIMEDOUT;
  int unchanged = now() == before;
  int cancellation_busy = cancel(token) == EBUSY;
  dispatch_semaphore_signal(release_allowed);
  dispatch_sync(destination, ^{});
  activity(&descendant);
  int child_blocked = advance(1, 0) == ETIMEDOUT;
  dispatch_resume(release_descendant);
  require(!idle(2000000000));
  activity(&finished);
  int valid = held.pending == 1 && blocked && advance_blocked && unchanged && cancellation_busy &&
    atomic_load(&release_self_rejected) && descendant.pending == 1 && child_blocked &&
    atomic_load(&completion_calls) == 1 && atomic_load(&release_children) == 1 &&
    !finished.pending && finished.submitted == finished.completed && now() == before && cancel(token) == ENOENT;
  printf("{\"completion_release\":true,\"held_pending\":%llu,\"blocked\":%s,\"advance_blocked\":%s,"
    "\"unchanged\":%s,\"cancellation_busy\":%s,\"self_rejected\":%s,\"descendant_pending\":%llu,"
    "\"child_blocked\":%s,\"calls\":%d,\"children\":%d,\"passed\":%s}\n", held.pending,
    blocked ? "true" : "false", advance_blocked ? "true" : "false", unchanged ? "true" : "false",
    cancellation_busy ? "true" : "false", atomic_load(&release_self_rejected) ? "true" : "false",
    descendant.pending, child_blocked ? "true" : "false", atomic_load(&completion_calls),
    atomic_load(&release_children), valid ? "true" : "false");
  require(valid);
}

static void *control(void *unused) {
  @autoreleasepool {
    require(!start());
    dispatch_suspend(destination);
    require(!after(5000000000, destination, ^{
      atomic_fetch_add(&calls, 1);
      // A zero-delay child must be admitted before this callback completes.
      require(!after(0, dispatch_get_global_queue(0, 0), ^{
        dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&children, 1); });
      }));
    }));
    require(!idle(0)); report("future", 0);
    require(!advance(4999000000, 0)); require(!idle(0)); report("before", 0);
    require(!advance(1000000, 0));
    int blocked = idle(20000000); require(blocked == ETIMEDOUT); report("due_queued", blocked);
    blocked = advance(1000000000, 20000000); require(blocked == ETIMEDOUT); report("advance_blocked", blocked);
    dispatch_resume(destination);
    require(!idle(2000000000)); report("drained", 0);
    require(after(UINT64_MAX, destination, ^{}) == EOVERFLOW);
    require(advance(UINT64_MAX, 0) == EOVERFLOW);
    report("overflow_unchanged", 0);
    verify_cancellation();
    verify_cross_queue_completion();
    verify_capture_release();
    verify_completion_release();
    CFRunLoopStop(CFRunLoopGetMain());
  }
  return NULL;
}
int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    after = dlsym(RTLD_DEFAULT, "fc_dispatch_after_ns");
    schedule = dlsym(RTLD_DEFAULT, "fc_dispatch_schedule_ns");
    cancel = dlsym(RTLD_DEFAULT, "fc_dispatch_cancel");
    now = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    require(start && idle && advance && after && schedule && cancel && now && activity);
    require(cancel(0) == ENOTSUP);
    require(after(1, dispatch_get_main_queue(), ^{}) == ENOTSUP);
    destination = dispatch_queue_create("deadline-destination", DISPATCH_QUEUE_SERIAL);
    pthread_t thread;
    require(!pthread_create(&thread, NULL, control, NULL));
    CFRunLoopRun();
    pthread_join(thread, NULL);
  }
}
