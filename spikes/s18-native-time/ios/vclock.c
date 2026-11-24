// S18: run an iOS simulator process on a scaled clock. Every time read returns
// base + (real - base) * FC_SCALE; every kernel timer deadline is mapped back to real time.
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <sys/event.h>
#include <sys/time.h>
#include <time.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <math.h>
#include <pthread.h>
#include <errno.h>
#include <execinfo.h>
#include <fcntl.h>
#include <unistd.h>
#include <dlfcn.h>

#define INTERPOSE(replacement, original) \
  __attribute__((used)) static struct { const void *r; const void *o; } interpose_##original \
  __attribute__((section("__DATA,__interpose"))) = {(const void *)&replacement, (const void *)&original};

// Private libsystem_kernel entry points (no public header).
extern kern_return_t mach_get_times(uint64_t *absolute, uint64_t *continuous, struct timespec *wall);
extern kern_return_t mk_timer_arm(mach_port_t, uint64_t);
extern kern_return_t mk_timer_arm_leeway(mach_port_t, uint64_t, uint64_t, uint64_t);
extern kern_return_t mk_timer_cancel(mach_port_t, uint64_t *);
extern kern_return_t mk_timer_destroy(mach_port_t);
extern int __workq_kernreturn(int options, void *item, int affinity, int prio);
struct kevent_qos_s {
  uint64_t ident; int16_t filter; uint16_t flags; int32_t qos; uint64_t udata;
  uint32_t fflags; uint32_t xflags; int64_t data; uint64_t ext[4];
};
extern int kevent_qos(int, const struct kevent_qos_s *, int, struct kevent_qos_s *, int, void *, size_t *, unsigned);
extern int kevent_id(uint64_t, const struct kevent_qos_s *, int, struct kevent_qos_s *, int, void *, size_t *, unsigned);
#define WQOPS_THREAD_KEVENT_RETURN 0x40
#define WQOPS_THREAD_WORKLOOP_RETURN 0x100

static double scale = 1;
static int manual;
static _Atomic uint64_t advance_ns;
static uint64_t base_abs, base_cont, base_wall_ns;
static mach_timebase_info_data_t timebase;
// Clock hooks must not instantiate compiler TLS: dyld may allocate that storage,
// and the allocator can call mach_absolute_time before TLS initialization finishes.
static pthread_key_t host_mach_key;
static _Atomic int host_mach_key_ready;
static _Atomic int timers_mapped, mk_timers_mapped;

static int trace_fd = -1;
static _Thread_local int tracing;
static _Atomic unsigned trace_count;
static pthread_mutex_t trace_lock = PTHREAD_MUTEX_INITIALIZER;
static void trace_timer(const char *kind, mach_port_t port, uint64_t deadline, int host_ui) {
  if (trace_fd < 0 || tracing) return;
  unsigned count = atomic_load(&trace_count);
  do { if (count >= 128) return; }
  while (!atomic_compare_exchange_weak(&trace_count, &count, count + 1));
  tracing = 1;
  uint64_t instant = atomic_load(&advance_ns);
  void *frames[24];
  int frame_count = backtrace(frames, 24);
  // Serialize the header and its stack as one record. The wrapper records before
  // acquiring the timer registry; registry operations do not acquire trace_lock.
  pthread_mutex_lock(&trace_lock);
  char line[384];
  int length = snprintf(line, sizeof(line),
    "{\"timer\":\"%s\",\"port\":%u,\"deadline_ticks\":%llu,"
    "\"base_ticks\":%llu,\"now_ns\":%llu,\"numer\":%u,\"denom\":%u,\"manual\":%s,\"host_ui\":%s}\n",
    kind, port, deadline, base_abs, instant, timebase.numer, timebase.denom,
    manual && !host_ui ? "true" : "false", host_ui ? "true" : "false");
  if (length > 0 && (size_t)length < sizeof(line)) {
    write(trace_fd, line, (size_t)length);
    backtrace_symbols_fd(frames, frame_count, trace_fd);
  }
  pthread_mutex_unlock(&trace_lock);
  tracing = 0;
}

__attribute__((constructor)) static void init(void) {
  struct timeval tv;
  base_abs = mach_absolute_time();
  base_cont = mach_continuous_time();
  gettimeofday(&tv, NULL);
  base_wall_ns = (uint64_t)tv.tv_sec * 1000000000ull + (uint64_t)tv.tv_usec * 1000ull;
  mach_timebase_info(&timebase);
  const char *s = getenv("FC_SCALE");
  if (s) scale = atof(s);
  if (!isfinite(scale) || scale <= 0) {
    fputs("FC_SCALE must be finite and positive; use FC_MANUAL for stepping\n", stderr);
    _Exit(2);
  }
  manual = getenv("FC_MANUAL") != NULL;
  if (pthread_key_create(&host_mach_key, NULL)) abort();
  atomic_store_explicit(&host_mach_key_ready, 1, memory_order_release);
  if (getenv("FC_TRACE_TIMERS") && getenv("HOME")) {
    char path[4096];
    const char *destination = getenv("FC_TRACE_TIMERS");
    if (!strcmp(destination, "1")) snprintf(path, sizeof(path), "%s/Documents/native-timers.txt", getenv("HOME"));
    else snprintf(path, sizeof(path), "%s", destination);
    trace_fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
  }
}

__attribute__((destructor)) static void report(void) {
  if (getenv("FC_REPORT"))
    fprintf(stderr, "vclock: kevent timers mapped=%d mk_timers mapped=%d\n", timers_mapped, mk_timers_mapped);
}

static uint64_t ticks_to_ns(uint64_t t) { return t * timebase.numer / timebase.denom; }
static uint64_t ns_to_ticks(uint64_t t) { return t * timebase.denom / timebase.numer; }
static uint64_t to_virtual(uint64_t real, uint64_t base) {
  return base + (manual ? ns_to_ticks(atomic_load(&advance_ns)) : (uint64_t)((double)(real - base) * scale));
}
static uint64_t to_real(uint64_t virt, uint64_t base) {
  if (!manual) return virt <= base ? virt : base + (uint64_t)((double)(virt - base) / scale);
  int wall = base == base_wall_ns;
  uint64_t elapsed = atomic_load(&advance_ns);
  uint64_t current = base + (wall ? elapsed : ns_to_ticks(elapsed));
  uint64_t real = wall ? base_wall_ns + ticks_to_ns(mach_absolute_time() - base_abs)
                      : (base == base_cont ? mach_continuous_time() : mach_absolute_time());
  // Future manual timers are disabled by the registry, never submitted as periodic wakeups.
  if (virt > current) abort();
  return real;
}

#include "manual_timers.h"

// The real clock, for probes that measure wall time.
uint64_t fc_real_abs(void) { return mach_absolute_time(); }
int fc_manual_clock_enabled(void) { return manual; }
uint64_t fc_manual_now_ns(void) { return atomic_load(&advance_ns); }
// External commands must also fit the absolute clock bases and the existing integer
// conversion intermediates. The caller serializes advances; native probes retain their API.
uint64_t fc_manual_advance_limit_ns(void) {
  uint64_t limit = base_wall_ns < INT64_MAX ? INT64_MAX - base_wall_ns : 0;
  uint64_t ticks = UINT64_MAX / timebase.numer;
  uint64_t base = base_abs > base_cont ? base_abs : base_cont;
  uint64_t clock_limit = base < ticks ? (uint64_t)((__uint128_t)(ticks - base) * timebase.numer / timebase.denom) : 0;
  if (clock_limit < limit) limit = clock_limit;
  if (UINT64_MAX / timebase.denom < limit) limit = UINT64_MAX / timebase.denom;
  uint64_t now = atomic_load(&advance_ns);
  return now < limit ? limit - now : 0;
}


// Time reads.
static uint64_t v_abs(void) { return to_virtual(mach_absolute_time(), base_abs); }
static uint64_t v_cont(void) { return to_virtual(mach_continuous_time(), base_cont); }
static uint64_t v_wall_ns(void) {
  return base_wall_ns + (manual ? atomic_load(&advance_ns)
                              : (uint64_t)((double)ticks_to_ns(mach_absolute_time() - base_abs) * scale));
}
static int v_clock_ns(clockid_t c, uint64_t *ns) {
  switch (c) {
    case CLOCK_REALTIME: *ns = v_wall_ns(); return 1;
    case CLOCK_MONOTONIC:
    case CLOCK_MONOTONIC_RAW:
    case CLOCK_MONOTONIC_RAW_APPROX: *ns = ticks_to_ns(v_cont()); return 1;
    case CLOCK_UPTIME_RAW:
    case CLOCK_UPTIME_RAW_APPROX: *ns = ticks_to_ns(v_abs()); return 1;
    default: return 0; // CPU accounting and unsupported IDs retain the system API.
  }
}
// Private UIKit frame-source calculations share UpdateCycle's host scheduling domain.
// Only the synchronous calculation's thread is scoped; application reads stay virtual.
static uintptr_t host_mach_depth(void) {
  return atomic_load_explicit(&host_mach_key_ready, memory_order_acquire)
    ? (uintptr_t)pthread_getspecific(host_mach_key) : 0;
}
void fc_host_mach_time_enter(void) {
  if (!atomic_load_explicit(&host_mach_key_ready, memory_order_acquire)) abort();
  uintptr_t depth = host_mach_depth();
  if (depth == UINTPTR_MAX || pthread_setspecific(host_mach_key, (void *)(depth + 1))) abort();
}
void fc_host_mach_time_leave(void) {
  uintptr_t depth = host_mach_depth();
  if (!depth || pthread_setspecific(host_mach_key, (void *)(depth - 1))) abort();
}
static uint64_t my_mach_absolute_time(void) {
  return host_mach_depth() ? mach_absolute_time() : v_abs();
}
static uint64_t my_mach_approximate_time(void) { return v_abs(); }
static uint64_t my_mach_continuous_time(void) { return v_cont(); }
static uint64_t my_mach_continuous_approximate_time(void) { return v_cont(); }
static uint64_t my_clock_gettime_nsec_np(clockid_t c) {
  uint64_t ns;
  return v_clock_ns(c, &ns) ? ns : clock_gettime_nsec_np(c);
}
static int my_clock_gettime(clockid_t c, struct timespec *ts) {
  uint64_t ns;
  if (!v_clock_ns(c, &ns)) return clock_gettime(c, ts);
  ts->tv_sec = (time_t)(ns / 1000000000ull); ts->tv_nsec = (long)(ns % 1000000000ull);
  return 0;
}
static int my_gettimeofday(struct timeval *tv, void *tz) {
  int r = gettimeofday(tv, tz);
  if (tv) { uint64_t us = v_wall_ns() / 1000; tv->tv_sec = (time_t)(us / 1000000); tv->tv_usec = (suseconds_t)(us % 1000000); }
  return r;
}
// libdispatch's timer heap reads all three clocks here, not through the calls above.
static kern_return_t my_mach_get_times(uint64_t *a, uint64_t *c, struct timespec *wall) {
  kern_return_t r = mach_get_times(a, c, wall);
  if (a) *a = to_virtual(*a, base_abs);
  if (c) *c = to_virtual(*c, base_cont);
  if (wall) my_clock_gettime(CLOCK_REALTIME, wall);
  return r;
}

// Waits: absolute timer deadlines computed from virtual reads go back to the real clock.
static void map_timers(struct kevent_qos_s *ev, int n) {
  for (int i = 0; i < n; i++) {
    if (ev[i].filter != EVFILT_TIMER || !(ev[i].fflags & NOTE_ABSOLUTE)) continue;
    if (ev[i].fflags & NOTE_MACHTIME)
      ev[i].data = (int64_t)to_real((uint64_t)ev[i].data, (ev[i].fflags & NOTE_MACH_CONTINUOUS_TIME) ? base_cont : base_abs);
    else if (ev[i].fflags & NOTE_NSECONDS)
      ev[i].data = (int64_t)to_real((uint64_t)ev[i].data, base_wall_ns);
    else
      continue;
    timers_mapped++;
  }
}
static const struct kevent_qos_s *mapped_copy(const struct kevent_qos_s *in, int n, struct kevent_qos_s *buf) {
  if (n <= 0 || n > 64) return in;
  memcpy(buf, in, sizeof(*in) * (size_t)n);
  map_timers(buf, n);
  return buf;
}
static int my_kevent_qos(int kq, const struct kevent_qos_s *c, int nc, struct kevent_qos_s *e, int ne, void *d, size_t *da, unsigned f) {
  struct kevent_qos_s buf[64];
  if (manual) {
    int count = submit_manual_changes(c, nc, buf, (struct timer_queue){.fd = kq, .flags = f & FC_KEVENT_WORKQ});
    return kevent_qos(kq, buf, count, e, ne, d, da, f);
  }
  return kevent_qos(kq, mapped_copy(c, nc, buf), nc, e, ne, d, da, f);
}
static int my_kevent_id(uint64_t id, const struct kevent_qos_s *c, int nc, struct kevent_qos_s *e, int ne, void *d, size_t *da, unsigned f) {
  struct kevent_qos_s buf[64];
  if (manual) {
    int count = submit_manual_changes(c, nc, buf, (struct timer_queue){.id = id, .flags = FC_KEVENT_WORKLOOP});
    return kevent_id(id, buf, count, e, ne, d, da, f);
  }
  return kevent_id(id, mapped_copy(c, nc, buf), nc, e, ne, d, da, f);
}
// libdispatch's workqueue threads hand their timer changes to the kernel on return, here.
static int my_workq_kernreturn(int options, void *item, int affinity, int prio) {
  struct kevent_qos_s buf[64];
  if (manual && item && affinity > 0 &&
      (options & (WQOPS_THREAD_KEVENT_RETURN | WQOPS_THREAD_WORKLOOP_RETURN))) {
    // libpthread passes the original kernel event buffer back. XNU places the workloop ID
    // immediately before that buffer; libdispatch preserves the pointer while replacing events.
    struct timer_queue queue = {.fd = -1, .flags = FC_KEVENT_WORKQ};
    if (options & WQOPS_THREAD_WORKLOOP_RETURN) {
      uintptr_t top = (uintptr_t)pthread_get_stackaddr_np(pthread_self());
      size_t size = pthread_get_stacksize_np(pthread_self());
      uintptr_t buffer = (uintptr_t)item;
      if (affinity > 64 || buffer < top - size + sizeof(uint64_t) || buffer > top ||
          (size_t)affinity * sizeof(struct kevent_qos_s) > top - buffer)
        timer_error("workloop event buffer is outside the current thread stack");
      queue = (struct timer_queue){.flags = FC_KEVENT_WORKLOOP};
      memcpy(&queue.id, (const char *)item - sizeof(queue.id), sizeof(queue.id));
      if (!queue.id) timer_error("workloop return has no queue id");
      if (trace_fd >= 0) {
        static _Atomic unsigned workloop_traces;
        if (atomic_fetch_add(&workloop_traces, 1) < 32) {
          char line[256];
          int length = snprintf(line, sizeof(line),
            "{\"workloop_return\":%llu,\"pid\":%d,\"count\":%d,\"now_ns\":%llu}\n",
            queue.id, getpid(), affinity, atomic_load(&advance_ns));
          if (length > 0 && (size_t)length < sizeof(line)) write(trace_fd, line, (size_t)length);
        }
      }
    }
    affinity = submit_manual_changes(item, affinity, buf, queue);
    return __workq_kernreturn(options, buf, affinity, prio);
  }
  if ((options & (WQOPS_THREAD_KEVENT_RETURN | WQOPS_THREAD_WORKLOOP_RETURN)) && item && affinity > 0 && affinity <= 64)
    item = (void *)mapped_copy(item, affinity, buf);
  return __workq_kernreturn(options, item, affinity, prio);
}
// CFRunLoop timers.
static int host_update_timer(void *caller) {
  if (!manual || !getenv("FC_UC_HOST_TIME")) return 0;
  Dl_info info;
  return dladdr(caller, &info) && info.dli_fname && strstr(info.dli_fname, "/UpdateCycle.framework/");
}
static kern_return_t my_mk_timer_arm(mach_port_t p, uint64_t t) {
  int host_ui = host_update_timer(__builtin_return_address(0));
  trace_timer("mach", p, t, host_ui);
  if (host_ui) return mk_timer_arm(p, t);
  mk_timers_mapped++;
  return manual ? register_mach_timer(p, t, 0) : mk_timer_arm(p, to_real(t, base_abs));
}
static kern_return_t my_mk_timer_arm_leeway(mach_port_t p, uint64_t fl, uint64_t t, uint64_t lw) {
  int host_ui = host_update_timer(__builtin_return_address(0));
  trace_timer("mach-leeway", p, t, host_ui);
  if (host_ui) return mk_timer_arm_leeway(p, fl, t, lw);
  mk_timers_mapped++;
  return manual ? register_mach_timer(p, t, fl) : mk_timer_arm_leeway(p, fl, to_real(t, base_abs), lw);
}
static kern_return_t my_mk_timer_cancel(mach_port_t p, uint64_t *time) {
  return manual ? remove_mach_timer(p, time, 0) : mk_timer_cancel(p, time);
}
static kern_return_t my_mk_timer_destroy(mach_port_t p) {
  return manual ? remove_mach_timer(p, NULL, 1) : mk_timer_destroy(p);
}

INTERPOSE(my_mach_absolute_time, mach_absolute_time)
INTERPOSE(my_mach_approximate_time, mach_approximate_time)
INTERPOSE(my_mach_continuous_time, mach_continuous_time)
INTERPOSE(my_mach_continuous_approximate_time, mach_continuous_approximate_time)
INTERPOSE(my_clock_gettime_nsec_np, clock_gettime_nsec_np)
INTERPOSE(my_clock_gettime, clock_gettime)
INTERPOSE(my_gettimeofday, gettimeofday)
INTERPOSE(my_mach_get_times, mach_get_times)
INTERPOSE(my_kevent_qos, kevent_qos)
INTERPOSE(my_kevent_id, kevent_id)
INTERPOSE(my_workq_kernreturn, __workq_kernreturn)
INTERPOSE(my_mk_timer_arm, mk_timer_arm)
INTERPOSE(my_mk_timer_arm_leeway, mk_timer_arm_leeway)
INTERPOSE(my_mk_timer_cancel, mk_timer_cancel)
INTERPOSE(my_mk_timer_destroy, mk_timer_destroy)
