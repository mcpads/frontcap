// Native dependency waits preserve real deadlines, single-waiter ownership and capture release.
#import <Foundation/Foundation.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
#include "probe_timing.h"
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>

static int (*start_tracking)(void), (*wait_idle)(uint64_t);
static uint64_t (*host_ticks)(void), (*now_ns)(void);
static void (*advance)(uint64_t);
static void (*read_wait_state)(struct fc_dispatch_wait_state *);
static void (*read_activity)(struct fc_dispatch_activity *);
static mach_timebase_info_data_t timebase;
static _Atomic unsigned resumes, forwarded, calls, destroyed;

static void resume_background_work(void *unused) {
  (void)unused;
  for (unsigned i = 0; i < 12; i++) { usleep(10000); atomic_fetch_add(&resumes, 1); }
}

static uint64_t elapsed_ns(uint64_t start) {
  return (uint64_t)((__uint128_t)(host_ticks() - start) * timebase.numer / timebase.denom);
}

static struct fc_dispatch_wait_state await_callers(uint64_t count, uint64_t blocks) {
  uint64_t start = host_ticks();
  struct fc_dispatch_wait_state state;
  for (;;) {
    read_wait_state(&state);
    if (state.callers == count) {
      require(state.native_waiters == 1 && state.retained_blocks == blocks);
      return state;
    }
    require(elapsed_ns(start) < 2000000000);
    usleep(1000);
  }
}

static void *waiter(void *unused) {
  (void)unused;
  @autoreleasepool { return (void *)(intptr_t)wait_idle(2000000000); }
}

@interface WaitCapture : NSObject
- (void)invoke;
@end
@implementation WaitCapture
- (void)invoke { atomic_fetch_add(&calls, 1); }
- (void)dealloc {
  // Final capture release may re-enter the observer. It must not hold its mutex.
  struct fc_dispatch_activity activity;
  read_activity(&activity);
  atomic_fetch_add(&destroyed, 1);
}
@end

static void submit(unsigned kind, dispatch_queue_t queue, dispatch_group_t group, dispatch_block_t block) {
  if (kind == 0) dispatch_async(queue, block);
  else if (kind == 1) dispatch_barrier_async(queue, block);
  else dispatch_group_async(group, queue, block);
}

static _Atomic unsigned entered;
static void activate_waiting_work(void) {
  dispatch_queue_t queues[3];
  dispatch_semaphore_t gates[3];
  dispatch_group_t group = dispatch_group_create();
  for (unsigned i = 0; i < 3; i++) {
    // Establish entered/queued states before the controller has a dependency to
    // wait on. The separate progress comparison exercises background scheduling.
    queues[i] = dispatch_queue_create("s18-adopted-wait",
      dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_DEFAULT, 0));
    gates[i] = dispatch_semaphore_create(0);
    @autoreleasepool {
      WaitCapture *capture = [WaitCapture new];
      submit(i, queues[i], group, ^{ [capture invoke]; });
    }
    dispatch_sync(queues[i], ^{});
  }
  require(atomic_load(&calls) == 3 && atomic_load(&destroyed) == 3);
  atomic_store(&calls, 0); atomic_store(&destroyed, 0);
  for (unsigned i = 0; i < 3; i++) {
    dispatch_semaphore_t gate = gates[i];
    @autoreleasepool {
      WaitCapture *running = [WaitCapture new], *queued = [WaitCapture new];
      submit(i, queues[i], group, ^{
        [running invoke];
        atomic_fetch_add(&entered, 1);
        dispatch_semaphore_wait(gate, DISPATCH_TIME_FOREVER);
      });
      submit(i, queues[i], group, ^{
        [queued invoke];
        dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&forwarded, 1); });
      });
    }
  }
  uint64_t start = host_ticks(), before = now_ns();
  while (atomic_load(&entered) != 3) {
    if (elapsed_ns(start) >= 2000000000)
      fprintf(stderr, "activation-preparation entered=%u expected=3\n", atomic_load(&entered));
    require(elapsed_ns(start) < 2000000000);
    usleep(1000);
  }
  struct fc_dispatch_activity inactive, adopted, final;
  read_activity(&inactive);
  require(!inactive.pending && start_tracking() == 0);
  read_activity(&adopted);
  require(adopted.pending == 6);
  pthread_t controller;
  require(pthread_create(&controller, NULL, waiter, NULL) == 0);
  struct fc_dispatch_wait_state waiting = await_callers(1, 6);
  for (unsigned i = 0; i < 3; i++) dispatch_semaphore_signal(gates[i]);
  void *result = NULL;
  require(pthread_join(controller, &result) == 0);
  require(dispatch_group_wait(group, dispatch_time(DISPATCH_TIME_NOW, 2000000000)) == 0);
  read_activity(&final);
  struct fc_dispatch_wait_state retired;
  read_wait_state(&retired);
  require(!result && !final.pending && final.submitted == final.completed && now_ns() == before);
  require(!retired.callers && !retired.native_waiters && !retired.retained_blocks);
  require(atomic_load(&calls) == 6 && atomic_load(&destroyed) == 6 && atomic_load(&forwarded) == 3);
  printf("{\"phase\":\"activation\",\"completed_before\":3,\"inactive\":%llu,\"adopted\":%llu,"
         "\"native_waiters\":%llu,\"handles\":%llu,\"result\":%ld,\"pending\":%llu,"
         "\"calls\":%u,\"destroyed\":%u,\"forwarded\":%u,\"retired\":[%llu,%llu,%llu],"
         "\"clock_before\":%llu,\"clock_after\":%llu}\n",
         inactive.pending, adopted.pending, waiting.native_waiters, waiting.retained_blocks,
         (long)(intptr_t)result, final.pending, atomic_load(&calls), atomic_load(&destroyed), atomic_load(&forwarded),
         retired.callers, retired.native_waiters, retired.retained_blocks, before, now_ns());
  atomic_store(&calls, 0); atomic_store(&destroyed, 0); atomic_store(&forwarded, 0);
}

static void *control(void *unused) {
  FC_PROBE_TIMING("control-entry");
  (void)unused;
  @autoreleasepool {
    require(now_ns() == 0);
    advance(4999000000);
    activate_waiting_work();
    dispatch_queue_t queue = dispatch_queue_create("s18-background-wait",
      dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_BACKGROUND, 0));
    uint64_t before = now_ns(), start = host_ticks();
    dispatch_async(queue, ^{
      // Exercise repeated worker rescheduling without consuming virtual time.
      resume_background_work(NULL);
    });
    int result = wait_idle(2000000000);
    struct fc_dispatch_activity activity;
    read_activity(&activity);
    FC_PROBE_TIMING("report");
    printf("{\"phase\":\"progress\",\"result\":%d,\"resumes\":%u,\"pending\":%llu,"
           "\"clock_before\":%llu,\"clock_after\":%llu,\"host_ns\":%llu}\n",
           result, atomic_load(&resumes), activity.pending, before, now_ns(), elapsed_ns(start));
    require(result == 0 && atomic_load(&resumes) == 12 && !activity.pending && now_ns() == before);

    // The function predecessor has no native block handle. Waiting on the later
    // serial blocks must still let it finish and include their main-queue children.
    atomic_store(&resumes, 0);
    start = host_ticks();
    dispatch_async_f(queue, NULL, resume_background_work);
    for (unsigned i = 0; i < 2; i++) {
      dispatch_async(queue, ^{
        require(atomic_load(&resumes) == 12);
        dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&forwarded, 1); });
      });
    }
    result = wait_idle(2000000000);
    read_activity(&activity);
    FC_PROBE_TIMING("report");
    printf("{\"phase\":\"predecessor\",\"result\":%d,\"resumes\":%u,\"forwarded\":%u,\"pending\":%llu,"
           "\"clock_before\":%llu,\"clock_after\":%llu,\"host_ns\":%llu}\n",
           result, atomic_load(&resumes), atomic_load(&forwarded), activity.pending,
           before, now_ns(), elapsed_ns(start));
    require(result == 0 && atomic_load(&resumes) == 12 && atomic_load(&forwarded) == 2);
    require(!activity.pending && now_ns() == before);

    for (unsigned iteration = 0; iteration < 4; iteration++) {
      dispatch_suspend(queue);
      @autoreleasepool {
        WaitCapture *capture = [WaitCapture new];
        dispatch_async(queue, ^{ [capture invoke]; });
      }
      pthread_t first, second;
      require(pthread_create(&first, NULL, waiter, NULL) == 0);
      (void)await_callers(1, 1);
      require(pthread_create(&second, NULL, waiter, NULL) == 0);
      struct fc_dispatch_wait_state waiting = await_callers(2, 1);
      dispatch_resume(queue);
      void *a = NULL, *b = NULL;
      require(pthread_join(first, &a) == 0);
      require(pthread_join(second, &b) == 0);
      struct fc_dispatch_wait_state final;
      read_wait_state(&final);
      require(!a && !b && !final.callers && !final.native_waiters && !final.retained_blocks);
      require(atomic_load(&calls) == iteration + 1 && atomic_load(&destroyed) == iteration + 1);
      require(now_ns() == before);
      FC_PROBE_TIMING("report");
      printf("{\"phase\":\"concurrent\",\"iteration\":%u,\"callers\":%llu,\"native_waiters\":%llu,"
             "\"results\":[%ld,%ld],\"remaining\":[%llu,%llu,%llu],\"calls\":%u,\"destroyed\":%u,"
             "\"clock_ns\":%llu}\n", iteration, waiting.callers, waiting.native_waiters,
             (long)(intptr_t)a, (long)(intptr_t)b, final.callers, final.native_waiters,
             final.retained_blocks, atomic_load(&calls), atomic_load(&destroyed), now_ns());
    }
    CFRunLoopStop(CFRunLoopGetMain());
  }
  FC_PROBE_TIMING("control-return");
  return NULL;
}

int main(void) {
  FC_PROBE_TIMING("main-entry");
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    start_tracking = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    read_wait_state = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_state_read");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    host_ticks = dlsym(RTLD_DEFAULT, "fc_real_abs");
    now_ns = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    advance = dlsym(RTLD_DEFAULT, "fc_advance_ns");
    require(start_tracking && wait_idle && read_wait_state && read_activity && host_ticks && now_ns && advance);
    require(mach_timebase_info(&timebase) == KERN_SUCCESS);
    pthread_t controller;
    require(pthread_create(&controller, NULL, control, NULL) == 0);
    CFRunLoopRun();
    require(pthread_join(controller, NULL) == 0);
  }
  FC_PROBE_TIMING("main-return");
  return 0;
}
