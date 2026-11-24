// Optional per-thread scheduler counters. Unavailable/disabled data is never a zero-wait claim.
#pragma once
#include <jni.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static jlongArray diagnostic_scheduler_observation(JNIEnv *env) {
  const char *enabled = getenv("FC_REGISTRATION_SCHEDSTATS");
  if (!enabled || enabled[0] != '1' || enabled[1]) return NULL;
  struct timespec before, after;
  if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &before)) return NULL;
  jlong error = 0, running = -1, waiting = -1, slices = -1, collecting = -1, capability_error = 0;
  char bytes[256];
  int fd = open("/proc/thread-self/schedstat", O_RDONLY | O_CLOEXEC);
  if (fd < 0) error = errno;
  else {
    ssize_t size = read(fd, bytes, sizeof(bytes)-1);
    if (size < 0) error = errno;
    else {
      bytes[size] = 0;
      unsigned long long a, b, c; char extra;
      if (sscanf(bytes, "%llu %llu %llu %c", &a, &b, &c, &extra) != 3 ||
          a > 0x7fffffffffffffffULL || b > 0x7fffffffffffffffULL || c > 0x7fffffffffffffffULL) error = EPROTO;
      else { running = a; waiting = b; slices = c; }
    }
    close(fd);
  }
  fd = open("/proc/sys/kernel/sched_schedstats", O_RDONLY | O_CLOEXEC);
  if (fd < 0) capability_error = errno;
  else {
    ssize_t size = read(fd, bytes, sizeof(bytes)-1);
    if (size < 0) capability_error = errno;
    else if (size == 0) capability_error = EPROTO;
    else {
      bytes[size] = 0; int value; char extra;
      if (sscanf(bytes, "%d %c", &value, &extra) == 1 && (value == 0 || value == 1)) collecting = value;
      else capability_error = EPROTO;
    }
    close(fd);
  }
  if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &after)) return NULL;
  jlong values[] = {(jlong)before.tv_sec*1000000000LL+before.tv_nsec,
      (jlong)after.tv_sec*1000000000LL+after.tv_nsec, (jlong)syscall(SYS_gettid),
      error, running, waiting, slices, collecting, capability_error};
  jlongArray result = (*env)->NewLongArray(env, 9);
  if (result) (*env)->SetLongArrayRegion(env, result, 0, 9, values);
  return result;
}
