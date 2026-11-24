//! External WKWebView clock transport. This is not the ordinary UIKit clock receipt.
//! The owner must persist request intent and bind the simulator/process before calling.
use crate::request::{ClockRequest, canonical_uuid};
use std::{
    io::{self, Read},
    time::Instant,
};

pub use frontcap_core::native::webview::{Endpoint, Progress, Reply, State, Status};
/// Preserve the entire response, including producers not yet interpreted by this adapter.
#[derive(Debug)]
pub struct Observation {
    pub reply: Reply,
    pub bytes: Vec<u8>,
}

/// One loopback exchange, with one absolute deadline and a bounded response.
/// Every error after sending is potentially applied. Never retries or allocates a new request ID.
pub fn exchange(
    endpoint: &Endpoint,
    request: &ClockRequest,
    document: Option<&str>,
    deadline: Instant,
) -> io::Result<Observation> {
    endpoint.validate(request).map_err(io::Error::other)?;
    if document.is_some_and(|page| !canonical_uuid(page)) {
        return Err(io::Error::other("invalid expected WebView document"));
    }
    let bytes = crate::loopback::exchange(endpoint.port, &serde_json::to_vec(request)?, deadline)?;
    let reply: Reply = serde_json::from_slice(&bytes)?;
    reply
        .validate(endpoint, request, document)
        .map_err(io::Error::other)?;
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "WebView exchange deadline expired",
        ));
    }
    Ok(Observation { reply, bytes })
}

/// Read bounded evidence without granting endpoint or process authority.
pub(crate) fn read_receipt_bytes(path: &std::path::Path) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("WebView receipt is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return Err(io::Error::other("invalid WebView receipt size"));
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "webview_tests.rs"]
mod tests;
