// Experimental UIKit scheduling domain. FC_UC_HOST_TIME keeps private UpdateCycle
// pacing on host time; public application clocks still come from vclock.dylib.
// FC_TRACE_UI records timer admission and never changes a submitted deadline.
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <unistd.h>
#include <string.h>
#include <objc/runtime.h>

extern int64_t UCTimeNow(void);
extern void UCDriverTimerArm(void *driver, int64_t deadline);
struct driver_step { void *context; void *function; };
extern struct driver_step UCDriverNextStep(void *driver);
static int trace_fd = -1;
static _Atomic unsigned records;
static uint64_t (*host_now)(void);
static int64_t read_update_time(void) { return host_now ? (int64_t)host_now() : UCTimeNow(); }

static double (*original_frame_deadline)(id, SEL, double);
static void (*enter_host_mach)(void), (*leave_host_mach)(void);

static double frame_deadline(id source, SEL selector, double timestamp) {
  enter_host_mach();
  @try { return original_frame_deadline(source, selector, timestamp); }
  @finally { leave_host_mach(); }
}

static void install_frame_clock(void) {
  enter_host_mach = dlsym(RTLD_DEFAULT, "fc_host_mach_time_enter");
  leave_host_mach = dlsym(RTLD_DEFAULT, "fc_host_mach_time_leave");
  Class source = objc_getClass("CADynamicFrameRateSource");
  Method method = class_getInstanceMethod(source, sel_registerName("commitDeadlineAfterTimestamp:"));
  char result[16] = {0}, argument[16] = {0};
  if (method) {
    method_getReturnType(method, result, sizeof(result));
    method_getArgumentType(method, 2, argument, sizeof(argument));
  }
  if (!enter_host_mach || !leave_host_mach || !method || method_getNumberOfArguments(method) != 3 ||
      strcmp(result, "d") || strcmp(argument, "d")) {
    fprintf(stderr, "Unsupported host frame-source clock boundary\n");
    abort();
  }
  original_frame_deadline = (double (*)(id, SEL, double))method_setImplementation(method, (IMP)frame_deadline);
}

__attribute__((constructor)) static void configure_ui_schedule(void) {
  if (getenv("FC_UC_HOST_TIME")) {
    host_now = dlsym(RTLD_DEFAULT, "fc_real_abs");
    if (!host_now) abort();
    install_frame_clock();
  }
  if (!getenv("FC_TRACE_UI") || !getenv("HOME")) return;
  char path[4096];
  snprintf(path, sizeof(path), "%s/Documents/ui-schedule-%s.txt", getenv("HOME"),
           getenv("FC_MANUAL") ? "manual" : "real");
  trace_fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
}

static void trace_arm(void *driver, int64_t deadline) {
  if (trace_fd >= 0 && atomic_fetch_add(&records, 1) < 128) {
    struct driver_step step = UCDriverNextStep(driver);
    Dl_info caller = {0}, callback = {0};
    void *address = __builtin_return_address(0);
    dladdr(address, &caller);
    dladdr(step.function, &callback);
    char line[1024];
    int length = snprintf(line, sizeof(line),
      "deadline=%lld now=%lld caller=%s+%llu next=%s+%llu\n",
      (long long)deadline, (long long)read_update_time(),
      caller.dli_sname ? caller.dli_sname : "unknown",
      (unsigned long long)((uintptr_t)address - (uintptr_t)caller.dli_saddr),
      callback.dli_sname ? callback.dli_sname : "unknown",
      (unsigned long long)((uintptr_t)step.function - (uintptr_t)callback.dli_saddr));
    if (length > 0 && (size_t)length < sizeof(line)) write(trace_fd, line, (size_t)length);
  }
  UCDriverTimerArm(driver, deadline);
}
__attribute__((used,section("__DATA,__interpose"))) static struct {
  const void *replacement, *original;
} arm_interpose = {(const void *)&trace_arm, (const void *)&UCDriverTimerArm};
__attribute__((used,section("__DATA,__interpose"))) static struct {
  const void *replacement, *original;
} time_interpose = {(const void *)&read_update_time, (const void *)&UCTimeNow};
