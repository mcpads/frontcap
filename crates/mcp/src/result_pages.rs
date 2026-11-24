//! Immutable local readbacks of oversized tool results. Reading never contacts the target.
use crate::tools::ToolError;
use frontcap_core::{canonical::sha256_hex, ids::LaunchId, state::save_atomic_bytes};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

/// Serialized structured payload; MCP also carries its text copy.
pub const INLINE_BYTES: usize = 16 * 1024;
const CHUNK_BYTES: usize = 8 * 1024;

#[derive(Serialize)]
struct ResultPage<'a> {
    result_id: &'a str,
    pointer: &'a str,
    encoding: &'static str,
    offset: usize,
    total_bytes: usize,
    omitted_bytes: usize,
    next_offset: Option<usize>,
    json_fragment: &'a str,
    next_action: &'static str,
}

fn unavailable() -> ToolError {
    ToolError::new("result_unavailable", "retained result is missing, unreadable or corrupt", "inspect local workspace storage; do not repeat a mutation with a new request_id")
}

fn invalid(message: &str) -> ToolError {
    ToolError::new("invalid_arguments", message, "use the returned result_id, JSON pointer and next_offset with debug result")
}

fn page(id: &str, pointer: &str, text: &str, offset: usize) -> Result<Value, ToolError> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err(invalid("offset must be a UTF-8 byte boundary within the selected JSON"));
    }
    let mut end = offset.saturating_add(CHUNK_BYTES).min(text.len());
    while !text.is_char_boundary(end) { end -= 1; }
    Ok(serde_json::to_value(ResultPage {
        result_id: id, pointer, encoding: "json_utf8", offset,
        total_bytes: text.len(), omitted_bytes: text.len() - (end - offset),
        next_offset: (end < text.len()).then_some(end), json_fragment: &text[offset..end],
        next_action: "debug result with this result_id and pointer; use next_offset to continue, or a JSON pointer to select retained data; concatenate fragments before parsing JSON",
    }).expect("result page serializes"))
}

pub fn bound(root: &Path, launch: &LaunchId, value: Value) -> Result<Value, ToolError> {
    let text = serde_json::to_string(&value).expect("tool result serializes");
    if text.len() <= INLINE_BYTES { return Ok(value); }
    let id = sha256_hex(&text);
    let dir = root.join("results").join(launch.as_str());
    std::fs::create_dir_all(&dir).map_err(|_| unavailable())?;
    save_atomic_bytes(&dir.join(format!("{id}.json")), text.as_bytes()).map_err(|_| unavailable())?;
    page(&id, "", &text, 0)
}

/// Preserve MCP failure and the original error code while retaining oversized diagnostic details.
pub fn error(root: &Path, launch: &LaunchId, error: ToolError) -> crate::server::CallToolResult {
    let value = serde_json::to_value(&error).expect("tool error serializes");
    match bound(root, launch, value) {
        Ok(mut value) => {
            if value.get("result_id").is_some() {
                value["code"] = Value::String(error.code);
                value["message"] = Value::String("error details retained in result pages".into());
                value["recovery"] = Value::String("read debug result with this result_id for the original message and recovery".into());
            }
            crate::server::CallToolResult::error(&value)
        }
        Err(error) => error.result(),
    }
}

pub fn read(root: &Path, launch: &LaunchId, id: &str, pointer: &str, offset: usize) -> Result<Value, ToolError> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(invalid("result_id must be the returned SHA-256 identifier"));
    }
    if pointer.len() > 1024 || (!pointer.is_empty() && !pointer.starts_with('/')) {
        return Err(invalid("pointer must be empty or a JSON pointer of at most 1024 bytes"));
    }
    let text = std::fs::read_to_string(root.join("results").join(launch.as_str()).join(format!("{id}.json"))).map_err(|_| unavailable())?;
    if sha256_hex(&text) != id { return Err(unavailable()); }
    if pointer.is_empty() { return page(id, pointer, &text, offset); }
    let value: Value = serde_json::from_str(&text).map_err(|_| unavailable())?;
    let selected = value.pointer(pointer).ok_or_else(|| invalid("JSON pointer does not select a retained value"))?;
    page(id, pointer, &serde_json::to_string(selected).expect("selected value serializes"), offset)
}

#[cfg(test)]
#[path = "result_pages_tests.rs"]
mod tests;
