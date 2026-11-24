//! Published behavior history, read without recovering, truncating or contacting the generation.
use crate::tools::ToolError;
use frontcap_core::{canonical::sha256_bytes_hex,event::BehaviorEvent};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
pub(crate) struct History {
    pub events: Vec<BehaviorEvent>,
    pub complete_prefix_sha256: String,
    pub incomplete_tail: bool,
    pub last_seq: Option<u64>,
    pub next_after: Option<u64>,
}

pub(crate) fn read(bundle: &Path) -> Result<History,ToolError> {
    let invalid=|message:String| ToolError::new("invalid_history",message,"inspect trace.jsonl; history reads do not repair evidence");
    let path=bundle.join("trace.jsonl");
    let stat=std::fs::symlink_metadata(&path).map_err(|e|invalid(e.to_string()))?;
    if !stat.is_file() { return Err(invalid("trace.jsonl must be a regular file".into())); }
    if stat.len()>64*1024*1024 { return Err(ToolError::new("result_too_large","history exceeds the 64 MiB scan bound","inspect the bundle externally")); }
    let bytes=std::fs::read(&path).map_err(|e|invalid(e.to_string()))?;
    let cut=bytes.iter().rposition(|b|*b==b'\n').map(|n|n+1).unwrap_or(0);
    let mut events:Vec<BehaviorEvent>=Vec::new();
    for line in bytes[..cut].split_inclusive(|b|*b==b'\n') {
        let event:BehaviorEvent=serde_json::from_slice(line).map_err(|e|invalid(e.to_string()))?;
        if event.seq != events.len() as u64 || events.last().is_some_and(|last|event.t_us<last.t_us) {
            return Err(invalid("event sequence is not contiguous from zero or virtual time moves backward".into()));
        }
        events.push(event);
    }
    let last_seq=events.last().map(|e|e.seq);
    Ok(History {events,complete_prefix_sha256:sha256_bytes_hex(&bytes[..cut]),incomplete_tail:cut!=bytes.len(),last_seq,next_after:None})
}

pub(crate) fn page(bundle:&Path,after:Option<u64>,limit:Option<usize>) -> Result<History,ToolError> {
    let limit=limit.unwrap_or(50);
    if !(1..=100).contains(&limit) { return Err(ToolError::new("invalid_arguments","limit must be 1–100","use after_seq for the next page")); }
    let mut history=read(bundle)?;
    history.events.retain(|e|after.is_none_or(|a|e.seq>a));
    if history.events.len()>limit {
        history.events.truncate(limit);
        history.next_after=history.events.last().map(|e|e.seq);
    }
    Ok(history)
}

#[cfg(test)]
#[path="history_tests.rs"]
mod tests;
