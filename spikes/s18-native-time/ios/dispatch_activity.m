// Observe public libdispatch async/sync submissions, including transitive submissions.
// Observed NSTimer/delayed-dispatch registrations and explicit deadlines admit due work before entry.
// Source activation admits registration work; cancellation retains handler retirement before delivery.
// DATA_ADD merges on observed sources admit coalesced data before native delivery.
// Other source events still count only after entry.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#include "dispatch_activity.h"
#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <errno.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <limits.h>
#include <fcntl.h>
#include <unistd.h>
#include <execinfo.h>
#include <dlfcn.h>
#include <stdbool.h>

extern int fc_manual_clock_enabled(void);
extern uint64_t fc_real_abs(void);
extern void fc_advance_ns(uint64_t delta);
extern uint64_t fc_manual_now_ns(void);

#define INTERPOSE(replacement, original) \
  __attribute__((used)) static struct { const void *r; const void *o; } interpose_##original \
  __attribute__((section("__DATA,__interpose"))) = {(const void *)&replacement, (const void *)&original};

static pthread_mutex_t activity_lock = PTHREAD_MUTEX_INITIALIZER;
// Foundation bookkeeping can call synchronous dispatch. Those internal calls must
// not reenter observation while this thread owns the activity mutex.
static _Thread_local BOOL activity_locked;
static void lock_activity(void) {
  pthread_mutex_lock(&activity_lock);
  activity_locked = YES;
}
static void unlock_activity(void) {
  activity_locked = NO;
  pthread_mutex_unlock(&activity_lock);
}
static pthread_cond_t activity_changed = PTHREAD_COND_INITIALIZER;
static struct fc_dispatch_activity activity;
static int tracking;
static _Thread_local int task_depth;

// Native block waits communicate the controller's dependency to libdispatch.
// Reserve one waiter per submitted block; successful waits may only happen once.
// Other producers retain condition-variable waiting on the same pending predicate.
static uint64_t waiter_count, native_waiter_count, native_wait_calls, native_wait_timeouts;
extern void fc_host_mach_time_enter(void), fc_host_mach_time_leave(void);
@interface FCWaitBlock : NSObject
@property(nonatomic, copy) dispatch_block_t block;
@property(nonatomic) BOOL submitted, waiting;
@property(nonatomic) uint64_t task_id;
@end
@implementation FCWaitBlock
@end
static NSMutableDictionary<NSNumber *, FCWaitBlock *> *wait_blocks;
static void register_wait_block(uint64_t id, FCWaitBlock *entry) {
  if (!id || !entry) return;
  if (!wait_blocks) wait_blocks = [NSMutableDictionary new];
  entry.task_id = id;
  wait_blocks[@(id)] = entry;
}
void fc_dispatch_wait_state_read(struct fc_dispatch_wait_state *out) {
  lock_activity();
  *out = (struct fc_dispatch_wait_state){
    .callers = waiter_count, .native_waiters = native_waiter_count,
    .retained_blocks = wait_blocks.count, .native_calls = native_wait_calls,
    .native_timeouts = native_wait_timeouts
  };
  unlock_activity();
}

// Invocation records stay linked through their observed completion boundary.
// Common calls use stack storage; run-loop calls outlive the body and use heap storage.
// Publication, activation adoption and removal all hold activity_lock.
struct callback_scope {
  uint64_t id, thread_id;
  __unsafe_unretained FCWaitBlock *wait_block;
  struct callback_scope *next;
};
static struct callback_scope *entered_callbacks;

// Synchronous dispatch borrows the caller's objects. This stack record remains
// linked until the native API returns, including its queue-completion handoff.
struct sync_call {
  uint64_t id, thread_id;
  __unsafe_unretained dispatch_queue_t queue;
  void *context;
  dispatch_function_t function;
  struct sync_call *next;
};
static struct sync_call *sync_calls;

// Optional diagnostics never limit admission. Excess pending records are reported as omitted.
// All record access uses activity_lock; queue labels are copied while the queue is alive.
static int trace_fd = -1;
struct task_record {
  uint64_t id, thread_id;
  int running, frames;
  char queue[96];
  void *stack[12];
};
static struct task_record records[128];

static uint64_t admit_task(__unsafe_unretained dispatch_queue_t queue) {
  uint64_t id = ++activity.submitted;
  activity.pending++;
  if (trace_fd >= 0) {
    for (size_t i = 0; i < sizeof(records) / sizeof(records[0]); i++) {
      if (records[i].id) continue;
      struct task_record *r = &records[i];
      r->id = id;
      r->thread_id = 0;
      r->running = 0;
      snprintf(r->queue, sizeof(r->queue), "%s", queue ? dispatch_queue_get_label(queue) : "callback-entry");
      r->frames = backtrace(r->stack, 12);
      break;
    }
  }
  return id;
}

static void mark_task_running_locked(uint64_t id, uint64_t thread_id) {
  if (!id || trace_fd < 0) return;
  for (size_t i = 0; i < sizeof(records) / sizeof(records[0]); i++)
    if (records[i].id == id) {
      records[i].running = 1;
      records[i].thread_id = thread_id;
      break;
    }
}
static void enter_task(uint64_t id) {
  if (trace_fd < 0) return;
  uint64_t thread_id = 0;
  pthread_threadid_np(NULL, &thread_id);
  lock_activity();
  mark_task_running_locked(id, thread_id);
  unlock_activity();
}

#include "thread_frames.h"

// Capture at the failed predicate, before symbolization or returning to the caller.
// This is a sequential observation under activity_lock, not an atomic scheduler snapshot.
static void trace_threads(void) {
  uint64_t started = fc_real_abs(), controller = 0;
  pthread_threadid_np(NULL, &controller);
  thread_act_array_t threads = NULL;
  mach_msg_type_number_t count = 0;
  kern_return_t result = task_threads(mach_task_self(), &threads, &count);
  dprintf(trace_fd, "scheduler pid=%d controller=%llu host_ticks=%llu result=%d threads=%u\n",
    getpid(), controller, started, result, count);
  if (result != KERN_SUCCESS) return;
  unsigned captured = 0;
  mach_timebase_info_data_t timebase;
  mach_timebase_info(&timebase);
  for (mach_msg_type_number_t i = 0; i < count; i++) {
    uint64_t elapsed = (uint64_t)((__uint128_t)(fc_real_abs() - started) * timebase.numer / timebase.denom);
    if (captured < 64 && elapsed < 20000000) {
      thread_identifier_info_data_t identity = {0};
      thread_basic_info_data_t basic = {0};
      thread_extended_info_data_t extended = {0};
      mach_msg_type_number_t size = THREAD_IDENTIFIER_INFO_COUNT;
      kern_return_t id_result = thread_info(threads[i], THREAD_IDENTIFIER_INFO, (thread_info_t)&identity, &size);
      size = THREAD_BASIC_INFO_COUNT;
      kern_return_t basic_result = thread_info(threads[i], THREAD_BASIC_INFO, (thread_info_t)&basic, &size);
      size = THREAD_EXTENDED_INFO_COUNT;
      kern_return_t extended_result = thread_info(threads[i], THREAD_EXTENDED_INFO, (thread_info_t)&extended, &size);
      dprintf(trace_fd, "thread id=%llu id_result=%d basic_result=%d extended_result=%d state=%d suspend=%d cpu=%d priority=%d base_priority=%d\n",
        identity.thread_id, id_result, basic_result, extended_result, basic.run_state,
        basic.suspend_count, basic.cpu_usage, extended.pth_curpri, extended.pth_priority);
      if (id_result == KERN_SUCCESS)
        trace_thread_frames(trace_fd, threads[i], identity.thread_id, started, timebase);
      captured++;
    }
    mach_port_deallocate(mach_task_self(), threads[i]);
  }
  vm_deallocate(mach_task_self(), (vm_address_t)threads, count * sizeof(thread_t));
  trace_frame_images(trace_fd, started, timebase);
  dprintf(trace_fd, "scheduler_end captured=%u omitted=%u host_ticks=%llu numer=%u denom=%u\n",
    captured, count - captured, fc_real_abs(), timebase.numer, timebase.denom);
}

// Per-controller bounded history; no heap allocation or trace I/O during a wait.
struct wait_span {
  uint64_t sequence, task_id, started, finished, pending_before, pending_after;
  int native, result;
};
#define WAIT_SPANS 16
static void trace_pending(const struct wait_span *spans, uint64_t total, uint64_t controller) {
  if (trace_fd < 0) return;
  size_t count = 0;
  for (size_t i = 0; i < sizeof(records) / sizeof(records[0]); i++) count += records[i].id != 0;
  dprintf(trace_fd, "timeout now_ns=%llu pending=%llu recorded=%zu omitted=%llu\n",
    fc_manual_now_ns(), activity.pending, count, activity.pending - count);
  dprintf(trace_fd, "waiters callers=%llu native=%llu blocks=%lu calls=%llu timeouts=%llu\n",
    waiter_count, native_waiter_count, (unsigned long)wait_blocks.count, native_wait_calls, native_wait_timeouts);
  uint64_t retained = total < WAIT_SPANS ? total : WAIT_SPANS;
  dprintf(trace_fd, "wait_history controller=%llu total=%llu retained=%llu omitted=%llu clock=mach_absolute_ticks\n",
    controller, total, retained, total - retained);
  for (uint64_t i = total - retained; i < total; i++) {
    const struct wait_span *span = &spans[i % WAIT_SPANS];
    dprintf(trace_fd, "wait_span sequence=%llu kind=%s task=%llu started=%llu finished=%llu pending_before=%llu pending_after=%llu result=%d\n",
      span->sequence, span->native ? "native" : "condition", span->task_id,
      span->started, span->finished, span->pending_before, span->pending_after, span->result);
  }
  trace_threads();
  for (size_t i = 0; i < sizeof(records) / sizeof(records[0]); i++) {
    struct task_record *r = &records[i];
    if (!r->id) continue;
    dprintf(trace_fd, "task id=%llu state=%s thread=%llu queue=%s\n", r->id,
      r->running ? "running" : "queued", r->thread_id, r->queue);
    for (int j = 0; j < r->frames; j++) {
      // Symbol lookup can block while activity_lock excludes producer completion.
      // Keep the submission addresses; interpret them offline with the image inventory above.
      dprintf(trace_fd, "task_frame id=%llu depth=%d address=%p\n", r->id, j, r->stack[j]);
    }
  }
}

@interface FCDeadline : NSObject
@property(nonatomic) uint64_t deadline, token, task_id;
@property(nonatomic) BOOL running, cancelled;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, copy) dispatch_block_t block;
@end
@implementation FCDeadline
@end
static NSMutableArray<FCDeadline *> *deadlines;
static NSMutableDictionary<NSNumber *, FCDeadline *> *scheduled;
static uint64_t next_token;
static void finish_task_locked(uint64_t id);

// Queued submissions remain discoverable until entry transfers their obligation to a scope.
@interface FCAsyncInvocation : NSObject
@property(nonatomic) uint64_t task_id;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, strong) FCWaitBlock *wait_block;
@end
@implementation FCAsyncInvocation
@end
static NSMutableSet<FCAsyncInvocation *> *async_invocations;

// Source obligations outlive weak handler ownership without retaining the source or
// application captures. Registration and cancellation share one record through release.
@interface FCSourceObligation : NSObject {
@public
  uint64_t task_id, thread_id;
  BOOL cancelled;
}
@end
@implementation FCSourceObligation
@end
static NSMutableSet<FCSourceObligation *> *source_obligations;
static FCSourceObligation *begin_source_locked(void) {
  FCSourceObligation *item = [FCSourceObligation new];
  item->task_id = tracking ? admit_task(NULL) : 0;
  if (!source_obligations) source_obligations = [NSMutableSet new];
  [source_obligations addObject:item];
  return item;
}
static void finish_source_locked(FCSourceObligation *item) {
  if (item->task_id) {
    if (item->cancelled) {
      if (!activity.source_cancellations) abort();
      activity.source_cancellations--;
    }
    finish_task_locked(item->task_id);
  }
  [source_obligations removeObject:item];
}

@interface FCSourceActivation : NSObject {
@public
  NSUInteger suspensions;
  BOOL active, cancelled;
}
@end
@implementation FCSourceActivation
@end
static char source_activation_key;


// Apple's private decoder preserves the native clock domain without interpreting encoded bits.
// This spike targets the recorded simulator runtime; an unavailable decoder must not imply idle.
static bool (*decode_dispatch_time)(dispatch_time_t, uintptr_t *, uint64_t *);
@interface FCDelayedInvocation : NSObject
@property(nonatomic) uintptr_t clock;
@property(nonatomic) uint64_t deadline, task_id;
@property(nonatomic, strong) dispatch_queue_t queue;
@end
@implementation FCDelayedInvocation
@end
static NSMutableArray<FCDelayedInvocation *> *delayed_invocations;

static uint64_t dispatch_clock_now(uintptr_t clock) {
  dispatch_time_t now;
  if (clock == 1) now = DISPATCH_TIME_NOW;
  else if (clock == 2) now = (1ull << 63); // DISPATCH_MONOTONICTIME_NOW SPI.
  else if (clock == 3) now = dispatch_walltime(NULL, 0);
  else abort();
  uintptr_t decoded_clock;
  uint64_t ns;
  if (!decode_dispatch_time(now, &decoded_clock, &ns) || decoded_clock != clock) abort();
  return ns;
}

// Caller owns activity_lock. Native libdispatch still owns scheduling and invocation.
static void admit_due_dispatch_after(void) {
  if (!tracking) return;
  for (FCDelayedInvocation *item in delayed_invocations) {
    if (!item.task_id && item.deadline <= dispatch_clock_now(item.clock))
      item.task_id = admit_task(item.queue);
  }
}

// Caller owns activity_lock; all due work is counted before releasing the boundary.
static void admit_due_deadlines(void) {
  uint64_t now = fc_manual_now_ns();
  for (NSUInteger i = 0; i < deadlines.count;) {
    FCDeadline *item = deadlines[i];
    if (item.deadline > now) { i++; continue; }
    item.task_id = admit_task(item.queue);
    dispatch_async(item.queue, ^{
      lock_activity();
      if (item.cancelled) { unlock_activity(); return; }
      item.running = YES;
      unlock_activity();
      enter_task(item.task_id);
      task_depth++;
      @try {
        @autoreleasepool {
          dispatch_block_t callback = item.block;
          callback();
        }
      }
      @finally {
        // Explicitly release the owned callback before publishing completion.
        // Its captures may re-enter observation or submit more work.
        @try { item.block = nil; }
        @finally {
          task_depth--;
          lock_activity();
          finish_task_locked(item.task_id);
          [scheduled removeObjectForKey:@(item.token)];
          unlock_activity();
        }
      }
    });
    [deadlines removeObjectAtIndex:i];
  }
}

static int schedule_deadline(uint64_t delay_ns, dispatch_queue_t queue, dispatch_block_t block, uint64_t *token) {
  if (!queue || !block) return EINVAL;
  lock_activity();
  int result = tracking ? 0 : ENOTSUP;
  uint64_t now = fc_manual_now_ns();
  if (!result && (UINT64_MAX - now < delay_ns || next_token == UINT64_MAX)) result = EOVERFLOW;
  if (!result) {
    if (!deadlines) deadlines = [NSMutableArray new];
    if (!scheduled) scheduled = [NSMutableDictionary new];
    FCDeadline *item = [FCDeadline new];
    item.deadline = now + delay_ns;
    item.token = ++next_token;
    scheduled[@(item.token)] = item;
    if (token) *token = item.token;
    item.queue = queue;
    item.block = block;
    [deadlines addObject:item];
    admit_due_deadlines();
  }
  unlock_activity();
  return result;
}

int fc_dispatch_after_ns(uint64_t delay_ns, dispatch_queue_t queue, dispatch_block_t block) {
  return schedule_deadline(delay_ns, queue, block, NULL);
}

int fc_dispatch_schedule_ns(uint64_t delay_ns, dispatch_queue_t queue, dispatch_block_t block,
                            uint64_t *token) {
  if (!token) return EINVAL;
  return schedule_deadline(delay_ns, queue, block, token);
}

int fc_dispatch_cancel(uint64_t token) {
  lock_activity();
  FCDeadline *item = scheduled[@(token)];
  int result = !tracking ? ENOTSUP : !item ? ENOENT : item.running ? EBUSY : 0;
  uint64_t release_id = 0;
  if (!result) {
    // Keep capture destruction and its descendants ahead of the idle boundary.
    release_id = admit_task(NULL);
    item.cancelled = YES;
    if (item.task_id) finish_task_locked(item.task_id);
    else [deadlines removeObjectIdenticalTo:item];
    [scheduled removeObjectForKey:@(token)];
  }
  unlock_activity();
  // Captured objects may run application code when released; never release under our lock.
  if (!result) {
    enter_task(release_id);
    task_depth++;
    @try { item.block = nil; }
    @finally {
      task_depth--;
      lock_activity();
      finish_task_locked(release_id);
      unlock_activity();
    }
  }
  return result;
}

static NSArray *admit_source_data_locked(void) __attribute__((ns_returns_retained));
static void admit_due_nstimers(void);

int fc_dispatch_tracking_start(void) {
  if (!fc_manual_clock_enabled()) return ENOTSUP;
  @autoreleasepool {
    // A weak-registry snapshot may own the last reference; release it outside the lock.
    __attribute__((objc_precise_lifetime)) NSArray *sources;
    lock_activity();
    int result = tracking ? EALREADY : 0;
    if (!tracking) {
      const char *trace = getenv("FC_TRACE_ACTIVITY");
      if (trace && *trace) {
        char path[PATH_MAX];
        if (!strcmp(trace, "1")) snprintf(path, sizeof(path), "%s/Documents/dispatch-pending.txt", getenv("HOME") ?: ".");
        else snprintf(path, sizeof(path), "%s", trace);
        trace_fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
        if (trace_fd < 0) { result = errno; unlock_activity(); return result; }
      }
      tracking = 1;
      for (FCAsyncInvocation *item in async_invocations) {
        item.task_id = admit_task(item.queue);
        register_wait_block(item.task_id, item.wait_block);
      }
      for (struct sync_call *call = sync_calls; call; call = call->next) {
        call->id = admit_task(call->queue);
        if (call->thread_id) mark_task_running_locked(call->id, call->thread_id);
      }
      for (FCSourceObligation *item in source_obligations) {
        item->task_id = admit_task(NULL);
        if (item->cancelled) activity.source_cancellations++;
        if (item->thread_id) mark_task_running_locked(item->task_id, item->thread_id);
      }
      sources = admit_source_data_locked();
      for (struct callback_scope *scope = entered_callbacks; scope; scope = scope->next) {
        if (!scope->id) scope->id = admit_task(NULL);
        register_wait_block(scope->id, scope->wait_block);
        mark_task_running_locked(scope->id, scope->thread_id);
      }
      admit_due_nstimers();
      admit_due_dispatch_after();
    }
    unlock_activity();
    return result;
  }
}

void fc_dispatch_activity_read(struct fc_dispatch_activity *out) {
  lock_activity();
  *out = activity;
  unlock_activity();
}

static FCAsyncInvocation * __attribute__((ns_returns_retained)) begin_async(dispatch_queue_t queue) {
  FCAsyncInvocation *item = [FCAsyncInvocation new];
  item.queue = queue;
  lock_activity();
  item.task_id = tracking ? admit_task(queue) : 0;
  if (!async_invocations) async_invocations = [NSMutableSet new];
  [async_invocations addObject:item];
  unlock_activity();
  return item;
}

static void enter_async(FCAsyncInvocation *item, struct callback_scope *scope) {
  pthread_threadid_np(NULL, &scope->thread_id);
  lock_activity();
  scope->id = item.task_id;
  scope->wait_block = item.wait_block;
  scope->next = entered_callbacks;
  entered_callbacks = scope;
  mark_task_running_locked(scope->id, scope->thread_id);
  [async_invocations removeObject:item];
  unlock_activity();
}

static void finish_task_locked(uint64_t id) {
  if (!activity.pending) abort();
  if (trace_fd >= 0) {
    for (size_t i = 0; i < sizeof(records) / sizeof(records[0]); i++)
      if (records[i].id == id) { records[i].id = 0; break; }
  }
  // For async blocks the application captures were already released outside
  // activity_lock; the executing native block still owns its invocation record.
  [wait_blocks removeObjectForKey:@(id)];
  activity.completed++;
  activity.pending--;
  if (!activity.pending) pthread_cond_broadcast(&activity_changed);
}

static void finish_callback_locked(struct callback_scope *scope) {
  struct callback_scope **link = &entered_callbacks;
  while (*link && *link != scope) link = &(*link)->next;
  if (!*link) abort();
  *link = scope->next;
  if (scope->id) finish_task_locked(scope->id); // May have been assigned by activation.
}

#include "nstimer_registry.h"

static int wait_or_advance(uint64_t timeout_ns, uint64_t delta_ns, int advance) {
  if (task_depth || pthread_main_np()) return EDEADLK;
  mach_timebase_info_data_t timebase;
  mach_timebase_info(&timebase);
  uint64_t start = fc_real_abs(), span_count = 0, controller = 0;
  struct wait_span spans[WAIT_SPANS] = {0};
  pthread_threadid_np(NULL, &controller);
  lock_activity();
  int result = tracking ? 0 : ENOTSUP;
  waiter_count++;
  while (!result && activity.pending) {
    uint64_t elapsed = (uint64_t)((__uint128_t)(fc_real_abs() - start) * timebase.numer / timebase.denom);
    if (elapsed >= timeout_ns) { result = ETIMEDOUT; break; }
    uint64_t remaining = timeout_ns - elapsed;
    __attribute__((objc_precise_lifetime)) FCWaitBlock *selected = nil;
    for (FCWaitBlock *entry in wait_blocks.objectEnumerator) {
      if (entry.submitted && !entry.waiting) { selected = entry; break; }
    }
    struct wait_span span = {0};
    if (trace_fd >= 0) span = (struct wait_span){
      .sequence = span_count + 1, .task_id = selected ? selected.task_id : 0,
      .native = selected != nil, .started = fc_real_abs(), .pending_before = activity.pending
    };
    if (selected) {
      selected.waiting = YES;
      native_waiter_count++; native_wait_calls++;
      unlock_activity();
      fc_host_mach_time_enter();
      @try {
        int64_t interval = remaining > INT64_MAX ? INT64_MAX : (int64_t)remaining;
        result = dispatch_block_wait(selected.block, dispatch_time(DISPATCH_TIME_NOW, interval)) ? ETIMEDOUT : 0;
      } @finally { fc_host_mach_time_leave(); }
      lock_activity();
      if (result) native_wait_timeouts++;
      selected.waiting = NO;
      native_waiter_count--;
      pthread_cond_broadcast(&activity_changed);
      unlock_activity();
      selected = nil; // Captured-object releases must stay outside the observer mutex.
      lock_activity();
    } else {
      struct timespec interval = {.tv_sec = (time_t)(remaining / 1000000000),
                                 .tv_nsec = (long)(remaining % 1000000000)};
      result = pthread_cond_timedwait_relative_np(&activity_changed, &activity_lock, &interval);
    }
    if (trace_fd >= 0) {
      span.finished = fc_real_abs(); span.pending_after = activity.pending; span.result = result;
      spans[span_count++ % WAIT_SPANS] = span;
    }
    // The predicate wins over a timeout racing the last completion.
    if (result == ETIMEDOUT && !activity.pending) result = 0;
  }
  if (result == ETIMEDOUT) trace_pending(spans, span_count, controller);
  // Lock order: activity -> timer registry. Rearming never waits for callbacks.
  // Submission cannot admit new work between the idle predicate and clock mutation.
  if (!result && advance) {
    if (UINT64_MAX - fc_manual_now_ns() < delta_ns) result = EOVERFLOW;
    else {
      fc_advance_ns(delta_ns);
      admit_due_deadlines();
      admit_due_dispatch_after();
      admit_due_nstimers();
    }
  }
  waiter_count--;
  unlock_activity();
  return result;
}

int fc_dispatch_wait_idle(uint64_t timeout_ns) {
  return wait_or_advance(timeout_ns, 0, 0);
}

int fc_dispatch_advance_when_idle(uint64_t delta_ns, uint64_t timeout_ns) {
  return wait_or_advance(timeout_ns, delta_ns, 1);
}

static dispatch_block_t __attribute__((ns_returns_retained))
observed_block(dispatch_block_t block, FCAsyncInvocation *item) {
  __block dispatch_block_t owned = block;
  dispatch_block_t observed = ^{
    struct callback_scope scope = {0};
    enter_async(item, &scope);
    task_depth++;
    @try { @autoreleasepool { owned(); } }
    @finally {
      @try { @autoreleasepool { owned = nil; } }
      @finally {
        task_depth--;
        __attribute__((objc_precise_lifetime)) FCWaitBlock *retired;
        lock_activity();
        retired = item.wait_block;
        item.wait_block = nil; // Break native block -> invocation -> handle ownership.
        finish_callback_locked(&scope);
        unlock_activity();
        retired = nil; // Release native wrappers outside the activity mutex.
      }
    }
  };
  // Prepare native dependency waiting before submission, even while tracking is
  // inactive. Activation adopts queued or entered handles with their obligations.
  FCWaitBlock *entry = [FCWaitBlock new];
  entry.block = dispatch_block_create(0, observed);
  lock_activity();
  item.wait_block = entry;
  register_wait_block(item.task_id, entry);
  unlock_activity();
  return entry.block;
}
static void mark_async_submitted(FCAsyncInvocation *item) {
  lock_activity();
  item.wait_block.submitted = YES;
  pthread_cond_broadcast(&activity_changed);
  unlock_activity();
}

struct function_task { void *context; dispatch_function_t function; void *invocation; };
static struct function_task *observed_function(void *context, dispatch_function_t function, FCAsyncInvocation *item) {
  struct function_task *task = malloc(sizeof(*task));
  if (!task) abort();
  *task = (struct function_task){context, function, (__bridge_retained void *)item};
  return task;
}
static void run_function(void *context) {
  struct function_task *task = context;
  FCAsyncInvocation *item = (__bridge_transfer FCAsyncInvocation *)task->invocation;
  struct callback_scope scope = {0};
  enter_async(item, &scope);
  task_depth++;
  @try { @autoreleasepool { task->function(task->context); } }
  @finally {
    task_depth--;
    free(task);
    lock_activity();
    finish_callback_locked(&scope);
    unlock_activity();
  }
}

static void observed_async(dispatch_queue_t queue, dispatch_block_t block) {
  if (fc_manual_clock_enabled()) {
    FCAsyncInvocation *item = begin_async(queue);
    dispatch_async(queue, observed_block(block, item));
    mark_async_submitted(item);
  }
  else dispatch_async(queue, block);
}
static void observed_async_f(dispatch_queue_t queue, void *context, dispatch_function_t function) {
  if (fc_manual_clock_enabled()) dispatch_async_f(queue, observed_function(context, function, begin_async(queue)), run_function);
  else dispatch_async_f(queue, context, function);
}
static void observed_barrier_async(dispatch_queue_t queue, dispatch_block_t block) {
  if (fc_manual_clock_enabled()) {
    FCAsyncInvocation *item = begin_async(queue);
    dispatch_barrier_async(queue, observed_block(block, item));
    mark_async_submitted(item);
  }
  else dispatch_barrier_async(queue, block);
}
static void observed_barrier_async_f(dispatch_queue_t queue, void *context, dispatch_function_t function) {
  if (fc_manual_clock_enabled()) dispatch_barrier_async_f(queue, observed_function(context, function, begin_async(queue)), run_function);
  else dispatch_barrier_async_f(queue, context, function);
}
static void observed_group_async(dispatch_group_t group, dispatch_queue_t queue, dispatch_block_t block) {
  if (fc_manual_clock_enabled()) {
    FCAsyncInvocation *item = begin_async(queue);
    dispatch_group_async(group, queue, observed_block(block, item));
    mark_async_submitted(item);
  }
  else dispatch_group_async(group, queue, block);
}
static void observed_group_async_f(dispatch_group_t group, dispatch_queue_t queue, void *context, dispatch_function_t function) {
  if (fc_manual_clock_enabled()) dispatch_group_async_f(group, queue, observed_function(context, function, begin_async(queue)), run_function);
  else dispatch_group_async_f(group, queue, context, function);
}

static void begin_sync(struct sync_call *call, __unsafe_unretained dispatch_queue_t queue,
                       void *context, dispatch_function_t function) {
  *call = (struct sync_call){.queue = queue, .context = context, .function = function};
  lock_activity();
  call->id = tracking ? admit_task(queue) : 0;
  call->next = sync_calls;
  sync_calls = call;
  unlock_activity();
}
static void run_sync(void *raw) {
  struct sync_call *call = raw;
  lock_activity();
  pthread_threadid_np(NULL, &call->thread_id);
  mark_task_running_locked(call->id, call->thread_id);
  unlock_activity();
  task_depth++;
  // No added autorelease pool: synchronous work preserves the caller's pool lifetime.
  @try { call->function(call->context); }
  @finally {
    task_depth--;
    lock_activity();
    call->thread_id = 0;
    unlock_activity();
  }
}
static void finish_sync(struct sync_call *call) {
  lock_activity();
  struct sync_call **link = &sync_calls;
  while (*link && *link != call) link = &(*link)->next;
  if (!*link) abort();
  *link = call->next;
  if (call->id) finish_task_locked(call->id);
  unlock_activity();
}
static void invoke_borrowed_block(void *block) { ((__bridge dispatch_block_t)block)(); }
static void observed_sync(__unsafe_unretained dispatch_queue_t queue,
                          DISPATCH_NOESCAPE __unsafe_unretained dispatch_block_t block) {
  if (!fc_manual_clock_enabled() || activity_locked) { dispatch_sync(queue, block); return; }
  struct sync_call call, *record = &call;
  begin_sync(record, queue, (__bridge void *)block, invoke_borrowed_block);
  @try { dispatch_sync(queue, ^{ run_sync(record); }); }
  @finally { finish_sync(record); }
}
static void observed_sync_f(__unsafe_unretained dispatch_queue_t queue, void *context, dispatch_function_t function) {
  if (!fc_manual_clock_enabled() || activity_locked) { dispatch_sync_f(queue, context, function); return; }
  struct sync_call call;
  begin_sync(&call, queue, context, function);
  @try { dispatch_sync_f(queue, &call, run_sync); }
  @finally { finish_sync(&call); }
}
static void observed_barrier_sync(__unsafe_unretained dispatch_queue_t queue,
                                  DISPATCH_NOESCAPE __unsafe_unretained dispatch_block_t block) {
  if (!fc_manual_clock_enabled() || activity_locked) { dispatch_barrier_sync(queue, block); return; }
  struct sync_call call, *record = &call;
  begin_sync(record, queue, (__bridge void *)block, invoke_borrowed_block);
  @try { dispatch_barrier_sync(queue, ^{ run_sync(record); }); }
  @finally { finish_sync(record); }
}
static void observed_barrier_sync_f(__unsafe_unretained dispatch_queue_t queue, void *context, dispatch_function_t function) {
  if (!fc_manual_clock_enabled() || activity_locked) { dispatch_barrier_sync_f(queue, context, function); return; }
  struct sync_call call;
  begin_sync(&call, queue, context, function);
  @try { dispatch_barrier_sync_f(queue, &call, run_sync); }
  @finally { finish_sync(&call); }
}

INTERPOSE(observed_sync, dispatch_sync)
INTERPOSE(observed_sync_f, dispatch_sync_f)
INTERPOSE(observed_barrier_sync, dispatch_barrier_sync)
INTERPOSE(observed_barrier_sync_f, dispatch_barrier_sync_f)
INTERPOSE(observed_async, dispatch_async)
INTERPOSE(observed_async_f, dispatch_async_f)
INTERPOSE(observed_barrier_async, dispatch_barrier_async)
INTERPOSE(observed_barrier_async_f, dispatch_barrier_async_f)
INTERPOSE(observed_group_async, dispatch_group_async)
INTERPOSE(observed_group_async_f, dispatch_group_async_f)

// Keep entered callbacks visible before tracking as well as through activation.
// Registrations alone are not ready work; descendants own their own obligations.
void fc_observe_callback(void (^block)(void)) {
  struct callback_scope scope = {0};
  pthread_threadid_np(NULL, &scope.thread_id);
  lock_activity();
  scope.id = tracking ? admit_task(NULL) : 0;
  scope.next = entered_callbacks;
  entered_callbacks = &scope;
  mark_task_running_locked(scope.id, scope.thread_id);
  unlock_activity();
  task_depth++;
  @try { @autoreleasepool { block(); } }
  @finally {
    task_depth--;
    lock_activity();
    finish_callback_locked(&scope);
    unlock_activity();
  }
}

// NSTimer updates its native repeat deadline (or invalidates a one-shot) after
// the user callback returns. Keep the invocation pending through the next
// post-timer run-loop phase, including a loop stopped by that callback.
void fc_observe_runloop_callback(NSTimer *timer, void (^block)(void)) {
  struct callback_scope *scope = calloc(1, sizeof(*scope));
  if (!scope) abort();
  pthread_threadid_np(NULL, &scope->thread_id);
  lock_activity();
  FCTimerActivity *state = timer_state_locked(timer);
  revise_timer_locked(state);
  scope->id = state.task_id ?: (tracking ? admit_task(NULL) : 0);
  scope->next = entered_callbacks;
  entered_callbacks = scope;
  mark_task_running_locked(scope->id, scope->thread_id);
  state.task_id = 0;
  state.callbacks++;
  unlock_activity();
  CFRunLoopRef loop = CFRunLoopGetCurrent();
  CFStringRef mode = CFRunLoopCopyCurrentMode(loop);
  BOOL returned = NO;
  task_depth++;
  @try { @autoreleasepool { block(); } returned = YES; }
  @finally {
    task_depth--;
    if (mode && returned) {
      __block NSTimer *held_timer = timer;
      CFRunLoopObserverRef observer = CFRunLoopObserverCreateWithHandler(NULL,
        kCFRunLoopBeforeSources | kCFRunLoopBeforeWaiting | kCFRunLoopExit, false, 0,
        ^(CFRunLoopObserverRef observer, CFRunLoopActivity phase) {
          task_depth++;
          @try {
            refresh_returned_timer(state, held_timer);
            held_timer = nil; // Native/captured releases stay outside activity_lock.
          } @finally { task_depth--; finish_timer_callback(state, scope); }
        });
      if (!observer) abort();
      CFRunLoopAddObserver(loop, observer, mode);
      CFRelease(observer);
    } else {
      task_depth++;
      @try { refresh_returned_timer(state, timer); }
      @finally { task_depth--; finish_timer_callback(state, scope); }
    }
    if (mode) CFRelease(mode);
  }
}

// The source owns this state, and event handlers share it without retaining the source.
// Values, unlike merge counts, can be matched against libdispatch's latched callback data.
@interface FCSourceData : NSObject {
@public
  uintptr_t pending_value;
  uint64_t task_id, running_id, running_thread;
  NSUInteger owners;
  BOOL cancelled, running;
}
@end
static NSHashTable<FCSourceData *> *source_data_states;
@implementation FCSourceData
- (void)dealloc {
  lock_activity();
  [source_data_states removeObject:self];
  if (task_id) finish_task_locked(task_id);
  unlock_activity();
}
@end
static NSArray *admit_source_data_locked(void) {
  NSArray<FCSourceData *> *snapshot = [source_data_states allObjects];
  for (FCSourceData *data in snapshot) {
    if (data->pending_value && !data->task_id) data->task_id = admit_task(NULL);
    if (data->running && !data->running_id) {
      data->running_id = admit_task(NULL);
      mark_task_running_locked(data->running_id, data->running_thread);
    }
  }
  return snapshot;
}
static char source_data_key;
static void observed_event_handler(dispatch_source_t source, dispatch_block_t block);

static dispatch_source_t __attribute__((ns_returns_retained))
observed_source_create(dispatch_source_type_t type, uintptr_t handle, uintptr_t mask, dispatch_queue_t queue) {
  dispatch_source_t source = dispatch_source_create(type, handle, mask, queue);
  if (!source || !fc_manual_clock_enabled()) return source;
  objc_setAssociatedObject(source, &source_activation_key, [FCSourceActivation new], OBJC_ASSOCIATION_RETAIN_NONATOMIC);
  if (type == DISPATCH_SOURCE_TYPE_DATA_ADD) {
    FCSourceData *data = [FCSourceData new];
    objc_setAssociatedObject(source, &source_data_key, data, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    lock_activity();
    if (!source_data_states) source_data_states = [NSHashTable weakObjectsHashTable];
    [source_data_states addObject:data];
    unlock_activity();
    // Record even before activation; tracking start admits only still-pending data.
    // Native delivery consumes data even without an application handler.
    observed_event_handler(source, nil);
  }
  return source;
}

static void observed_source_merge(dispatch_source_t source, uintptr_t value) {
  if (!fc_manual_clock_enabled()) { dispatch_source_merge_data(source, value); return; }
  FCSourceData *data = objc_getAssociatedObject(source, &source_data_key);
  if (!data) { dispatch_source_merge_data(source, value); return; }
  lock_activity();
  if (value && !data->cancelled) {
    // Wrapped arithmetic would make a native coalesced value ambiguous. Fail closed.
    if (value > UINTPTR_MAX - data->pending_value) {
      dprintf(STDERR_FILENO, "source-data overflow pending=%llu merge=%llu\n",
        (unsigned long long)data->pending_value, (unsigned long long)value);
      abort();
    }
    data->pending_value += value;
    if (tracking && !data->task_id) data->task_id = admit_task(NULL);
  }
  // This API queues delivery; it does not invoke application handlers synchronously.
  // Keep admission and native submission ordered against callback entry and cancel.
  dispatch_source_merge_data(source, value);
  unlock_activity();
}

// libdispatch owns installed handlers until replacement or source teardown.
// Keep that ownership separate from each invocation's observed activity.
enum source_handler_kind { source_event, source_cancel, source_registration };
@interface FCSourceHandler : NSObject {
  dispatch_block_t _block;
  NSNumber *_sourceKey;
  FCSourceData *_data;
  BOOL _oneShot;
@public
  FCSourceObligation *obligation; // Protected by activity_lock; retained through owned release.
  BOOL owns_callback; // Protected by activity_lock; cleared after one-shot owned release.
}
- (instancetype)initWithBlock:(dispatch_block_t)block source:(dispatch_source_t)source kind:(enum source_handler_kind)kind;
- (void)invoke;
- (void)admitRegistrationLocked;
@end
static NSMutableDictionary<NSNumber *, NSHashTable<FCSourceHandler *> *> *source_handlers;
@implementation FCSourceHandler
- (instancetype)initWithBlock:(dispatch_block_t)block source:(dispatch_source_t)source kind:(enum source_handler_kind)kind {
  if ((self = [super init])) {
    _block = [block copy];
    owns_callback = block != nil;
    _oneShot = kind == source_registration;
    _sourceKey = @((uintptr_t)(__bridge void *)source);
    if (kind == source_event) _data = objc_getAssociatedObject(source, &source_data_key);
    lock_activity();
    if (_data) _data->owners++;
    if (!source_handlers) source_handlers = [NSMutableDictionary new];
    NSHashTable *handlers = source_handlers[_sourceKey];
    if (!handlers) source_handlers[_sourceKey] = handlers = [NSHashTable weakObjectsHashTable];
    [handlers addObject:self];
    FCSourceActivation *activation = objc_getAssociatedObject(source, &source_activation_key);
    if (activation && activation->active && !activation->cancelled) [self admitRegistrationLocked];
    unlock_activity();
  }
  return self;
}
- (void)admitRegistrationLocked {
  if (_oneShot && owns_callback && !obligation) obligation = begin_source_locked();
}
- (void)invoke {
  lock_activity();
  FCSourceObligation *retirement = obligation;
  uint64_t id = retirement ? retirement->task_id : 0;
  if (_data) {
    // A source cannot reenter its event handler. Keep this invocation visible even
    // before tracking starts, through both its body and autorelease-pool cleanup.
    if (_data->running) abort();
    uintptr_t value = dispatch_source_get_data((__bridge dispatch_source_t)(void *)_sourceKey.unsignedLongLongValue);
    if (!value || value > _data->pending_value) {
      dprintf(STDERR_FILENO, "source-data mismatch pending=%llu delivered=%llu\n",
        (unsigned long long)_data->pending_value, (unsigned long long)value);
      abort();
    }
    _data->pending_value -= value;
    if (!_data->pending_value) {
      id = _data->task_id;
      _data->task_id = 0;
    } else {
      id = tracking ? admit_task(NULL) : 0; // Later data retains its queued obligation.
    }
    _data->running = YES;
    _data->running_id = id;
    pthread_threadid_np(NULL, &_data->running_thread);
    mark_task_running_locked(id, _data->running_thread);
    unlock_activity();
    task_depth++;
    @try { @autoreleasepool { if (_block) _block(); } }
    @finally {
      task_depth--;
      lock_activity();
      // Activation may have assigned an id while the application body was running.
      if (_data->running_id) finish_task_locked(_data->running_id);
      _data->running_id = 0;
      _data->running_thread = 0;
      _data->running = NO;
      unlock_activity();
    }
    return;
  }
  if (retirement) {
    pthread_threadid_np(NULL, &retirement->thread_id);
    mark_task_running_locked(retirement->task_id, retirement->thread_id);
  }
  unlock_activity();
  dispatch_block_t body;
  if (_oneShot) {
    // Native registration delivery is one-shot. Release our callback inside the
    // invocation scope, before native wrapper destruction could leave an idle gap.
    __block dispatch_block_t owned = _block;
    _block = nil;
    body = ^{
      @try { @autoreleasepool { owned(); } }
      @finally { @autoreleasepool { owned = nil; } }
    };
  } else body = _block;
  if (!retirement) {
    @try { if (body) fc_observe_callback(body); }
    @finally {
      if (_oneShot) {
        lock_activity();
        owns_callback = NO;
        unlock_activity();
      }
    }
    return;
  }
  // Queued registration and cancellation share metadata across entry and activation.
  // Do not adopt a second common scope for the same obligation.
  task_depth++;
  @try { @autoreleasepool { if (body) body(); } }
  @finally {
    task_depth--;
    lock_activity();
    retirement->thread_id = 0;
    if (_oneShot) {
      owns_callback = NO;
      finish_source_locked(retirement);
      obligation = nil;
    }
    unlock_activity();
  }
}
- (void)dealloc {
  // Transfer ownership before clearing the ivar; application destruction must
  // happen inside observation, never under activity_lock or after finish_task.
  __block dispatch_block_t owned = _block;
  _block = nil;
  lock_activity();
  FCSourceObligation *retirement = obligation;
  if (retirement) {
    pthread_threadid_np(NULL, &retirement->thread_id);
    mark_task_running_locked(retirement->task_id, retirement->thread_id);
  }
  NSHashTable *handlers = source_handlers[_sourceKey];
  [handlers removeObject:self];
  if (!handlers.count) [source_handlers removeObjectForKey:_sourceKey];
  unlock_activity();
  if (retirement) {
    task_depth++;
    @try { @autoreleasepool { owned = nil; } }
    @finally {
      task_depth--;
      lock_activity();
      finish_source_locked(retirement); // Activation may have assigned its id during release.
      unlock_activity();
    }
  } else if (owned) {
    fc_observe_callback(^{ owned = nil; });
  }
  // Retain cancelled data until every owned event handler has released its captures.
  // Replacement on a live source must not consume queued data for the new handler.
  lock_activity();
  if (_data) {
    if (!_data->owners) abort();
    _data->owners--;
    if (_data->cancelled && !_data->owners) {
      if (_data->task_id) finish_task_locked(_data->task_id);
      _data->task_id = 0;
      _data->pending_value = 0;
    }
  }
  unlock_activity();
}
@end

// Returning +1 avoids an autoreleased wrapper alias extending native handler
// lifetime to the registering thread's unrelated outer pool.
static dispatch_block_t __attribute__((ns_returns_retained))
observed_callback(dispatch_source_t source, dispatch_block_t block, enum source_handler_kind kind) {
  if (!block && !(kind == source_event && objc_getAssociatedObject(source, &source_data_key))) return nil;
  FCSourceHandler *handler = [[FCSourceHandler alloc] initWithBlock:block source:source kind:kind];
  return ^{ [handler invoke]; };
}

static void schedule_wall_after(dispatch_time_t when, dispatch_queue_t queue, dispatch_block_t block) {
  // iOS 26.1's fixed-clock after source keeps its initial uptime index when
  // given a wall deadline. On an explicit workloop that selects the wrong heap.
  // A public, mutable-clock timer source uses libdispatch's anonymous timer
  // heap while delivering on the original queue. Keep the absolute wall deadline.
  __block dispatch_source_t timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
  if (!timer) abort();
  dispatch_source_set_timer(timer, when, DISPATCH_TIME_FOREVER, 0);
  dispatch_source_set_event_handler(timer, ^{
    dispatch_source_t firing = timer;
    timer = nil;
    dispatch_source_cancel(firing);
    block();
  });
  dispatch_resume(timer);
}

static void observed_after(dispatch_time_t when, dispatch_queue_t queue, dispatch_block_t block) {
  if (!fc_manual_clock_enabled()) { dispatch_after(when, queue, block); return; }
  FCDelayedInvocation *item = nil;
  lock_activity();
  if (when != DISPATCH_TIME_FOREVER) {
    if (!decode_dispatch_time) decode_dispatch_time = dlsym(RTLD_DEFAULT, "dispatch_time_to_nsec");
    if (!decode_dispatch_time) abort();
    item = [FCDelayedInvocation new];
    uintptr_t clock;
    uint64_t deadline;
    if (!decode_dispatch_time(when, &clock, &deadline)) abort();
    item.clock = clock;
    item.deadline = deadline;
    item.queue = queue;
    if (!delayed_invocations) delayed_invocations = [NSMutableArray new];
    [delayed_invocations addObject:item];
    admit_due_dispatch_after();
  }
  unlock_activity();
  __block dispatch_block_t owned = block;
  dispatch_block_t callback = ^{
    struct callback_scope scope = {0};
    pthread_threadid_np(NULL, &scope.thread_id);
    lock_activity();
    scope.id = item.task_id;
    if (!scope.id && tracking) scope.id = admit_task(queue);
    scope.next = entered_callbacks;
    entered_callbacks = &scope;
    mark_task_running_locked(scope.id, scope.thread_id);
    if (item) [delayed_invocations removeObjectIdenticalTo:item];
    unlock_activity();
    task_depth++;
    @try { @autoreleasepool { owned(); } }
    @finally {
      @try { @autoreleasepool { owned = nil; } }
      @finally {
        task_depth--;
        lock_activity();
        finish_callback_locked(&scope);
        unlock_activity();
      }
    }
  };
  // dispatch_time_to_nsec returns dispatch_clockid_t (wall = 3), not the
  // zero-based internal timer-heap clock used by libdispatch's source flags.
  if (item && item.clock == 3) schedule_wall_after(when, queue, callback);
  else dispatch_after(when, queue, callback);
}
static void observed_after_f(dispatch_time_t when, dispatch_queue_t queue,
                             void *context, dispatch_function_t function) {
  if (!fc_manual_clock_enabled()) { dispatch_after_f(when, queue, context, function); return; }
  observed_after(when, queue, ^{ function(context); });
}
static void observed_event_handler(dispatch_source_t source, dispatch_block_t block) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_event_handler(source, block); return; }
  @autoreleasepool { dispatch_source_set_event_handler(source, observed_callback(source, block, source_event)); }
}
static void observed_cancel_handler(dispatch_source_t source, dispatch_block_t block) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_cancel_handler(source, block); return; }
  @autoreleasepool { dispatch_source_set_cancel_handler(source, observed_callback(source, block, source_cancel)); }
}
static void observed_registration_handler(dispatch_source_t source, dispatch_block_t block) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_registration_handler(source, block); return; }
  @autoreleasepool { dispatch_source_set_registration_handler(source, observed_callback(source, block, source_registration)); }
}

static dispatch_block_t __attribute__((ns_returns_retained))
source_function(dispatch_source_t source, dispatch_function_t function, enum source_handler_kind kind) {
  if (!function) return observed_callback(source, nil, kind);
  // The source owns its handler. Do not retain it from that handler and create a cycle.
  // libdispatch keeps the source alive during invocation; read its current context then.
  void *reference = (__bridge void *)source;
  return observed_callback(source, ^{ function(dispatch_get_context((__bridge dispatch_source_t)reference)); }, kind);
}
static void observed_event_handler_f(dispatch_source_t source, dispatch_function_t function) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_event_handler_f(source, function); return; }
  @autoreleasepool { dispatch_source_set_event_handler(source, source_function(source, function, source_event)); }
}
static void observed_cancel_handler_f(dispatch_source_t source, dispatch_function_t function) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_cancel_handler_f(source, function); return; }
  @autoreleasepool { dispatch_source_set_cancel_handler(source, source_function(source, function, source_cancel)); }
}
static void observed_registration_handler_f(dispatch_source_t source, dispatch_function_t function) {
  if (!fc_manual_clock_enabled()) { dispatch_source_set_registration_handler_f(source, function); return; }
  @autoreleasepool { dispatch_source_set_registration_handler(source, source_function(source, function, source_registration)); }
}

// The source's public activation state is known only for observed creation.
// Explicit suspensions on an inactive source must not turn an ordinary resume into activation.
static void observed_source_activation(dispatch_object_t object, BOOL resume) {
  FCSourceActivation *state = fc_manual_clock_enabled() ? objc_getAssociatedObject(object, &source_activation_key) : nil;
  if (!state) { if (resume) dispatch_resume(object); else dispatch_activate(object); return; }
  @autoreleasepool {
    __attribute__((objc_precise_lifetime)) NSArray<FCSourceHandler *> *handlers;
    lock_activity();
    if (resume && state->suspensions) state->suspensions--;
    else if (!state->active) {
      state->active = YES;
      if (!state->cancelled) {
        handlers = [source_handlers[@((uintptr_t)(__bridge void *)object)] allObjects];
        for (FCSourceHandler *handler in handlers) [handler admitRegistrationLocked];
      }
    }
    if (resume) dispatch_resume(object); else dispatch_activate(object);
    unlock_activity();
  }
}
static void observed_activate(dispatch_object_t object) { observed_source_activation(object, NO); }
static void observed_resume(dispatch_object_t object) { observed_source_activation(object, YES); }
static void observed_suspend(dispatch_object_t object) {
  FCSourceActivation *state = fc_manual_clock_enabled() ? objc_getAssociatedObject(object, &source_activation_key) : nil;
  if (!state) { dispatch_suspend(object); return; }
  lock_activity();
  if (state->suspensions == NSUIntegerMax) abort();
  state->suspensions++;
  dispatch_suspend(object);
  unlock_activity();
}

static void observed_source_cancel(__unsafe_unretained dispatch_source_t source) {
  // Native callbacks can cancel a source held only by libdispatch's internal lifetime.
  // An ARC parameter retain would resurrect its already released external reference.
  if (!fc_manual_clock_enabled()) { dispatch_source_cancel(source); return; }
  // Weak registration records must not extend native ownership. The temporary
  // snapshot lives outside the lock and drains before returning to the caller.
  @autoreleasepool {
    __attribute__((objc_precise_lifetime)) NSArray<FCSourceHandler *> *handlers;
    FCSourceData *data = objc_getAssociatedObject(source, &source_data_key);
    FCSourceActivation *activation = objc_getAssociatedObject(source, &source_activation_key);
    lock_activity();
    if (activation) activation->cancelled = YES;
    if (data) {
      data->cancelled = YES;
      if (!data->owners) {
        if (data->task_id) finish_task_locked(data->task_id);
        data->task_id = 0;
        data->pending_value = 0;
      }
    }
    handlers = [source_handlers[@((uintptr_t)(__bridge void *)source)] allObjects];
    for (FCSourceHandler *handler in handlers) {
      if (handler->owns_callback) {
        if (!handler->obligation) handler->obligation = begin_source_locked();
        FCSourceObligation *retirement = handler->obligation;
        if (!retirement->cancelled) {
          retirement->cancelled = YES;
          if (retirement->task_id) activity.source_cancellations++;
        }
      }
    }
    dispatch_source_cancel(source); // Async native cancellation precedes later admitted merges.
    unlock_activity();
  }
}

INTERPOSE(observed_activate, dispatch_activate)
INTERPOSE(observed_resume, dispatch_resume)
INTERPOSE(observed_suspend, dispatch_suspend)
INTERPOSE(observed_source_cancel, dispatch_source_cancel)
INTERPOSE(observed_source_create, dispatch_source_create)
INTERPOSE(observed_source_merge, dispatch_source_merge_data)
INTERPOSE(observed_after, dispatch_after)
INTERPOSE(observed_after_f, dispatch_after_f)
INTERPOSE(observed_event_handler, dispatch_source_set_event_handler)
INTERPOSE(observed_event_handler_f, dispatch_source_set_event_handler_f)
INTERPOSE(observed_cancel_handler, dispatch_source_set_cancel_handler)
INTERPOSE(observed_cancel_handler_f, dispatch_source_set_cancel_handler_f)
INTERPOSE(observed_registration_handler, dispatch_source_set_registration_handler)
INTERPOSE(observed_registration_handler_f, dispatch_source_set_registration_handler_f)
