//! Cassettes (network-rules.md, Cassettes): recorded request–response pairs, one JSON line each,
//! answered again by `replay` rules in recorded order per key.
use crate::canonical::{canonical_of, sha256_bytes_hex};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bundle recording name selected when a request is admitted. Revision zero preserves existing
/// bundles; later installations have independent recordings even when caller names are reused.
pub fn recording_name(revision: u64, name: &str) -> String {
    if revision == 0 { name.to_string() } else { format!("revisions/{revision:04}/{name}") }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub method: String,
    pub url_normalized: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers_selected: BTreeMap<String, String>,
}

impl Key {
    pub fn of(method: &str, url: &str, body: Option<&[u8]>) -> Key {
        Key {
            method: method.to_ascii_uppercase(),
            url_normalized: normalize_url(url),
            body_sha256: body.filter(|b| !b.is_empty()).map(sha256_bytes_hex),
            headers_selected: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Latency {
    pub virtual_ms: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub key: Key,
    pub response: Response,
    pub latency: Latency,
    /// A non-document request was issued while stopped and answered only during explicit progress.
    /// Zero virtual latency does not make that response available between decisions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_progress: bool,
}

impl Line {
    pub fn to_line(&self) -> String {
        canonical_of(self)
    }
}

/// One event of a recorded stream (a stream line of a cassette): which stream (its handshake's key and
/// the session — the how-manyth stream of that key in the recording run), and when, from the instant
/// the page opened it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamEvent {
    pub key: Key,
    pub session: u64,
    /// Microseconds from the instant it opened in the page: its recorded slot, exactly.
    pub at_us: u64,
    /// `open`, `message`, `close`, or `error`.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamLine {
    stream: StreamEvent,
}

impl StreamEvent {
    pub fn to_line(&self) -> String {
        canonical_of(&StreamLine { stream: self.clone() })
    }
}

/// The URL with its query parameters sorted (and no fragment): the same request, however the page
/// ordered them.
pub fn normalize_url(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    match url.split_once('?') {
        Some((base, query)) => {
            // By name only, stably: repeated names keep their order (`a=1&a=2` is not `a=2&a=1`).
            let mut params: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
            params.sort_by_key(|p| p.split('=').next().unwrap_or(""));
            if params.is_empty() { base.to_string() } else { format!("{base}?{}", params.join("&")) }
        }
        None => url.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CassetteError {
    #[error("line {line}: {reason}")]
    Corrupt { line: usize, reason: String },
}

/// The exchange lines of a cassette's text, in order (stream lines aside).
pub fn parse(text: &str) -> Result<Vec<Line>, CassetteError> {
    Ok(parse_all(text)?.0)
}

/// The exchange lines and the stream events of a cassette's text, each in order. A last line without
/// its line feed that does not parse was cut short by a crash while it was written; it is not a line.
pub fn parse_all(text: &str) -> Result<(Vec<Line>, Vec<StreamEvent>), CassetteError> {
    let torn = !text.is_empty() && !text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let (mut exchanges, mut streams) = (Vec::new(), Vec::new());
    for (i, l) in lines.iter().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        if let Ok(s) = serde_json::from_str::<StreamLine>(l) {
            streams.push(s.stream);
            continue;
        }
        match serde_json::from_str(l) {
            Ok(line) => exchanges.push(line),
            Err(_) if torn && i + 1 == lines.len() => {}
            Err(e) => return Err(CassetteError::Corrupt { line: i + 1, reason: e.to_string() }),
        }
    }
    Ok((exchanges, streams))
}

/// Which line answers the next request with `key`: the first of that key not yet `served`, or, past
/// the last, the last again when `repeat_last`.
pub fn next(lines: &[Line], key: &Key, served: &std::collections::BTreeSet<usize>, repeat_last: bool) -> Option<usize> {
    let mut last = None;
    for (i, _) in lines.iter().enumerate().filter(|(_, l)| &l.key == key) {
        if !served.contains(&i) {
            return Some(i);
        }
        last = Some(i);
    }
    if repeat_last { last } else { None }
}

#[cfg(test)]
#[path = "cassette_tests.rs"]
mod tests;
