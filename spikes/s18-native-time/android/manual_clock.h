// Manual-mode epoll wakeups for the ART Handler probe. No periodic timeout polling.
#include <pthread.h>
#include <stdatomic.h>
#include <errno.h>
#include <unistd.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <jni.h>

#define EPOLL_SLOTS 64
#define WAKE_TAG UINT64_C(0xfc18fc18fc18fc18)
static pthread_mutex_t wait_lock = PTHREAD_MUTEX_INITIALIZER;
static struct { int used, epoll, wake; } waiters[EPOLL_SLOTS];
static int acknowledgments[2] = {-1, -1};
static void fail_manual(void) { _exit(90); }
static void signal_wake(int fd) {
  uint64_t value = 1;
  if (write(fd, &value, sizeof(value)) != sizeof(value) && errno != EAGAIN) fail_manual();
}
static int manual_epoll(int fd, struct epoll_event *events, int count, int timeout, const sigset_t *mask) {
  // Nonblocking drains must remain nonblocking. Blocking calls wake on readiness or explicit advance.
  pthread_mutex_lock(&wait_lock);
  int slot = -1;
  for (int i = 0; i < EPOLL_SLOTS; i++) if (waiters[i].used && waiters[i].epoll == fd) { slot = i; break; }
  if (slot < 0) {
    for (int i = 0; i < EPOLL_SLOTS; i++) if (!waiters[i].used) { slot = i; break; }
    if (slot < 0) fail_manual();
    int wake = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    struct epoll_event event = {.events = EPOLLIN, .data.u64 = WAKE_TAG};
    if (wake < 0 || epoll_ctl(fd, EPOLL_CTL_ADD, wake, &event)) fail_manual();
    waiters[slot].used = 1; waiters[slot].epoll = fd; waiters[slot].wake = wake;
    // Recheck the clock on first use: advancement may precede registration.
    signal_wake(wake);
  }
  int wake = waiters[slot].wake;
  pthread_mutex_unlock(&wait_lock);
  int (*original)(int, struct epoll_event *, int, int, const sigset_t *) = dlsym(RTLD_NEXT, "epoll_pwait");
  int result = original(fd, events, count, timeout == 0 ? 0 : -1, mask);
  if (result <= 0) return result;
  int kept = 0;
  for (int i = 0; i < result; i++) {
    if (events[i].data.u64 == WAKE_TAG) {
      uint64_t value;
      if (read(wake, &value, sizeof(value)) < 0 && errno != EAGAIN) fail_manual();
    } else events[kept++] = events[i];
  }
  // Returning zero makes Looper/MessageQueue reevaluate deadlines on the new clock.
  return kept;
}

int close(int fd) {
  int (*original)(int) = dlsym(RTLD_NEXT, "close");
  if (!manual) return original(fd);
  // A closed/reused epoll descriptor must never retain another queue's wake registration.
  pthread_mutex_lock(&wait_lock);
  for (int i = 0; i < EPOLL_SLOTS; i++) if (waiters[i].used && waiters[i].epoll == fd) {
    syscall(SYS_close, waiters[i].wake);
    waiters[i].used = 0;
    break;
  }
  int result = original(fd);
  pthread_mutex_unlock(&wait_lock);
  return result;
}

JNIEXPORT void JNICALL Java_dev_frontcap_s18_Steps_prepare(JNIEnv *env, jclass cls) {
  if (!manual || pipe(acknowledgments)) fail_manual();
}
static int64_t manual_advance_limit(void) {
  int64_t largest = base[CLOCK_REALTIME];
  if (base[CLOCK_MONOTONIC] > largest) largest = base[CLOCK_MONOTONIC];
  if (base[CLOCK_BOOTTIME] > largest) largest = base[CLOCK_BOOTTIME];
  return INT64_MAX - largest - atomic_load(&advance_ns);
}
JNIEXPORT jlong JNICALL Java_dev_frontcap_s18_Steps_advanceLimit(JNIEnv *env, jclass cls) {
  if (!manual) fail_manual();
  pthread_mutex_lock(&wait_lock);
  int64_t limit = manual_advance_limit();
  pthread_mutex_unlock(&wait_lock);
  return limit;
}
JNIEXPORT void JNICALL Java_dev_frontcap_s18_Steps_advance(JNIEnv *env, jclass cls, jlong delta) {
  if (!manual || delta < 0) fail_manual();
  pthread_mutex_lock(&wait_lock);
  if (delta > manual_advance_limit()) fail_manual();
  atomic_fetch_add(&advance_ns, delta);
#ifdef FRONTCAP_VIRTUAL_FUTEX
  extern void fc_advance_saver_waits(uint64_t now);
  fc_advance_saver_waits(atomic_load(&advance_ns));
#endif
  for (int i = 0; i < EPOLL_SLOTS; i++) if (waiters[i].used) signal_wake(waiters[i].wake);
  pthread_mutex_unlock(&wait_lock);
}
JNIEXPORT void JNICALL Java_dev_frontcap_s18_Steps_ack(JNIEnv *env, jclass cls) {
  char value = 1;
  if (write(acknowledgments[1], &value, 1) != 1) fail_manual();
}
JNIEXPORT void JNICALL Java_dev_frontcap_s18_Steps_awaitAck(JNIEnv *env, jclass cls) {
  int (*original)(struct pollfd *, nfds_t, int) = dlsym(RTLD_NEXT, "poll");
  struct pollfd fd = {.fd = acknowledgments[0], .events = POLLIN};
  char value;
  if (original(&fd, 1, 2000) != 1 || read(fd.fd, &value, 1) != 1) fail_manual();
}
JNIEXPORT void JNICALL Java_dev_frontcap_s18_Steps_hold(JNIEnv *env, jclass cls, jint millis) {
  struct timespec request = {.tv_sec = millis / 1000, .tv_nsec = (millis % 1000) * 1000000L};
  while (syscall(SYS_nanosleep, &request, &request) < 0) if (errno != EINTR) fail_manual();
}

// Diagnostic checkpoint budget, independent of the virtual application clock.
JNIEXPORT jlong JNICALL Java_dev_frontcap_s18_Steps_realNanos(JNIEnv *env, jclass cls) {
  struct timespec value;
  if (real_clock_gettime(CLOCK_MONOTONIC, &value)) fail_manual();
  return ns(&value);
}


// Optional timing uses the same bracketed reference as the standalone ART clock observer.
#include "clock_observation.h"
JNIEXPORT jlongArray JNICALL Java_dev_frontcap_s18_Steps_clockObservation(JNIEnv *env, jclass cls) {
  (void)cls;
  return diagnostic_clock_observation(env);
}


#include "scheduler_observation.h"
JNIEXPORT jlongArray JNICALL Java_dev_frontcap_s18_Steps_schedulerObservation(JNIEnv *env, jclass cls) {
  (void)cls;
  return diagnostic_scheduler_observation(env);
}
