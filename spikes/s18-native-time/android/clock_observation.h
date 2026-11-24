// Shared diagnostic clock sample: real monotonic brackets around one thread-CPU read.
#pragma once
#include <jni.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static jlongArray diagnostic_clock_observation(JNIEnv *env) {
  struct timespec before, cpu, after;
  if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &before) ||
      syscall(SYS_clock_gettime, CLOCK_THREAD_CPUTIME_ID, &cpu) ||
      syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &after)) {
    jclass error = (*env)->FindClass(env, "java/lang/IllegalStateException");
    if (error) (*env)->ThrowNew(env, error, "Native clock observation failed");
    return NULL;
  }
  jlong values[] = {
    (jlong)before.tv_sec * 1000000000LL + before.tv_nsec,
    (jlong)cpu.tv_sec * 1000000000LL + cpu.tv_nsec,
    (jlong)after.tv_sec * 1000000000LL + after.tv_nsec,
    (jlong)syscall(SYS_gettid)
  };
  jlongArray result = (*env)->NewLongArray(env, 4);
  if (result) (*env)->SetLongArrayRegion(env, result, 0, 4, values);
  return result;
}
