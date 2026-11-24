// Opt-in causal probe, excluded from run.sh and the native acceptance suite.
// Observe worker retirement, queue reassignment, and new worker creation.
// Usage: queue_retirement real|manual|tracked [retirement-observation-ms]
// Exit: 0 completed after observed retirement and new worker creation;
//       1 callback/drain failure; 2 condition not observed; 3 observation error.
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "dispatch_activity.h"
#include "probe_assert.h"

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static uint64_t (*host_ticks)(void);
static uint64_t (*now_ns)(void);
static int (*wait_idle)(uint64_t);
static mach_timebase_info_data_t timebase;
static const char *mode;
static uint64_t observation_ns = 30000000000ull;
static uint64_t worker_id;
static unsigned worker_qos;
static bool entered, released, acknowledged;
static bool peer_entered, peer_released, peer_acknowledged;
static uint64_t peer_id;
static unsigned peer_qos;
static uint64_t worker_entry_ns, worker_ack_ns;
static uint64_t peer_submit_ticks, peer_entry_ns, peer_ack_ns;
static dispatch_queue_t peer_queue;
static int outcome;

struct thread_inventory { uint64_t ids[128]; unsigned count; };

static uint64_t elapsed_ns(uint64_t start) {
  return (uint64_t)((__uint128_t)(host_ticks() - start) * timebase.numer / timebase.denom);
}

// The caller holds lock. Never derive a timeout from an interposed clock.
static int wait_flag(bool *flag, uint64_t start, uint64_t budget) {
  while (!*flag) {
    uint64_t elapsed = elapsed_ns(start);
    if (elapsed >= budget) return ETIMEDOUT;
    uint64_t remaining = budget - elapsed;
    struct timespec interval = {remaining / 1000000000, remaining % 1000000000};
    int result = pthread_cond_timedwait_relative_np(&changed, &lock, &interval);
    if (result && result != ETIMEDOUT) return result;
  }
  return elapsed_ns(start) >= budget ? ETIMEDOUT : 0;
}

// A missing ID only means retirement when every thread was queried successfully.
// Release every send right and the returned array on all paths. No thread is suspended.
static int read_thread_inventory(struct thread_inventory *inventory) {
  thread_act_array_t threads = NULL;
  mach_msg_type_number_t count = 0;
  kern_return_t result = task_threads(mach_task_self(), &threads, &count);
  if (result != KERN_SUCCESS) return result;
  inventory->count = count;
  if (count > 128) result = KERN_RESOURCE_SHORTAGE;
  for (mach_msg_type_number_t i = 0; i < count; i++) {
    thread_identifier_info_data_t info = {0};
    mach_msg_type_number_t size = THREAD_IDENTIFIER_INFO_COUNT;
    kern_return_t query = thread_info(threads[i], THREAD_IDENTIFIER_INFO,
                                      (thread_info_t)&info, &size);
    if (query != KERN_SUCCESS || size != THREAD_IDENTIFIER_INFO_COUNT)
      result = query != KERN_SUCCESS ? query : KERN_FAILURE;
    else if (i < 128) inventory->ids[i] = info.thread_id;
    kern_return_t release = mach_port_deallocate(mach_task_self(), threads[i]);
    if (release != KERN_SUCCESS) result = release;
  }
  kern_return_t release = vm_deallocate(mach_task_self(), (vm_address_t)threads,
                                       count * sizeof(*threads));
  return release != KERN_SUCCESS ? release : result;
}

static bool inventory_contains(const struct thread_inventory *inventory, uint64_t wanted) {
  for (unsigned i = 0; i < inventory->count; i++) if (inventory->ids[i] == wanted) return true;
  return false;
}

static int worker_present(uint64_t wanted, bool *present, unsigned *count_out) {
  struct thread_inventory inventory = {0};
  int result = read_thread_inventory(&inventory);
  *count_out = inventory.count;
  *present = result ? true : inventory_contains(&inventory, wanted);
  return result;
}

// Called with lock held; the previous peer callback has drained before reuse.
static void submit_peer(void) {
  peer_entered = peer_released = peer_acknowledged = false;
  peer_id = peer_qos = 0;
  peer_entry_ns = peer_ack_ns = 0;
  peer_submit_ticks = host_ticks();
  uint64_t submitted = peer_submit_ticks;
  dispatch_async(peer_queue, ^{
    uint64_t entry_ns = elapsed_ns(submitted);
    pthread_mutex_lock(&lock);
    require(pthread_threadid_np(NULL, &peer_id) == 0);
    peer_qos = qos_class_self();
    peer_entry_ns = entry_ns;
    peer_entered = true;
    pthread_cond_broadcast(&changed);
    while (!peer_released) pthread_cond_wait(&changed, &lock);
    peer_ack_ns = elapsed_ns(submitted);
    peer_acknowledged = true;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
  });
}

static int submit_callback(dispatch_queue_t queue, bool hold, bool creation) {
  pthread_mutex_lock(&lock);
  entered = acknowledged = false;
  released = !hold;
  worker_id = worker_qos = 0;
  worker_entry_ns = worker_ack_ns = 0;
  uint64_t start = host_ticks(), clock_before = mach_absolute_time();
  dispatch_async(queue, ^{
    uint64_t entry_ns = elapsed_ns(start);
    pthread_mutex_lock(&lock);
    require(pthread_threadid_np(NULL, &worker_id) == 0);
    worker_qos = qos_class_self();
    worker_entry_ns = entry_ns;
    entered = true;
    pthread_cond_broadcast(&changed);
    while (!released) pthread_cond_wait(&changed, &lock);
    worker_ack_ns = elapsed_ns(start);
    acknowledged = true;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);
  });
  const char *wait_stage = "entry";
  int result = wait_flag(&entered, start, 2000000000);
  if (hold && !result) {
    bool present = false;
    unsigned count = 0;
    int query = worker_present(worker_id, &present, &count);
    printf("{\"event\":\"held_worker\",\"id\":%llu,\"present\":%s,"
           "\"query_result\":%d,\"threads\":%u}\n",
           worker_id, present ? "true" : "false", query, count);
    if (query || !present) outcome = 3;
    // XNU keeps its last idle worker. Start a second worker while the first is
    // held, then release the first before the peer so it can become killable.
    submit_peer();
    wait_stage = "peer_entry";
    result = wait_flag(&peer_entered, start, 2000000000);
    printf("{\"event\":\"peer\",\"id\":%llu,\"qos\":%u,\"result\":%d}\n",
           peer_id, peer_qos, result);
    if (!result && (peer_id == worker_id || peer_qos != QOS_CLASS_BACKGROUND)) outcome = 3;
  }
  released = true;
  uint64_t release_ns = elapsed_ns(start);
  pthread_cond_broadcast(&changed);
  if (!result) {
    wait_stage = "acknowledgement";
    result = wait_flag(&acknowledged, start, 2000000000);
  }
  uint64_t elapsed = elapsed_ns(start), clock_after = mach_absolute_time();
  uint64_t id = worker_id;
  unsigned qos = worker_qos;
  uint64_t entry_ns = worker_entry_ns, ack_ns = worker_ack_ns;
  bool saw_entry = entered, saw_ack = acknowledged;
  pthread_mutex_unlock(&lock);
  // Snapshot a failed predicate before releasing the peer, while queued work is
  // still distinguishable from the intentional hold. This does not extend its budget.
  if (result && wait_idle) (void)wait_idle(1);
  uint64_t peer_release_ns = 0, peer_entered_ns = 0, peer_acked_ns = 0, peer_submit_ns = 0;
  if (hold || creation) {
    // This host delay only biases parking order; the inventory below must still
    // prove that the target worker exited. It is not evidence of retirement.
    if (hold && !result) usleep(100000);
    pthread_mutex_lock(&lock);
    peer_released = true;
    peer_release_ns = elapsed_ns(start);
    pthread_cond_broadcast(&changed);
    if (!result) {
      wait_stage = "peer_acknowledgement";
      result = wait_flag(&peer_acknowledged, start, 2000000000);
    }
    peer_entered_ns = peer_entry_ns;
    peer_acked_ns = peer_ack_ns;
    if (hold && peer_submit_ticks) peer_submit_ns = (uint64_t)((__uint128_t)(peer_submit_ticks - start) * timebase.numer / timebase.denom);
    pthread_mutex_unlock(&lock);
  }
  uint64_t protocol_ns = elapsed_ns(start);
  // Preserve timeout diagnostics without granting a second callback budget.
  int drain = wait_idle ? wait_idle(result ? 1 : 2000000000) : 0;
  printf("{\"event\":\"callback\",\"phase\":\"%s\",\"mode\":\"%s\","
         "\"result\":%d,\"host_ns\":%llu,\"id\":%llu,\"qos\":%u,"
         "\"protocol_host_ns\":%llu,"
         "\"clock_before\":%llu,\"clock_after\":%llu,\"drain\":%d,"
         "\"failed_wait\":\"%s\",\"entered\":%s,\"acknowledged\":%s,"
         "\"entry_ns\":%llu,\"release_ns\":%llu,\"ack_ns\":%llu,"
         "\"peer_submit_ns\":%llu,\"peer_entry_ns\":%llu,\"peer_ack_ns\":%llu,\"peer_release_ns\":%llu}\n",
         hold ? "warmup" : creation ? "while_peer_held" : "after_retirement", mode, result, elapsed, id, qos,
         protocol_ns, clock_before, clock_after, drain, result ? wait_stage : "none", saw_entry ? "true" : "false",
         saw_ack ? "true" : "false", entry_ns, release_ns, ack_ns,
         peer_submit_ns, peer_entered_ns, peer_acked_ns, peer_release_ns);
  if (result || drain || qos != QOS_CLASS_BACKGROUND) return 1;
  if (now_ns && (clock_before != clock_after || now_ns() != 4999000000)) return 3;
  return outcome;
}

static void *run_controller(void *unused) {
  (void)unused;
  @autoreleasepool {
    dispatch_queue_t queue = dispatch_queue_create("probe-background-retirement",
      dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_BACKGROUND, 0));
    peer_queue = dispatch_queue_create("probe-background-retirement-peer",
      dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_BACKGROUND, 0));
    if (!strcmp(mode, "tracked")) {
      int (*start)(void) = dlsym(RTLD_DEFAULT, "fc_dispatch_tracking_start");
      require(start && start() == 0);
    }
    outcome = submit_callback(queue, true, false);
    if (outcome) return NULL;
    uint64_t previous = worker_id, start = host_ticks();
    bool retired = false;
    unsigned samples = 0, query_errors = 0;
    while (elapsed_ns(start) < observation_ns) {
      bool present = true;
      unsigned count = 0;
      int result = worker_present(previous, &present, &count);
      samples++;
      if (result) query_errors++;
      if (result || !present || samples == 1) {
        printf("{\"event\":\"inventory\",\"sample\":%u,\"host_ns\":%llu,"
               "\"id\":%llu,\"present\":%s,\"query_result\":%d,\"threads\":%u}\n",
               samples, elapsed_ns(start), previous, present ? "true" : "false", result, count);
      }
      if (!result && !present) { retired = true; break; }
      usleep(100000);
    }
    printf("{\"event\":\"retirement\",\"observed\":%s,\"host_ns\":%llu,"
           "\"samples\":%u,\"query_errors\":%u,\"previous_id\":%llu}\n",
           retired ? "true" : "false", elapsed_ns(start), samples, query_errors, previous);
    if (!retired) { outcome = query_errors == samples ? 3 : 2; return NULL; }
    outcome = submit_callback(queue, false, false);
    if (!outcome && worker_id == previous) outcome = 3;
    if (outcome) return NULL;

    // Hold the remaining worker on another queue. Snapshot *all* existing IDs,
    // then require the target callback's worker to be absent from that snapshot.
    // More cached workers may exist: reuse gives "condition not observed", not
    // proof of new thread creation. The peer stays held throughout measurement.
    pthread_mutex_lock(&lock);
    start = host_ticks();
    submit_peer();
    int result = wait_flag(&peer_entered, start, 2000000000);
    struct thread_inventory before = {0};
    int query = result ? KERN_FAILURE : read_thread_inventory(&before);
    printf("{\"event\":\"before_creation\",\"peer_id\":%llu,\"peer_qos\":%u,"
           "\"result\":%d,\"query_result\":%d,\"ids\":[", peer_id, peer_qos, result, query);
    if (!query) for (unsigned i = 0; i < before.count; i++)
      printf("%s%llu", i ? "," : "", before.ids[i]);
    printf("]}\n");
    if (result || query || peer_qos != QOS_CLASS_BACKGROUND || !inventory_contains(&before, peer_id)) {
      peer_released = true;
      pthread_cond_broadcast(&changed);
      pthread_mutex_unlock(&lock);
      outcome = result ? 1 : 3;
      return NULL;
    }
    pthread_mutex_unlock(&lock);
    outcome = submit_callback(queue, false, true);
    pthread_mutex_lock(&lock);
    uint64_t next_worker = worker_id;
    pthread_mutex_unlock(&lock);
    bool created = !outcome && next_worker && !inventory_contains(&before, next_worker);
    printf("{\"event\":\"creation\",\"observed\":%s,\"worker_id\":%llu}\n",
           created ? "true" : "false", next_worker);
    if (!outcome && !created) outcome = 2;
    peer_queue = nil;
  }
  return NULL;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  unsigned long diagnostic_hold_us = 0;
  const char *hold = getenv("FC_QUEUE_DIAGNOSTIC_HOLD_US");
  if (hold) {
    char *end = NULL;
    errno = 0;
    diagnostic_hold_us = strtoul(hold, &end, 10);
    if (errno || !hold[0] || *end || diagnostic_hold_us > 10000000) return 3;
  }
  if (argc < 2 || argc > 3 || (strcmp(argv[1], "real") &&
      strcmp(argv[1], "manual") && strcmp(argv[1], "tracked"))) return 3;
  mode = argv[1];
  if (argc == 3) {
    char *end = NULL;
    errno = 0;
    unsigned long value = strtoul(argv[2], &end, 10);
    if (errno || !argv[2][0] || *end || value < 1 || value > 60000) return 3;
    observation_ns = value * 1000000ull;
  }
  host_ticks = dlsym(RTLD_DEFAULT, "fc_real_abs");
  if (!strcmp(mode, "real")) {
    require(!host_ticks); // Real control must not accidentally load the interposer.
    host_ticks = mach_absolute_time;
  } else {
    int (*manual)(void) = dlsym(RTLD_DEFAULT, "fc_manual_clock_enabled");
    void (*advance)(uint64_t) = dlsym(RTLD_DEFAULT, "fc_advance_ns");
    now_ns = dlsym(RTLD_DEFAULT, "fc_manual_now_ns");
    require(host_ticks && manual && manual() && advance && now_ns && now_ns() == 0);
    advance(4999000000);
    if (!strcmp(mode, "tracked")) {
      wait_idle = dlsym(RTLD_DEFAULT, "fc_dispatch_wait_idle");
      require(wait_idle);
    }
  }
  require(mach_timebase_info(&timebase) == KERN_SUCCESS);
  printf("{\"event\":\"start\",\"pid\":%d,\"mode\":\"%s\",\"observation_ns\":%llu}\n",
         getpid(), mode, observation_ns);
  pthread_t controller;
  require(pthread_create(&controller, NULL, run_controller, NULL) == 0);
  require(pthread_join(controller, NULL) == 0);
  printf("{\"event\":\"end\",\"exit_code\":%d}\n", outcome);
  // Keep a failed process available to an external owned-PID sampler. The result
  // is already final; later callback delivery cannot turn the failure into a pass.
  if (outcome == 1 && diagnostic_hold_us) {
    printf("{\"event\":\"diagnostic_hold\",\"host_us\":%lu}\n", diagnostic_hold_us);
    usleep((useconds_t)diagnostic_hold_us);
  }
  return outcome;
}
