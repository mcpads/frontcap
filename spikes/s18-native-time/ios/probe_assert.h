#ifndef FRONTCAP_PROBE_ASSERT_H
#define FRONTCAP_PROBE_ASSERT_H
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

// Failure-only context; errno is a snapshot, not an inferred cause. A failed
// diagnostic write must not replace the probe's existing process exit code.
static inline void fc_probe_failure(const char *condition, const char *file, int line, int code) {
  int saved_errno = errno;
  dprintf(STDERR_FILENO, "probe-failure file=%s line=%d condition=%s errno_snapshot=%d exit_code=%d\n",
          file, line, condition, saved_errno, code);
  _Exit(code);
}
#define FC_REQUIRE(condition, code) do { \
  if (!(condition)) fc_probe_failure(#condition, __FILE__, __LINE__, (code)); \
} while (0)
#define require(condition) FC_REQUIRE(condition, 3)
#endif
