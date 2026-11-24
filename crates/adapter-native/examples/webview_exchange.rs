//! One opt-in external WebView exchange. The caller owns durable IDs and target lifetime.
use frontcap_native::{
    request::ClockRequest,
    webview::{Endpoint, exchange},
};
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    endpoint: Endpoint,
    request: ClockRequest,
    document: Option<String>,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    io::stdin().take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err("input too large".into());
    }
    let input: Input = serde_json::from_slice(&bytes)?;
    let result = exchange(
        &input.endpoint,
        &input.request,
        input.document.as_deref(),
        Instant::now() + Duration::from_secs(10),
    )?;
    io::stdout().write_all(&result.bytes)?;
    Ok(())
}
