#ifndef FRONTCAP_PROBE_TIMING_H
#define FRONTCAP_PROBE_TIMING_H
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <mach/mach_time.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// Diagnostic boundaries use the runtime's original Mach clock, never virtual time.
// Callers supply fixed phase literals; source_line and the frozen source identify
// repeated observations. Recording failure must not alter a predicate's errno.
static inline void fc_probe_timing(const char *phase, int source_line) {
  int saved_errno = errno;
  const char *path = getenv("FC_PROBE_TIMING_PATH");
  if (path && *path) {
    uint64_t (*host_ticks)(void) = dlsym(RTLD_DEFAULT, "fc_real_abs");
    mach_timebase_info_data_t timebase;
    if (!host_ticks || mach_timebase_info(&timebase) != KERN_SUCCESS || !timebase.denom) {
      dprintf(STDERR_FILENO, "Cannot record probe timing: original Mach clock unavailable\n");
    } else {
      uint64_t ns = (uint64_t)((__uint128_t)host_ticks() * timebase.numer / timebase.denom);
      int fd = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
      if (fd < 0) {
        dprintf(STDERR_FILENO, "Cannot record probe timing: open failed, errno=%d\n", errno);
      } else {
        int written = dprintf(fd, "{\"phase\":\"%s\",\"source_line\":%d,\"pid\":%d,"
          "\"host_monotonic_ns\":%llu}\n", phase, source_line, getpid(), (unsigned long long)ns);
        if (written < 0) dprintf(STDERR_FILENO, "Cannot record probe timing: write failed, errno=%d\n", errno);
        close(fd);
      }
    }
  }
  errno = saved_errno;
}
#define FC_PROBE_TIMING(phase) fc_probe_timing((phase), __LINE__)
#endif
