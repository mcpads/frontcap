// Internal to vclock.c. Manual mode retains native timers while their virtual deadline is future,
// then explicitly arms the kernel when advancing makes them due. No periodic wakeup is scheduled.
// The fixed registry is for this probe; unsupported routes and exhaustion fail rather than bypass it.
#define TIMER_SLOTS 128
#define FC_KEVENT_WORKQ 0x20
#define FC_KEVENT_WORKLOOP 0x400
#define FC_KEVENT_ERROR_EVENTS 0x2
#define FC_KEVENT_MUST_EXIST 0x20000

struct timer_queue { int fd; uint64_t id; unsigned flags; };
struct native_timer {
  int used, pending, mach;
  mach_port_t port;
  uint64_t deadline, base, mach_flags;
  struct timer_queue queue;
  struct kevent_qos_s event;
};
static struct native_timer native_timers[TIMER_SLOTS];
static pthread_mutex_t timer_lock = PTHREAD_MUTEX_INITIALIZER;
static _Atomic uint64_t timer_changes, timer_wakeups;
static _Atomic uint64_t workloop_timer_changes, workloop_timer_wakeups;

static void timer_error(const char *message) {
  int saved_errno = errno;
  char line[512];
  int length = snprintf(line, sizeof(line), "manual clock: %s (errno=%d, pid=%d, now_ns=%llu)\n",
                        message, saved_errno, getpid(), atomic_load(&advance_ns));
  if (length > 0 && (size_t)length < sizeof(line)) {
    write(STDERR_FILENO, line, (size_t)length);
    if (trace_fd >= 0) write(trace_fd, line, (size_t)length);
  }
  abort();
}

uint64_t fc_timer_changes(void) { return atomic_load(&timer_changes); }
uint64_t fc_timer_wakeups(void) { return atomic_load(&timer_wakeups); }
uint64_t fc_workloop_timer_changes(void) { return atomic_load(&workloop_timer_changes); }
uint64_t fc_workloop_timer_wakeups(void) { return atomic_load(&workloop_timer_wakeups); }

static struct native_timer *new_timer(void) {
  for (int i = 0; i < TIMER_SLOTS; i++) {
    if (!native_timers[i].used) {
      memset(&native_timers[i], 0, sizeof(native_timers[i]));
      native_timers[i].used = 1;
      return &native_timers[i];
    }
  }
  timer_error("timer registry exhausted");
  return NULL;
}

static int timer_due(const struct native_timer *timer) {
  uint64_t delta = atomic_load(&advance_ns);
  return timer->deadline <= timer->base + (timer->base == base_wall_ns ? delta : ns_to_ticks(delta));
}

static void send_kevent(struct timer_queue queue, const struct kevent_qos_s *event) {
  struct kevent_qos_s error = {0};
  int count;
  // ERROR_EVENTS returns only registration failures, never consumes the timer notification.
  if (queue.flags & FC_KEVENT_WORKLOOP) {
    if (!queue.id) timer_error("workloop return without a known queue id is unsupported");
    count = kevent_id(queue.id, event, 1, &error, 1, NULL, NULL,
                      queue.flags | FC_KEVENT_ERROR_EVENTS | FC_KEVENT_MUST_EXIST);
  } else {
    count = kevent_qos(queue.fd, event, 1, &error, 1, NULL, NULL, queue.flags | FC_KEVENT_ERROR_EVENTS);
  }
  if (count < 0 || (count && error.data)) {
    int saved_errno = count < 0 ? errno : 0;
    char line[768];
    int length = snprintf(line, sizeof(line),
      "{\"timer_error\":\"kevent\",\"pid\":%d,\"now_ns\":%llu,\"queue_fd\":%d,"
      "\"queue_id\":%llu,\"queue_flags\":%u,\"ident\":%llu,\"flags\":%u,"
      "\"fflags\":%u,\"data\":%lld,\"udata\":%llu,\"count\":%d,"
      "\"errno\":%d,\"error_flags\":%u,\"error_data\":%lld}\n",
      getpid(), atomic_load(&advance_ns), queue.fd, queue.id, queue.flags,
      event->ident, event->flags, event->fflags, event->data, event->udata,
      count, saved_errno, error.flags, error.data);
    // Fatal diagnostic only. Do not acquire trace_lock while holding timer_lock.
    if (length > 0 && (size_t)length < sizeof(line)) {
      write(STDERR_FILENO, line, (size_t)length);
      if (trace_fd >= 0) write(trace_fd, line, (size_t)length);
    }
    errno = count < 0 ? saved_errno : (int)error.data;
    timer_error("kernel timer registration failed");
  }
}

static void arm_due_timer(struct native_timer *timer) {
  if (!timer->pending || !timer_due(timer)) return;
  if (timer->mach) {
    if (mk_timer_arm_leeway(timer->port, timer->mach_flags, mach_absolute_time(), 0) != KERN_SUCCESS)
      timer_error("Mach timer rearm failed");
  } else {
    struct kevent_qos_s event = timer->event;
    event.flags = (event.flags & ~EV_DISABLE) | EV_ADD | EV_ENABLE;
    event.data = (int64_t)to_real(timer->deadline, timer->base);
    event.ext[1] = 0; // A due virtual timer has no real-time coalescing allowance.
    send_kevent(timer->queue, &event);
    if (timer->queue.flags & FC_KEVENT_WORKLOOP) workloop_timer_wakeups++;
  }
  timer->pending = 0;
  timer_wakeups++;
}

void fc_advance_ns(uint64_t delta) {
  if (!manual) abort();
  pthread_mutex_lock(&timer_lock);
  if (UINT64_MAX - atomic_load(&advance_ns) < delta) timer_error("clock overflow");
  atomic_fetch_add(&advance_ns, delta);
  for (int i = 0; i < TIMER_SLOTS; i++) if (native_timers[i].used) arm_due_timer(&native_timers[i]);
  pthread_mutex_unlock(&timer_lock);
}

static struct native_timer *mach_timer(mach_port_t port) {
  for (int i = 0; i < TIMER_SLOTS; i++)
    if (native_timers[i].used && native_timers[i].mach && native_timers[i].port == port) return &native_timers[i];
  return NULL;
}

static kern_return_t register_mach_timer(mach_port_t port, uint64_t deadline, uint64_t flags) {
  pthread_mutex_lock(&timer_lock);
  struct native_timer *timer = mach_timer(port);
  if (!timer) timer = new_timer();
  *timer = (struct native_timer){.used = 1, .mach = 1, .pending = 1, .port = port,
                               .deadline = deadline, .base = base_abs, .mach_flags = flags};
  kern_return_t result = mk_timer_cancel(port, NULL);
  if (result == KERN_SUCCESS) arm_due_timer(timer);
  else timer->used = 0;
  timer_changes++;
  pthread_mutex_unlock(&timer_lock);
  return result;
}

static kern_return_t remove_mach_timer(mach_port_t port, uint64_t *time, int destroy) {
  pthread_mutex_lock(&timer_lock);
  struct native_timer *timer = mach_timer(port);
  uint64_t kernel_deadline = 0;
  kern_return_t result = destroy ? mk_timer_destroy(port) : mk_timer_cancel(port, time ? &kernel_deadline : NULL);
  if (result == KERN_SUCCESS) {
    // A due timer may still be armed in the kernel. Preserve cancel's returned deadline in
    // virtual units as well as the deadline of a future timer held only in our registry.
    if (time) *time = timer && (timer->pending || kernel_deadline) ? timer->deadline : kernel_deadline;
    if (timer) timer->used = 0;
  }
  timer_changes++;
  pthread_mutex_unlock(&timer_lock);
  return result;
}

static void register_kevent(struct timer_queue queue, const struct kevent_qos_s *event) {
  if (trace_fd >= 0) {
    static _Atomic unsigned registrations;
    if (atomic_fetch_add(&registrations, 1) < 256) {
      char line[512];
      int length = snprintf(line, sizeof(line),
        "{\"kernel_timer\":%llu,\"queue\":%llu,\"fd\":%d,\"queue_flags\":%u,\"flags\":%u,"
        "\"fflags\":%u,\"udata\":%llu,\"deadline\":%lld,\"now_ns\":%llu,"
        "\"base_abs\":%llu,\"base_wall_ns\":%llu,\"numer\":%u,\"denom\":%u}\n",
        event->ident, queue.id, queue.fd, queue.flags, event->flags, event->fflags, event->udata,
        event->data, atomic_load(&advance_ns), base_abs, base_wall_ns, timebase.numer, timebase.denom);
      if (length > 0 && (size_t)length < sizeof(line)) write(trace_fd, line, (size_t)length);
    }
  }
  struct native_timer *timer = NULL;
  for (int i = 0; i < TIMER_SLOTS; i++) {
    struct native_timer *candidate = &native_timers[i];
    if (candidate->used && !candidate->mach && candidate->queue.id == queue.id &&
        candidate->queue.fd == queue.fd && candidate->queue.flags == queue.flags &&
        candidate->event.ident == event->ident && candidate->event.udata == event->udata) {
      timer = candidate;
      break;
    }
  }
  if (event->flags & EV_DELETE) {
    send_kevent(queue, event);
    if (timer) timer->used = 0;
  } else if (event->flags & EV_ADD) {
    if (!(event->fflags & NOTE_ABSOLUTE) || !(event->flags & EV_ONESHOT))
      timer_error("only absolute one-shot kernel timers are supported");
    uint64_t base;
    if (event->fflags & NOTE_MACHTIME)
      base = event->fflags & NOTE_MACH_CONTINUOUS_TIME ? base_cont : base_abs;
    else if (event->fflags & NOTE_NSECONDS) base = base_wall_ns;
    else { timer_error("unsupported kernel timer clock"); return; }
    if (!timer) timer = new_timer();
    *timer = (struct native_timer){.used = 1, .pending = !(event->flags & EV_DISABLE),
                                 .queue = queue, .event = *event, .deadline = (uint64_t)event->data, .base = base};
    struct kevent_qos_s disabled = *event;
    disabled.flags = (disabled.flags & ~EV_ENABLE) | EV_DISABLE;
    // Use a valid real deadline even though the registration is disabled.
    disabled.data = base == base_wall_ns ? (int64_t)(base_wall_ns + ticks_to_ns(mach_absolute_time() - base_abs))
                                        : (int64_t)(base == base_cont ? mach_continuous_time() : mach_absolute_time());
    send_kevent(queue, &disabled);
    arm_due_timer(timer);
  } else {
    timer_error("unsupported timer change flags");
  }
  timer_changes++;
  if (queue.flags & FC_KEVENT_WORKLOOP) workloop_timer_changes++;
}

static int submit_manual_changes(const struct kevent_qos_s *events, int count,
                                 struct kevent_qos_s *other, struct timer_queue queue) {
  if (count < 0 || count > 64) timer_error("oversized kernel change batch");
  int remaining = 0;
  if (queue.flags & FC_KEVENT_WORKQ) queue.fd = -1;
  pthread_mutex_lock(&timer_lock);
  for (int i = 0; i < count; i++) {
    if (events[i].filter == EVFILT_TIMER) register_kevent(queue, &events[i]);
    else other[remaining++] = events[i];
  }
  pthread_mutex_unlock(&timer_lock);
  return remaining;
}
