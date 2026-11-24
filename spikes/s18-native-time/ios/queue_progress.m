// Opt-in scheduling investigation; excluded from run.sh and acceptance checks.
// Usage: queue_progress real|manual|tracked
//        background-first|default-first|delayed-default|background-only
// Compare first and warm callbacks in one process, with a two-second entry budget.
// Exit: 0 observed progress; 1 callback/drain failure; 3 setup/observation error.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <string.h>
#include <unistd.h>
#include "probe_assert.h"

static uint64_t (*host_ticks)(void);
static uint64_t (*now_ns)(void);
static int (*wait_idle)(uint64_t);
static mach_timebase_info_data_t timebase;
static const uint64_t budget_ns = 2000000000;
static const char *mode, *order;
static int outcome;
static unsigned observation_errors;

struct callback {
  uint64_t submitted, returned, id;
  unsigned qos;
  _Atomic uint64_t entry;
  _Atomic bool complete;
  bool resolved;
};
// A late callback must not outlive stack storage or write into the next round.
static struct callback callbacks[2][2];

static uint64_t ns_between(uint64_t start, uint64_t end) {
  return (uint64_t)((__uint128_t)(end - start) * timebase.numer / timebase.denom);
}
static uint64_t elapsed_ns(uint64_t start) { return ns_between(start, host_ticks()); }

// Sequential observations, never an atomic scheduler snapshot. No thread is
// suspended. Limit queries to 64 threads / 20 ms and release all Mach rights.
static void snapshot(unsigned round, const char *reason, uint64_t start) {
  uint64_t began = host_ticks();
  host_cpu_load_info_data_t cpu = {0};
  mach_msg_type_number_t size = HOST_CPU_LOAD_INFO_COUNT;
  mach_port_t host = mach_host_self();
  kern_return_t cpu_result = host_statistics(host, HOST_CPU_LOAD_INFO, (host_info_t)&cpu, &size);
  if (cpu_result == KERN_SUCCESS && size != HOST_CPU_LOAD_INFO_COUNT) cpu_result = KERN_FAILURE;
  require(mach_port_deallocate(mach_task_self(), host) == KERN_SUCCESS);
  thread_act_array_t threads = NULL;
  mach_msg_type_number_t count = 0;
  kern_return_t result = task_threads(mach_task_self(), &threads, &count);
  printf("{\"event\":\"snapshot\",\"round\":%u,\"reason\":\"%s\",\"host_ns\":%llu,"
         "\"query_result\":%d,\"thread_count\":%u,\"cpu_result\":%d,\"cpu_ticks\":[%u,%u,%u,%u],\"threads\":[",
         round, reason, ns_between(start, began), result, count, cpu_result,
         cpu.cpu_ticks[CPU_STATE_USER], cpu.cpu_ticks[CPU_STATE_SYSTEM],
         cpu.cpu_ticks[CPU_STATE_IDLE], cpu.cpu_ticks[CPU_STATE_NICE]);
  unsigned captured = 0, errors = 0;
  if (result == KERN_SUCCESS) {
    for (mach_msg_type_number_t i = 0; i < count; i++) {
      if (captured < 64 && elapsed_ns(began) < 20000000) {
        thread_identifier_info_data_t identity = {0};
        thread_basic_info_data_t basic = {0};
        thread_extended_info_data_t extended = {0};
        size = THREAD_IDENTIFIER_INFO_COUNT;
        kern_return_t id_result = thread_info(threads[i], THREAD_IDENTIFIER_INFO, (thread_info_t)&identity, &size);
        if (id_result == KERN_SUCCESS && size != THREAD_IDENTIFIER_INFO_COUNT) id_result = KERN_FAILURE;
        size = THREAD_BASIC_INFO_COUNT;
        kern_return_t basic_result = thread_info(threads[i], THREAD_BASIC_INFO, (thread_info_t)&basic, &size);
        if (basic_result == KERN_SUCCESS && size != THREAD_BASIC_INFO_COUNT) basic_result = KERN_FAILURE;
        size = THREAD_EXTENDED_INFO_COUNT;
        kern_return_t extended_result = thread_info(threads[i], THREAD_EXTENDED_INFO, (thread_info_t)&extended, &size);
        if (extended_result == KERN_SUCCESS && size != THREAD_EXTENDED_INFO_COUNT) extended_result = KERN_FAILURE;
        errors += id_result != KERN_SUCCESS || basic_result != KERN_SUCCESS || extended_result != KERN_SUCCESS;
        printf("%s{\"id\":%llu,\"id_result\":%d,\"basic_result\":%d,\"extended_result\":%d,"
               "\"state\":%d,\"suspend\":%d,\"cpu\":%d,\"priority\":%d,\"base_priority\":%d,"
               "\"user_us\":%llu,\"system_us\":%llu}", captured ? "," : "", identity.thread_id,
               id_result, basic_result, extended_result, basic.run_state, basic.suspend_count,
               basic.cpu_usage, extended.pth_curpri, extended.pth_priority,
               (uint64_t)basic.user_time.seconds * 1000000 + basic.user_time.microseconds,
               (uint64_t)basic.system_time.seconds * 1000000 + basic.system_time.microseconds);
        captured++;
      }
      if (mach_port_deallocate(mach_task_self(), threads[i]) != KERN_SUCCESS) errors++;
    }
    if (vm_deallocate(mach_task_self(), (vm_address_t)threads, count * sizeof(*threads)) != KERN_SUCCESS) errors++;
  }
  printf("],\"captured\":%u,\"errors\":%u,\"duration_ns\":%llu}\n", captured, errors, elapsed_ns(began));
  observation_errors += result != KERN_SUCCESS || cpu_result != KERN_SUCCESS || errors || captured != count;
}

static void submit(dispatch_queue_t queue, struct callback *callback) {
  callback->submitted = host_ticks();
  dispatch_async(queue, ^{
    // Timestamp before any mutex, logging, thread query or observer handshake.
    atomic_store_explicit(&callback->entry, host_ticks(), memory_order_release);
    require(pthread_threadid_np(NULL, &callback->id) == 0);
    callback->qos = qos_class_self();
    atomic_store_explicit(&callback->complete, true, memory_order_release);
  });
  callback->returned = host_ticks();
}

static void *run_controller(void *unused) {
  (void)unused;
  @autoreleasepool {
    uint64_t controller = 0;
    require(pthread_threadid_np(NULL, &controller) == 0);
    printf("{\"event\":\"controller\",\"id\":%llu,\"qos\":%u}\n", controller, qos_class_self());
    dispatch_queue_t queues[2] = {
      dispatch_queue_create("probe-background-progress", dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_BACKGROUND, 0)),
      dispatch_queue_create("probe-default-progress", dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_DEFAULT, 0))
    };
    if (!strcmp(mode, "tracked")) {
      int (*start)(void) = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
      require(start && start() == 0);
    }
    for (unsigned round = 0; round < 2 && !outcome; round++) {
      struct callback *pair = callbacks[round];
      uint64_t start = host_ticks(), clock_before = mach_absolute_time();
      snapshot(round, "before_submit", start);
      bool default_wanted = strcmp(order, "background-only") != 0;
      if (!strcmp(order, "default-first")) submit(queues[1], &pair[1]);
      submit(queues[0], &pair[0]);
      if (!strcmp(order, "background-first")) submit(queues[1], &pair[1]);
      unsigned remaining = default_wanted ? 2 : 1;
      uint64_t next_snapshot = 0;
      while (remaining) {
        if (default_wanted && !pair[1].submitted && elapsed_ns(pair[0].submitted) >= 500000000) {
          snapshot(round, "before_default", start);
          submit(queues[1], &pair[1]);
        }
        for (unsigned i = 0; i < 2; i++) {
          struct callback *callback = &pair[i];
          if (!callback->submitted || callback->resolved) continue;
          bool complete = atomic_load_explicit(&callback->complete, memory_order_acquire);
          uint64_t observed = host_ticks();
          if (!complete && ns_between(callback->submitted, observed) < budget_ns) continue;
          uint64_t entry = atomic_load_explicit(&callback->entry, memory_order_acquire);
          uint64_t entry_ns = entry ? ns_between(callback->submitted, entry) : 0;
          int result = complete && entry && entry_ns < budget_ns ? 0 : ETIMEDOUT;
          const char *failed_wait = !result ? "none" : (!entry || entry_ns >= budget_ns) ? "entry" : "completion";
          unsigned expected_qos = i ? QOS_CLASS_DEFAULT : QOS_CLASS_BACKGROUND;
          if (result) outcome = 1;
          else if (callback->qos != expected_qos && !outcome) outcome = 3;
          callback->resolved = true;
          remaining--;
          printf("{\"event\":\"callback\",\"round\":%u,\"queue\":\"%s\",\"result\":%d,"
                 "\"submitted_ns\":%llu,\"submit_call_ns\":%llu,\"entry_ns\":%llu,\"observed_ns\":%llu,"
                 "\"entered\":%s,\"complete\":%s,\"id\":%llu,\"qos\":%u,\"failed_wait\":\"%s\"}\n", round,
                 i ? "default" : "background", result, ns_between(start, callback->submitted),
                 ns_between(callback->submitted, callback->returned), entry_ns,
                 ns_between(callback->submitted, observed), entry ? "true" : "false",
                 complete ? "true" : "false", complete ? callback->id : 0, complete ? callback->qos : 0, failed_wait);
          if (result) {
            snapshot(round, "entry_deadline", start);
            if (wait_idle) (void)wait_idle(1);
          }
        }
        if (!remaining) break;
        if (elapsed_ns(start) >= next_snapshot) {
          snapshot(round, "pending", start);
          next_snapshot = elapsed_ns(start) + 100000000;
        }
        usleep(10000);
      }
      snapshot(round, "after_callbacks", start);
      int drain = wait_idle ? wait_idle(outcome ? 1 : budget_ns) : 0;
      uint64_t clock_after = mach_absolute_time();
      if (drain) outcome = 1;
      if (now_ns && (clock_after != clock_before || now_ns() != 4999000000) && !outcome) outcome = 3;
      printf("{\"event\":\"round_end\",\"round\":%u,\"host_ns\":%llu,\"drain\":%d,"
             "\"clock_before\":%llu,\"clock_after\":%llu,\"exit_code\":%d}\n",
             round, elapsed_ns(start), drain, clock_before, clock_after, outcome);
      if (!outcome && round == 0) usleep(100000);
    }
  }
  return NULL;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 3 || (strcmp(argv[1], "real") && strcmp(argv[1], "manual") && strcmp(argv[1], "tracked")) ||
      (strcmp(argv[2], "background-first") && strcmp(argv[2], "default-first") &&
       strcmp(argv[2], "delayed-default") && strcmp(argv[2], "background-only"))) return 3;
  mode = argv[1]; order = argv[2];
  host_ticks = dlsym(RTLD_DEFAULT, "fc_real_abs");
  if (!strcmp(mode, "real")) {
    require(!host_ticks);
    host_ticks = mach_absolute_time;
  } else {
    int (*manual)(void) = dlsym(RTLD_DEFAULT, "fc_manual_clock_enabled");
    void (*advance)(uint64_t) = dlsym(RTLD_DEFAULT, "fc_advance_ns");
    now_ns = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(host_ticks && manual && manual() && advance && now_ns && now_ns() == 0);
    advance(4999000000);
    if (!strcmp(mode, "tracked")) { wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle"); require(wait_idle); }
  }
  require(mach_timebase_info(&timebase) == KERN_SUCCESS);
  uint64_t main_id = 0;
  require(pthread_threadid_np(NULL, &main_id) == 0);
  printf("{\"event\":\"start\",\"pid\":%d,\"main_id\":%llu,\"mode\":\"%s\",\"order\":\"%s\",\"budget_ns\":%llu}\n",
         getpid(), main_id, mode, order, budget_ns);
  pthread_t controller;
  require(pthread_create(&controller, NULL, run_controller, NULL) == 0);
  require(pthread_join(controller, NULL) == 0);
  if (observation_errors && !outcome) outcome = 3;
  printf("{\"event\":\"end\",\"exit_code\":%d,\"observation_errors\":%u}\n", outcome, observation_errors);
  return outcome;
}
