//! One typed Flutter HTTP exchange using the existing bounded bridge process owner.
//! Target acquisition and response/raster publication belong to the live session owner.
use crate::{
    flutter_store::RequestStore,
    process::{Exchange, exchange_typed_recorded_with_budget},
    store::StoreError,
};
use frontcap_core::{
    ids::RequestId,
    native::flutter::{Request, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    endpoint: Endpoint,
    request: Request,
    timeout_ms: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub http_status: u16,
    pub response: Response,
}
impl Reply {
    pub fn validate(&self, request: &Request) -> io::Result<()> {
        match (&self.response, self.http_status) {
            (Response::Completed(result), 200)
                if result.generation == request.generation && result.id == request.id =>
            {
                Ok(())
            }
            (Response::Pending { id, generation }, 200)
                if id == &request.id && generation == &request.generation =>
            {
                Ok(())
            }
            (Response::Rejected { .. }, 400 | 403 | 409) => Ok(()),
            _ => Err(io::Error::other(
                "Flutter HTTP response identity or status differs",
            )),
        }
    }
}
/// Persist dispatch before process launch. Any transport/storage ambiguity stays dispatched.
/// This function never retries or completes the store; the owner validates artifacts and calls
/// complete separately. A Reconcile binding cannot enter this function a second time.
pub fn dispatch(
    store: &mut RequestStore,
    id: &RequestId,
    command: &mut Command,
    endpoint: Endpoint,
    deadline: Instant,
    cancelled: &AtomicBool,
    evidence: &Path,
) -> Result<Exchange<Reply>, StoreError> {
    if endpoint.port == 0
        || endpoint.token.len() != 48
        || !endpoint
            .token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(io::Error::other("invalid Flutter loopback endpoint").into());
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(io::Error::other("Flutter exchange cancelled or deadline expired").into());
    }
    let timeout_ms = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
    .clamp(1, 60000);
    let request = store.dispatch(id)?;
    let call = Call {
        endpoint,
        request,
        timeout_ms,
    };
    let mut exchange: Exchange<Reply> = exchange_typed_recorded_with_budget(
        command,
        &call,
        deadline,
        cancelled,
        RESPONSE_LIMIT + 65536,
        evidence,
        store.write_budget(),
    )?;
    if let Some(reply) = &exchange.reply
        && let Err(error) = reply.validate(&call.request) {
            exchange.failure = Some(error.to_string());
            exchange.reply = None;
            crate::process::save_exchange_with_budget(&evidence.join("exchange.json"), &exchange, store.write_budget())?;
        }
    Ok(exchange)
}

#[cfg(test)]
#[path = "flutter_bridge_tests.rs"]
mod tests;
