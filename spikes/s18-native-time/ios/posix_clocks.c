// CPU budgets must keep progressing while application elapsed clocks are held.
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>
#include "probe_assert.h"

static uint64_t (*host_ticks)(void), (*instant)(void);
static void (*advance)(uint64_t);
static mach_timebase_info_data_t timebase;
static mach_port_t thread;
static const clockid_t elapsed_clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW,
  CLOCK_MONOTONIC_RAW_APPROX, CLOCK_UPTIME_RAW, CLOCK_UPTIME_RAW_APPROX};
static const char *elapsed_names[] = {"realtime", "monotonic", "continuous", "continuous_approx", "uptime", "uptime_approx"};

static uint64_t host_ns(void) { return (uint64_t)((__uint128_t)host_ticks()*timebase.numer/timebase.denom); }
static uint64_t cpu_ns(clockid_t clock) {
  if (clock == CLOCK_THREAD_CPUTIME_ID) {
    thread_basic_info_data_t info;
    mach_msg_type_number_t count = THREAD_BASIC_INFO_COUNT;
    require(thread_info(thread, THREAD_BASIC_INFO, (thread_info_t)&info, &count) == KERN_SUCCESS);
    require(count == THREAD_BASIC_INFO_COUNT);
    return ((uint64_t)info.user_time.seconds+info.system_time.seconds)*1000000000ull
      + ((uint64_t)info.user_time.microseconds+info.system_time.microseconds)*1000;
  }
  struct rusage usage;
  require(getrusage(RUSAGE_SELF, &usage) == 0);
  return ((uint64_t)usage.ru_utime.tv_sec+usage.ru_stime.tv_sec)*1000000000ull
    + ((uint64_t)usage.ru_utime.tv_usec+usage.ru_stime.tv_usec)*1000;
}
static void cpu_read(const char *phase, clockid_t clock, int ns_api) {
  uint64_t before = cpu_ns(clock), value;
  struct timespec time = {0};
  errno = 0;
  int result = ns_api ? 0 : clock_gettime(clock, &time);
  value = ns_api ? clock_gettime_nsec_np(clock) : (uint64_t)time.tv_sec*1000000000ull+time.tv_nsec;
  int error = errno;
  uint64_t after = cpu_ns(clock);
  printf("{\"phase\":\"%s\",\"clock\":\"%s\",\"api\":\"%s\",\"before_ns\":%llu,\"value_ns\":%llu,\"after_ns\":%llu,\"result\":%d,\"errno\":%d}\n",
    phase, clock == CLOCK_THREAD_CPUTIME_ID ? "thread" : "process", ns_api ? "nsec" : "timespec", before, value, after, result, error);
}
static void elapsed(const char *phase) {
  for (unsigned i=0; i<sizeof(elapsed_clocks)/sizeof(elapsed_clocks[0]); i++) {
    struct timespec time;
    require(clock_gettime(elapsed_clocks[i], &time) == 0);
    int approximate = i == 3 || i == 5;
    uint64_t before = approximate ? clock_gettime_nsec_np(elapsed_clocks[i-1]) : 0;
    uint64_t ns = clock_gettime_nsec_np(elapsed_clocks[i]);
    uint64_t after = approximate ? clock_gettime_nsec_np(elapsed_clocks[i-1]) : 0;
    char bounds[160] = "";
    if (approximate) snprintf(bounds, sizeof(bounds), ",\"exact_before_ns\":%llu,\"exact_after_ns\":%llu", before, after);
    printf("{\"phase\":\"%s\",\"clock\":\"%s\",\"timespec_ns\":%llu,\"nsec_ns\":%llu%s}\n",
      phase, elapsed_names[i], (uint64_t)time.tv_sec*1000000000ull+time.tv_nsec, ns, bounds);
  }
}
int main(int argc, char **argv) {
  require(argc == 2 && (!strcmp(argv[1], "real") || !strcmp(argv[1], "scaled") || !strcmp(argv[1], "manual")));
  setvbuf(stdout, NULL, _IONBF, 0);
  host_ticks = dlsym(RTLD_DEFAULT, "fc_real_abs");
  instant = dlsym(RTLD_DEFAULT, "fc_manual_now_ns"); advance = dlsym(RTLD_DEFAULT, "fc_advance_ns");
  int injected = host_ticks != NULL, manual = !strcmp(argv[1], "manual");
  require(injected == (strcmp(argv[1], "real") != 0));
  if (!host_ticks) host_ticks = mach_absolute_time;
  require(!manual || (instant && advance && instant() == 0));
  require(mach_timebase_info(&timebase) == KERN_SUCCESS);
  thread = mach_thread_self();
  printf("{\"phase\":\"start\",\"mode\":\"%s\",\"pid\":%d,\"injected\":%s,\"family_brackets\":true}\n",argv[1],getpid(),injected ? "true" : "false");
  for (int api=0;api<2;api++) {
    cpu_read("cpu-before", CLOCK_PROCESS_CPUTIME_ID, api); cpu_read("cpu-before", CLOCK_THREAD_CPUTIME_ID, api);
  }
  elapsed("elapsed-before");
  uint64_t began = host_ns(), cpu_before = clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID), cpu_after = cpu_before;
  uint64_t native_before = cpu_ns(CLOCK_THREAD_CPUTIME_ID), native_after;
  volatile uint64_t work = 1;
  while (cpu_after-cpu_before < 20000000 && host_ns()-began < 2000000000) {
    for (unsigned i=0;i<4096;i++) work = work*6364136223846793005ull+1;
    cpu_after = clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID);
  }
  uint64_t finished = host_ns(); native_after = cpu_ns(CLOCK_THREAD_CPUTIME_ID);
  int completed = cpu_after >= cpu_before && cpu_after-cpu_before >= 20000000;
  printf("{\"phase\":\"cpu-budget\",\"completed\":%s,\"budget_ns\":20000000,\"host_limit_ns\":2000000000,"
    "\"host_ns\":%llu,\"cpu_before_ns\":%llu,\"cpu_after_ns\":%llu,\"native_before_ns\":%llu,\"native_after_ns\":%llu}\n",
    completed ? "true" : "false",finished-began,cpu_before,cpu_after,native_before,native_after);
  usleep(100000);
  uint64_t elapsed_host = host_ns()-began;
  elapsed("elapsed-after");
  printf("{\"phase\":\"elapsed-window\",\"host_ns\":%llu,\"now_ns\":%llu}\n",elapsed_host,manual ? instant() : 0);
  if (manual) { advance(1000000000); elapsed("elapsed-advanced"); }
  for (int api=0;api<2;api++) {
    cpu_read("cpu-after", CLOCK_PROCESS_CPUTIME_ID, api); cpu_read("cpu-after", CLOCK_THREAD_CPUTIME_ID, api);
  }
  for (unsigned i=0;i<2;i++) {
    clockid_t invalid = i ? (clockid_t)INT_MAX : (clockid_t)-1;
    struct timespec time = {0}; errno = 0;
    int result = clock_gettime(invalid, &time), error = errno;
    errno = 0; uint64_t value = clock_gettime_nsec_np(invalid); int ns_error = errno;
    printf("{\"phase\":\"invalid\",\"id\":%d,\"result\":%d,\"errno\":%d,\"nsec\":%llu,\"nsec_errno\":%d}\n",invalid,result,error,value,ns_error);
  }
  require(mach_port_deallocate(mach_task_self(), thread) == KERN_SUCCESS);
  return completed ? 0 : 1;
}
