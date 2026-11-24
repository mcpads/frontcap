//! JSON-RPC 2.0 envelopes for MCP's stdio transport: one message per line. Parameters and results stay
//! opaque (`RawValue`) here and are decoded into typed structs by the method that owns them.
use serde::{Deserialize, Serialize};
pub use serde_json::value::RawValue;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Version {
    #[serde(rename = "2.0")]
    V2,
}

/// An `id` member as sent: MCP forbids `null`, which is told apart from an absent id (a notification).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum IdField {
    Id(Id),
    Null(()),
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<IdField>, D::Error> {
    IdField::deserialize(d).map(Some)
}

/// A request (with an id) or a notification (without one).
#[derive(Debug, Deserialize)]
pub struct Incoming {
    pub jsonrpc: Version,
    /// Absent for a notification; present (possibly `null`) for a request.
    #[serde(default, deserialize_with = "present")]
    pub id: Option<IdField>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Box<RawValue>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
}

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// MCP: a request other than `ping` before `initialize`.
pub const NOT_INITIALIZED: i64 = -32002;

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> ErrorObject {
        ErrorObject { code, message: message.into() }
    }
}

#[derive(Debug, Serialize)]
pub struct Response<'a> {
    pub jsonrpc: Version,
    /// `null` when the request's id could not be read.
    pub id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<&'a RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

/// Parses one line. An error carries the id when it could still be read.
pub fn parse_line(line: &str) -> Result<Incoming, (Option<Id>, ErrorObject)> {
    #[derive(Deserialize)]
    struct IdOnly {
        #[serde(default)]
        id: Option<Id>,
    }
    match serde_json::from_str::<Incoming>(line) {
        Ok(Incoming { id: Some(IdField::Null(())), .. }) => Err((None, ErrorObject::new(INVALID_REQUEST, "a request id must not be null"))),
        Ok(m) => Ok(m),
        Err(e) => match serde_json::from_str::<IdOnly>(line) {
            Ok(partial) => Err((partial.id, ErrorObject::new(INVALID_REQUEST, e.to_string()))),
            // Valid JSON that is not a request object (a batch, a number) is an invalid request.
            Err(_) if serde_json::from_str::<serde::de::IgnoredAny>(line).is_ok() => Err((None, ErrorObject::new(INVALID_REQUEST, e.to_string()))),
            Err(_) => Err((None, ErrorObject::new(PARSE_ERROR, e.to_string()))),
        },
    }
}

pub fn result_line(id: Id, result: &RawValue) -> String {
    serde_json::to_string(&Response { jsonrpc: Version::V2, id: Some(id), result: Some(result), error: None }).expect("responses serialize")
}

pub fn error_line(id: Option<Id>, error: ErrorObject) -> String {
    serde_json::to_string(&Response { jsonrpc: Version::V2, id, result: None, error: Some(error) }).expect("responses serialize")
}
