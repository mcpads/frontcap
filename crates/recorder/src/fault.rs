//! Fault points for recovery tests. With the `fault-injection` feature, `FRONTCAP_FAULT=<request_id>@<point>`
//! aborts the recorder at that point, which the target observes exactly as a killed process. Points:
//! `before_dispatch` (intent written), `after_effect` (effect applied, completion not written),
//! `after_complete` (completion written, answer not sent), `after_dispatch` (an act dispatched, nothing done),
//! `after_clock` (a clock request reached its boundary, before its answer is saved),
//! `after_substep_<n>`, `after_engine` (a recorder-state request's engine effect done, its answer not saved), `after_action` (an act's action done, its settle not begun), `settle_step_<n>` (after a
//! settle's n-th step, its progress saved). State interventions add `state_before_write_<n>`,
//! `state_after_write_<n>`, `state_after_receipt_<n>`, and `state_saved_<n>`; these distinguish native
//! setter completion, browser-side readback and recorder-side persistence.
//! Predicate waits add `before_watch_install`, `after_watch_install`, `before_watch_cleanup`
//! (terminal result saved), `after_watch_remove`, and `after_watch_cleanup` (ownership cleared).
//! Termination-only stop adds `terminate_before_dispatch`, `terminate_after_dispatch`,
//! `terminate_after_exit`, and `terminate_after_complete` in the launcher process.
//! Native requests add `native_before_dispatch`, `native_after_dispatch`, `native_after_effect`,
//! and `native_after_complete` at the corresponding durable admission/result boundaries.
//! Offline finalization adds `finalize_after_<snapshot filename>`, `finalize_before_manifest`,
//! `finalize_after_manifest`, and `finalize_after_status`, scoped to the launch id.
#[cfg(feature = "fault-injection")]
pub fn point(request_id: &str, point: &str) {
    if let Ok(spec) = std::env::var("FRONTCAP_FAULT")
        && spec == format!("{request_id}@{point}")
    {
        std::process::abort();
    }
}

#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub fn point(_request_id: &str, _point: &str) {}
