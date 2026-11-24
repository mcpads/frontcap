// Test-only gate for a native metadata read; never loaded into the app/UI probes.
#include <CoreFoundation/CoreFoundation.h>
#include <stdatomic.h>
#include <stdint.h>
#include <math.h>
#include <poll.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <dlfcn.h>
#include <string.h>
static _Atomic uintptr_t selected_timer;
static _Atomic double expected_date;
static _Atomic int armed, entered_fd, release_fd;
void fc_nstimer_read_gate(CFRunLoopTimerRef timer, CFAbsoluteTime date, int entered, int released) {
  atomic_store(&armed, 0);
  atomic_store(&selected_timer, (uintptr_t)timer); atomic_store(&expected_date, date);
  atomic_store(&entered_fd, entered); atomic_store(&release_fd, released);
  atomic_store(&armed, timer != NULL);
}
static CFAbsoluteTime paused_fire_date(CFRunLoopTimerRef timer) {
  CFAbsoluteTime value = CFRunLoopTimerGetNextFireDate(timer);
  Dl_info caller = {0}; dladdr(__builtin_return_address(0), &caller);
  if ((uintptr_t)timer == atomic_load(&selected_timer) && atomic_load(&armed))
    fprintf(stderr, "metadata-read value=%.12f expected=%.12f caller=%s\n", value, atomic_load(&expected_date), caller.dli_fname ?: "unknown");
  if ((uintptr_t)timer == atomic_load(&selected_timer) && fabs(value - atomic_load(&expected_date)) < .000001 &&
      caller.dli_fname && strstr(caller.dli_fname, "/vclock.dylib") && atomic_exchange(&armed, 0)) {
    char token = 'r';
    if (write(atomic_load(&entered_fd), &token, 1) != 1) _Exit(3);
    struct pollfd input = {.fd = atomic_load(&release_fd), .events = POLLIN};
    if (poll(&input, 1, 5000) != 1 || read(input.fd, &token, 1) != 1) _Exit(3);
  }
  return value;
}
__attribute__((used)) static struct { const void *replacement, *original; } read_interpose
__attribute__((section("__DATA,__interpose"))) = {(const void *)&paused_fire_date, (const void *)&CFRunLoopTimerGetNextFireDate};
