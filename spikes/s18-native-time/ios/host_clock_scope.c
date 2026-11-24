// Run with FC_MANUAL=1 and vclock.dylib injected. First reads on fresh threads must
// remain virtual; nested host scheduling scopes must affect only their own thread.
#include <assert.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

static void (*enter_host)(void), (*leave_host)(void);
static uint64_t frozen;
static void check_host_progress(void) {
  uint64_t before = mach_absolute_time();
  usleep(10000);
  assert(mach_absolute_time() > before);
}
static void *fresh_thread(void *unused) {
  (void)unused;
  assert(mach_absolute_time() == frozen);
  enter_host();
  enter_host();
  check_host_progress();
  leave_host();
  check_host_progress();
  leave_host();
  assert(mach_absolute_time() == frozen);
  return NULL;
}
int main(void) {
  enter_host = dlsym(RTLD_DEFAULT, "fc_host_mach_time_enter");
  leave_host = dlsym(RTLD_DEFAULT, "fc_host_mach_time_leave");
  assert(enter_host && leave_host);
  frozen = mach_absolute_time();
  usleep(10000);
  assert(mach_absolute_time() == frozen);
  enter_host();
  enter_host();
  for (unsigned i = 0; i < 16; ++i) {
    pthread_t thread;
    assert(!pthread_create(&thread, NULL, fresh_thread, NULL));
    assert(!pthread_join(thread, NULL));
    check_host_progress();
  }
  leave_host();
  check_host_progress();
  leave_host();
  assert(mach_absolute_time() == frozen);
  puts("HOST_CLOCK_SCOPE passed: fresh threads, nested scopes, thread isolation");
}
