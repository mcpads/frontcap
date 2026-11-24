#ifndef FRONTCAP_DISPATCH_ACTIVITY_H
#define FRONTCAP_DISPATCH_ACTIVITY_H
#include <stdint.h>
#include <dispatch/dispatch.h>

struct fc_dispatch_activity {
  uint64_t submitted;
  uint64_t completed;
  uint64_t pending;
  uint64_t source_cancellations; // Subset of pending: admitted handler retirement.
};

// Activated scope: intercepted async/sync work across activation, mutation work, entered callbacks, explicit deadlines
// and due delayed-dispatch/NSTimer factory registrations with observed membership changes.
int fc_dispatch_tracking_start(void);
void fc_dispatch_activity_read(struct fc_dispatch_activity *out);
// Diagnostic snapshot under the activity mutex; counts are not an idle predicate.
struct fc_dispatch_wait_state {
  uint64_t callers;
  uint64_t native_waiters;
  uint64_t retained_blocks;
  uint64_t native_calls;
  uint64_t native_timeouts;
};
void fc_dispatch_wait_state_read(struct fc_dispatch_wait_state *out);
// A bounded wait on real time; call from the external controller, never the main/observed queue.
// Zero means observed work is idle at this boundary, not that no future producer can submit work.
int fc_dispatch_wait_idle(uint64_t timeout_ns);
// Wait and advance under the submission lock. Failure leaves virtual time unchanged.
// Submissions admitted after this boundary belong to the new virtual instant.
int fc_dispatch_advance_when_idle(uint64_t delta_ns, uint64_t timeout_ns);
// Opt-in relative deadlines. Future work does not block idle; due work is admitted
// atomically with advance, before it can enter its queue. No repetition.
// Completion includes the callback's local autorelease pool and owned capture release.
int fc_dispatch_after_ns(uint64_t delay_ns, dispatch_queue_t queue, dispatch_block_t block);
// A successful schedule writes a nonzero token; failures leave the output unchanged.
int fc_dispatch_schedule_ns(uint64_t delay_ns, dispatch_queue_t queue, dispatch_block_t block,
                            uint64_t *token);
// Cancels future or admitted-but-not-entered work and releases its completion obligation.
// EBUSY: callback already entered; ENOENT: unknown, finished or already cancelled token.
// Successful cancellation guarantees the callback will not run; it does not resume its queue.
// Capture destruction stays pending through return and any observed async descendants.
// Reentrant idle/advance from that destruction returns EDEADLK.
int fc_dispatch_cancel(uint64_t token);
#endif
