//! Bounded CDP timer policy effects. Budget acknowledgement is not budget completion.
use frontcap_core::state::save_atomic;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
pub use frontcap_core::native::android_timers::{Action, Completion};
pub(crate) use frontcap_core::native::android_timers::State;
#[derive(Serialize)]
struct Policy {
    policy: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<u32>,
}
impl cdp::Command for Policy {
    const METHOD: &'static str = "Emulation.setVirtualTimePolicy";
    type Returns = serde_json::Value;
}
#[derive(Deserialize, Serialize)]
struct Expired {}
impl cdp::Event for Expired {
    const NAME: &'static str = "Emulation.virtualTimeBudgetExpired";
}
pub(crate) async fn execute(
    client: &cdp::Client,
    action: &Action,
    evidence: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<(f64, bool)> {
    action.validate()?;
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(io::Error::other("timer effect budget exhausted"));
    }
    // Subscribe before dispatch: completion may precede the command acknowledgement.
    let mut events = client.subscribe::<Expired>(None);
    let policy = match action {
        Action::Pause => Policy {
            policy: "pause",
            budget: None,
        },
        Action::Advance { milliseconds } => Policy {
            policy: "advance",
            budget: Some(*milliseconds),
        },
    };
    save_atomic(&evidence.join("policy-intent.json"), &policy)?;
    let cancelled_future = async {
        while !cancelled.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let work = async {
        let reply = client.call(None, &policy).await.map_err(io::Error::other)?;
        save_atomic(&evidence.join("policy-reply.json"), &reply)?;
        let ticks = reply["virtualTimeTicksBase"]
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| io::Error::other("invalid virtual time base"))?;
        let expired = matches!(action, Action::Advance { .. });
        if expired {
            let event = events.next().await.map_err(io::Error::other)?;
            save_atomic(&evidence.join("budget-expired.json"), &event)?;
        }
        Ok((ticks, expired))
    };
    tokio::select! {
        result=tokio::time::timeout_at(deadline.into(),work)=>result.map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"timer policy completion unknown"))?,
        _=cancelled_future=>Err(io::Error::new(io::ErrorKind::Interrupted,"timer policy cancelled; outcome unknown")),
    }
}

#[cfg(test)]
#[path = "android_timers_tests.rs"]
mod tests;
