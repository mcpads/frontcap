// Completion must include queued work and transitive submissions, with no fixture callback counts.
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
#include <mach/mach_time.h>

static int (*start_tracking)(void);
static uint64_t (*real_abs)(void);
static int (*wait_idle)(uint64_t);
static int (*advance_when_idle)(uint64_t, uint64_t);
static void (*read_activity)(struct fc_dispatch_activity *);
static dispatch_queue_t first, second, concurrent, suspended;
static int entered[2], release_work[2];
static _Atomic int chain_nodes, leaves, resumed, writers, function_effects;
static _Atomic int block_barrier_seen, function_barrier_seen, self_wait_result;
static CFTimeInterval origin;

static void read_signal(int descriptor) {
  struct pollfd fd = {.fd = descriptor, .events = POLLIN};
  char signal;
  FC_REQUIRE(poll(&fd, 1, 2000) == 1 && read(descriptor, &signal, 1) == 1, 3);
}

static void report(const char *phase, int result) {
  struct fc_dispatch_activity state;
  read_activity(&state);
  printf("{\"phase\":\"%s\",\"idle\":%s,\"timed_out\":%s,\"error\":%d,"
         "\"pending\":%llu,\"submitted\":%llu,\"completed\":%llu,\"virtual_s\":%.6f,"
         "\"chain_nodes\":%d,\"leaves\":%d,\"resumed\":%d,\"writers\":%d,"
         "\"function_effects\":%d,\"block_barrier_seen\":%d,\"function_barrier_seen\":%d,"
         "\"self_wait_rejected\":%s}\n", phase, result == 0 ? "true" : "false",
         result == ETIMEDOUT ? "true" : "false", result,
         (unsigned long long)state.pending, (unsigned long long)state.submitted, (unsigned long long)state.completed,
         CACurrentMediaTime() - origin, atomic_load(&chain_nodes), atomic_load(&leaves), atomic_load(&resumed),
         atomic_load(&writers), atomic_load(&function_effects), atomic_load(&block_barrier_seen),
         atomic_load(&function_barrier_seen), atomic_load(&self_wait_result) == EDEADLK ? "true" : "false");
}

static void chain(int depth) {
  dispatch_async(depth % 2 ? first : second, ^{
    atomic_fetch_add(&chain_nodes, 1);
    if (depth < 32) chain(depth + 1);
    else dispatch_async(dispatch_get_main_queue(), ^{ atomic_fetch_add(&leaves, 1); });
  });
}

static void write_value(void *context) {
  atomic_fetch_add(&writers, 1);
  atomic_fetch_add(&function_effects, 1);
}
static void function_work(void *context) { atomic_fetch_add(&function_effects, 1); }
static void barrier_work(void *context) { atomic_store(&function_barrier_seen, atomic_load(&writers)); }

static void *control(void *unused) {
  @autoreleasepool {
    dispatch_async(first, ^{
      FC_REQUIRE(write(entered[1], "r", 1) == 1, 4);
      read_signal(release_work[0]);
      chain(1);
    });
    read_signal(entered[0]);
    report("running", wait_idle(20000000));
    FC_REQUIRE(write(release_work[1], "r", 1) == 1, 4);
    int result = wait_idle(2000000000);
    report("descendants_finished", result);
    FC_REQUIRE(!result, 5);

    dispatch_suspend(suspended);
    dispatch_async(suspended, ^{ atomic_fetch_add(&resumed, 1); });
    report("queued", wait_idle(20000000));
    dispatch_resume(suspended);
    result = wait_idle(2000000000);
    report("resumed", result);
    FC_REQUIRE(!result, 5);

    dispatch_group_t group = dispatch_group_create();
    dispatch_group_async(group, concurrent, ^{ atomic_fetch_add(&writers, 1); });
    dispatch_group_async_f(group, concurrent, NULL, write_value);
    dispatch_barrier_async(concurrent, ^{ atomic_store(&block_barrier_seen, atomic_load(&writers)); });
    dispatch_barrier_async_f(concurrent, NULL, barrier_work);
    dispatch_async_f(second, NULL, function_work);
    result = wait_idle(2000000000);
    report("barriers_finished", result);
    FC_REQUIRE(!result, 5);

    dispatch_async(second, ^{ atomic_store(&self_wait_result, wait_idle(0) == EDEADLK ? advance_when_idle(1000000000, 0) : 0); });
    result = wait_idle(2000000000);
    report("self_wait", result);
    FC_REQUIRE(!result, 5);
    // An idle observation becomes stale when an external producer submits again.
    FC_REQUIRE(!wait_idle(0), 5);
    dispatch_suspend(suspended);
    dispatch_async(suspended, ^{ atomic_fetch_add(&resumed, 1); });
    int blocked = advance_when_idle(1000000000, 20000000);
    double blocked_at = CACurrentMediaTime() - origin;
    dispatch_resume(suspended);
    int advanced = advance_when_idle(1000000000, 2000000000);
    double advanced_at = CACurrentMediaTime() - origin;
    printf("{\"advance_boundary\":true,\"busy_rejected\":%s,\"blocked_at\":%.6f,"
           "\"advanced\":%s,\"advanced_at\":%.6f,\"effects\":%d}\n",
           blocked == ETIMEDOUT ? "true" : "false", blocked_at,
           advanced == 0 ? "true" : "false", advanced_at, atomic_load(&resumed));
    FC_REQUIRE(!advanced, 5);

    // Host wait budgets must not use the application clock, even a day ahead.
    FC_REQUIRE(!advance_when_idle(86400000000000ULL, 2000000000), 5);
    dispatch_suspend(suspended);
    dispatch_async(suspended, ^{ atomic_fetch_add(&resumed, 1); });
    uint64_t began = real_abs();
    int timeout = advance_when_idle(1000000000, 20000000);
    uint64_t ended = real_abs();
    mach_timebase_info_data_t timebase;
    mach_timebase_info(&timebase);
    uint64_t elapsed = (uint64_t)((__uint128_t)(ended - began) * timebase.numer / timebase.denom);
    double unchanged = CACurrentMediaTime() - origin;
    struct fc_dispatch_activity state;
    read_activity(&state);
    dispatch_resume(suspended);
    int recovered = wait_idle(2000000000);
    printf("{\"timeout_clock\":true,\"timed_out\":%s,\"elapsed_real_ns\":%llu,"
           "\"virtual_s\":%.6f,\"pending\":%llu,\"recovered\":%s,\"effects\":%d}\n",
           timeout == ETIMEDOUT ? "true" : "false", elapsed, unchanged, state.pending,
           recovered == 0 ? "true" : "false", atomic_load(&resumed));
    FC_REQUIRE(!recovered, 5);
    CFRunLoopStop(CFRunLoopGetMain());
  }
  return NULL;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  @autoreleasepool {
    real_abs = dlsym(RTLD_DEFAULT, "fc_real_abs");
    start_tracking = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    advance_when_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    read_activity = dlsym(RTLD_DEFAULT, "fc_dispatch_activity_read");
    if (!real_abs || !start_tracking || !wait_idle || !advance_when_idle || !read_activity) return 2;
    if (pipe(entered) || pipe(release_work)) return 2;
    first = dispatch_queue_create("s18-first", DISPATCH_QUEUE_SERIAL);
    second = dispatch_queue_create("s18-second", DISPATCH_QUEUE_SERIAL);
    concurrent = dispatch_queue_create("s18-concurrent", DISPATCH_QUEUE_CONCURRENT);
    suspended = dispatch_queue_create("s18-suspended", DISPATCH_QUEUE_SERIAL);
    origin = CACurrentMediaTime();
    if (start_tracking()) return 2;
    printf("{\"main_wait_rejected\":%s}\n", wait_idle(0) == EDEADLK && advance_when_idle(1000000000, 0) == EDEADLK ? "true" : "false");
    pthread_t controller;
    if (pthread_create(&controller, NULL, control, NULL)) return 2;
    CFRunLoopRun();
    pthread_join(controller, NULL);
    close(entered[0]); close(entered[1]); close(release_work[0]); close(release_work[1]);
  }
  return 0;
}
