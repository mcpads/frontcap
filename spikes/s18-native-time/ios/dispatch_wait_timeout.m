// Standalone timeout diagnostic: no argument selects a block; function selects async_f.
// history chains 24 selected native waits before holding the last callback.
// A suspended queue must time out without advancing, then drain after resume.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <string.h>
#include <mach/mach_time.h>
#include "dispatch_activity.h"
#include "probe_assert.h"
static _Atomic unsigned calls;
static void callback(void *unused) { atomic_fetch_add(&calls, 1); }
static void (*read_wait_state)(struct fc_dispatch_wait_state *);
static uint64_t (*host_ticks)(void);
static mach_timebase_info_data_t timebase;
static dispatch_semaphore_t final_gate;
static void submit_chain(dispatch_queue_t queue, unsigned index) {
  dispatch_async(queue, ^{
    uint64_t started = host_ticks();
    struct fc_dispatch_wait_state state;
    do {
      read_wait_state(&state);
      require((__uint128_t)(host_ticks() - started) * timebase.numer / timebase.denom < 3000000000);
      if (state.native_calls < index) usleep(1000);
    } while (state.native_calls < index);
    // Do not let scheduling combine several completions into a single wait.
    require(state.native_calls == index && state.native_waiters == 1);
    if (index < 24) submit_chain(queue, index + 1);
    else dispatch_semaphore_wait(final_gate, DISPATCH_TIME_FOREVER);
    atomic_fetch_add(&calls, 1);
  });
}
static void *control(void *arg) {
  @autoreleasepool {
    int (*start)(void) = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
    int (*advance)(uint64_t, uint64_t) = dlsym(RTLD_DEFAULT, "fc_dispatch_advance_when_idle");
    int (*wait_idle)(uint64_t) = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
    uint64_t (*now)(void) = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(start && advance && wait_idle && now);
    dispatch_queue_t queue = dispatch_queue_create("wait-history-suspended", DISPATCH_QUEUE_SERIAL);
    BOOL history = arg && !strcmp(arg, "history");
    if (!history) dispatch_suspend(queue);
    require(start() == 0);
    if (history) {
      read_wait_state = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_state_read");
      host_ticks = dlsym(RTLD_DEFAULT, "fc_real_abs");
      require(read_wait_state && host_ticks && mach_timebase_info(&timebase) == KERN_SUCCESS);
      final_gate = dispatch_semaphore_create(0);
      submit_chain(queue, 1);
    } else if (arg) dispatch_async_f(queue, NULL, callback);
    else dispatch_async(queue, ^{ callback(NULL); });
    uint64_t before = now();
    require(advance(1000000, history ? 2000000000 : 100000000) == ETIMEDOUT);
    require(now() == before && atomic_load(&calls) == (history ? 23 : 0));
    if (history) dispatch_semaphore_signal(final_gate);
    else dispatch_resume(queue);
    require(wait_idle(2000000000) == 0);
    require(atomic_load(&calls) == (history ? 24 : 1));
    require(advance(1000000, 2000000000) == 0 && now() == before + 1000000);
    puts("timeout preserved; resumed work drained; advance succeeded");
  }
  return NULL;
}
int main(int argc, char **argv) {
  pthread_t thread;
  require(pthread_create(&thread, NULL, control, argc > 1 ? argv[1] : NULL) == 0);
  require(pthread_join(thread, NULL) == 0);
}
