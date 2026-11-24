// S18: run an Android process on a scaled clock (LD_PRELOAD). Every clock_gettime returns
// base + (real - base) * FC_SCALE; relative waits through libc entry points shrink to match.
// Nothing here may log from the constructor: liblog reads the clock under its own lock.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <poll.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/time.h>
#include <time.h>
#include <stdatomic.h>
#include <sys/prctl.h>
#include <string.h>
#include <math.h>

#define CLOCKS 12
static double scale = 1;
static int manual, host_render;
static int render_host_clock(void) {
  if (!host_render) return 0;
  char name[16] = {0};
  return prctl(PR_GET_NAME, name) == 0 && (!strcmp(name, "RenderThread") || !strcmp(name, "InsetsAnimation"));
}
static _Atomic int64_t advance_ns;
#ifdef FRONTCAP_VIRTUAL_FUTEX
__attribute__((visibility("hidden"))) uint64_t fc_virtual_now(void) { return atomic_load(&advance_ns); }
#endif
static int64_t base[CLOCKS];
static int (*real_clock_gettime)(clockid_t, struct timespec *);

static int64_t ns(const struct timespec *t) { return (int64_t)t->tv_sec * 1000000000LL + t->tv_nsec; }
static void set_ns(struct timespec *t, int64_t v) { t->tv_sec = v / 1000000000LL; t->tv_nsec = v % 1000000000LL; }

static void init(void) {
  if (real_clock_gettime) return;
  int (*f)(clockid_t, struct timespec *) = dlsym(RTLD_NEXT, "clock_gettime");
  for (int c = 0; c < CLOCKS; c++) { struct timespec t; if (f(c, &t) == 0) base[c] = ns(&t); }
  const char *s = getenv("FC_SCALE");
  if (s) scale = atof(s);
  if (!isfinite(scale) || scale <= 0) _Exit(2);
  manual = getenv("FC_MANUAL") != NULL;
  host_render = getenv("FC_HOST_RENDER") != NULL;
  real_clock_gettime = f;
}
__attribute__((constructor)) static void ctor(void) { init(); }

// Query the receiver clock without applying the caller's host-render exception.
// Experimental Handler-boundary agents use this; ordinary clock calls are unchanged.
int fc_manual_uptime_ns(int64_t *value) {
  init();
  if (!manual || !value) return 0;
  *value = base[CLOCK_MONOTONIC] + atomic_load(&advance_ns);
  return 1;
}


int clock_gettime(clockid_t c, struct timespec *t) {
  init();
  int r = real_clock_gettime(c, t);
  if (r == 0 && c >= 0 && c < CLOCKS && !render_host_clock()) {
    if (manual) {
      if (c == CLOCK_REALTIME || c == CLOCK_MONOTONIC || c == CLOCK_BOOTTIME)
        set_ns(t, base[c] + atomic_load(&advance_ns));
    } else set_ns(t, base[c] + (int64_t)((double)(ns(t) - base[c]) * scale));
  }
  return r;
}
int gettimeofday(struct timeval *tv, struct timezone *tz) {
  (void)tz;
  struct timespec t;
  clock_gettime(CLOCK_REALTIME, &t);
  if (tv) { tv->tv_sec = t.tv_sec; tv->tv_usec = t.tv_nsec / 1000; }
  return 0;
}

#include "manual_clock.h"

static int shrink_ms(int ms) { if (ms <= 0) return ms; int r = (int)(ms / scale); return r < 1 ? 1 : r; }
int epoll_wait(int e, struct epoll_event *ev, int n, int to) {
  if (manual && !render_host_clock()) return manual_epoll(e, ev, n, to, NULL);
  static int (*f)(int, struct epoll_event *, int, int);
  if (!f) f = dlsym(RTLD_NEXT, "epoll_wait");
  return f(e, ev, n, shrink_ms(to));
}
int epoll_pwait(int e, struct epoll_event *ev, int n, int to, const sigset_t *ss) {
  if (manual && !render_host_clock()) return manual_epoll(e, ev, n, to, ss);
  static int (*f)(int, struct epoll_event *, int, int, const sigset_t *);
  if (!f) f = dlsym(RTLD_NEXT, "epoll_pwait");
  return f(e, ev, n, shrink_ms(to), ss);
}
int poll(struct pollfd *p, nfds_t n, int to) {
  static int (*f)(struct pollfd *, nfds_t, int);
  if (!f) f = dlsym(RTLD_NEXT, "poll");
  return f(p, n, shrink_ms(to));
}
int nanosleep(const struct timespec *req, struct timespec *rem) {
  static int (*f)(const struct timespec *, struct timespec *);
  if (!f) f = dlsym(RTLD_NEXT, "nanosleep");
  struct timespec t;
  set_ns(&t, (int64_t)((double)ns(req) / scale));
  return f(&t, rem);
}
