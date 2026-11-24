//! Active host service time. A native interaction may park execution while its owner waits for an
//! agent; that interval must not consume a page-command watchdog or restart its remaining budget.
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Copy)]
struct State {
    elapsed: Duration,
    started: Instant,
    holds: usize,
}

impl State {
    fn elapsed_at(self, now: Instant) -> Duration {
        self.elapsed + if self.holds == 0 { now.duration_since(self.started) } else { Duration::ZERO }
    }
}

/// Clones measure the same active-service timeline, including holds that begin and end between polls.
#[derive(Clone)]
pub struct ServiceClock {
    state: watch::Sender<State>,
}

impl Default for ServiceClock {
    fn default() -> Self {
        Self { state: watch::channel(State { elapsed: Duration::ZERO, started: Instant::now(), holds: 0 }).0 }
    }
}

/// Releases only this hold when dropped. Overlapping owners cannot resume each other's watchdogs.
pub struct ServiceHold {
    clock: ServiceClock,
}

impl Drop for ServiceHold {
    fn drop(&mut self) {
        self.clock.state.send_modify(|state| {
            state.holds -= 1;
            if state.holds == 0 {
                state.started = Instant::now();
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceTimeout;

impl ServiceClock {
    pub fn elapsed(&self) -> Duration {
        self.state.borrow().elapsed_at(Instant::now())
    }

    pub fn park(&self) -> ServiceHold {
        self.state.send_modify(|state| {
            if state.holds == 0 {
                state.elapsed = state.elapsed_at(Instant::now());
            }
            state.holds += 1;
        });
        ServiceHold { clock: self.clone() }
    }

    /// Bounds active time while retaining the original future, including any already-sent native
    /// command. A completed future wins at the deadline, like `tokio::time::timeout`.
    pub async fn timeout<F: Future>(&self, budget: Duration, future: F) -> Result<F::Output, ServiceTimeout> {
        self.timeout_at(self.elapsed().saturating_add(budget), future).await
    }

    /// The deadline is on this clock's timeline. Repeated waits can share one retained budget.
    pub async fn timeout_at<F: Future>(&self, deadline: Duration, future: F) -> Result<F::Output, ServiceTimeout> {
        let mut changes = self.state.subscribe();
        tokio::pin!(future);
        loop {
            let state = *changes.borrow_and_update();
            let remaining = deadline.saturating_sub(state.elapsed_at(Instant::now()));
            tokio::select! {
                biased;
                result = &mut future => return Ok(result),
                _ = changes.changed() => {},
                _ = tokio::time::sleep(remaining), if state.holds == 0 || remaining.is_zero() => {
                    // A hold may have arrived alongside the timer. Reconcile the current cumulative
                    // service time instead of treating the old wall deadline as proof of expiry.
                    if self.elapsed() >= deadline {
                        return Err(ServiceTimeout);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "service_clock_tests.rs"]
mod tests;
