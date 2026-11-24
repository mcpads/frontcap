//! The MCP lifecycle over stdio: `initialize` (version negotiation), `ping`, `tools/list`, `tools/call`.
//! Notifications (initialized, cancelled) need no answer; a cancelled request still runs to completion,
//! as every recorder request does.
use crate::jsonrpc::{self, ErrorObject, RawValue};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Newest first; `initialize` answers with the client's version when it is listed.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = CallToolResult> + Send + 'a>>;

pub trait Tools: Send + Sync {
    fn list(&self) -> Vec<ToolDescription>;
    fn call(&self, name: &str, arguments: Option<Box<RawValue>>) -> ToolFuture<'_>;
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescription {
    pub name: String,
    pub description: String,
    pub input_schema: schemars::Schema,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text { text: String },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: &'static str,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<Content>,
    pub structured_content: Box<RawValue>,
    pub is_error: bool,
}

impl CallToolResult {
    fn of(value: &impl Serialize, is_error: bool) -> CallToolResult {
        let text = serde_json::to_string(value).expect("tool results serialize");
        CallToolResult {
            content: vec![Content::Text { text: text.clone() }],
            structured_content: RawValue::from_string(text).expect("serialized JSON is valid"),
            is_error,
        }
    }

    pub fn structured(value: &impl Serialize) -> CallToolResult {
        CallToolResult::of(value, false)
    }

    pub fn error(value: &impl Serialize) -> CallToolResult {
        CallToolResult::of(value, true)
    }

    /// A result with a PNG image after its text.
    pub fn with_png(value: &impl Serialize, png: &[u8]) -> CallToolResult {
        let mut r = CallToolResult::of(value, false);
        r.content.push(Content::Image { data: frontcap_core::encode::base64(png), mime_type: "image/png" });
        r
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct InitializeParams {
    protocol_version: String,
    #[serde(default)]
    capabilities: Option<Box<RawValue>>,
    #[serde(default)]
    client_info: Option<Box<RawValue>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult<'a> {
    protocol_version: &'a str,
    capabilities: ServerCapabilities,
    server_info: Implementation,
    instructions: &'static str,
}

#[derive(Serialize)]
struct ServerCapabilities {
    tools: ToolsCapability,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolsCapability {
    list_changed: bool,
}

#[derive(Serialize)]
struct Implementation {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct ListToolsResult {
    tools: Vec<ToolDescription>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallToolParams {
    name: String,
    #[serde(default)]
    arguments: Option<Box<RawValue>>,
    #[serde(default, rename = "_meta")]
    #[allow(dead_code)]
    meta: Option<Box<RawValue>>,
}

#[derive(Serialize)]
struct Empty {}

const INSTRUCTIONS: &str = include_str!("server_instructions.txt");

pub struct Server<T: Tools> {
    tools: T,
    initialized: AtomicBool,
    /// The listed tools' names. The list never changes (`listChanged` is false), and building it
    /// generates every tool's schema.
    names: std::sync::OnceLock<Vec<String>>,
}

fn raw(value: &impl Serialize) -> Box<RawValue> {
    RawValue::from_string(serde_json::to_string(value).expect("results serialize")).expect("serialized JSON is valid")
}

impl<T: Tools> Server<T> {
    pub fn new(tools: T) -> Server<T> {
        Server { tools, initialized: AtomicBool::new(false), names: std::sync::OnceLock::new() }
    }

    /// Answers one line; `None` for a notification.
    pub async fn handle(&self, line: &str) -> Option<String> {
        let message = match jsonrpc::parse_line(line) {
            Ok(m) => m,
            Err((id, e)) => return Some(jsonrpc::error_line(id, e)),
        };
        let id = match message.id? {
            jsonrpc::IdField::Id(id) => id,
            jsonrpc::IdField::Null(()) => unreachable!("parse_line rejects null ids"),
        };
        Some(match self.answer(&message.method, message.params).await {
            Ok(result) => jsonrpc::result_line(id, &result),
            Err(e) => jsonrpc::error_line(Some(id), e),
        })
    }

    async fn answer(&self, method: &str, params: Option<Box<RawValue>>) -> Result<Box<RawValue>, ErrorObject> {
        let decode = |p: Option<Box<RawValue>>| -> Result<Box<RawValue>, ErrorObject> {
            p.ok_or_else(|| ErrorObject::new(jsonrpc::INVALID_PARAMS, format!("{method} needs params")))
        };
        match method {
            "ping" => Ok(raw(&Empty {})),
            "initialize" => {
                let p: InitializeParams = serde_json::from_str(decode(params)?.get())
                    .map_err(|e| ErrorObject::new(jsonrpc::INVALID_PARAMS, e.to_string()))?;
                let version = SUPPORTED_VERSIONS.iter().find(|v| **v == p.protocol_version).copied().unwrap_or(SUPPORTED_VERSIONS[0]);
                self.initialized.store(true, Ordering::SeqCst);
                Ok(raw(&InitializeResult {
                    protocol_version: version,
                    capabilities: ServerCapabilities { tools: ToolsCapability { list_changed: false } },
                    server_info: Implementation { name: "frontcap", version: env!("CARGO_PKG_VERSION") },
                    instructions: INSTRUCTIONS,
                }))
            }
            _ if !self.initialized.load(Ordering::SeqCst) => Err(ErrorObject::new(jsonrpc::NOT_INITIALIZED, "call initialize first")),
            "tools/list" => Ok(raw(&ListToolsResult { tools: self.tools.list() })),
            "tools/call" => {
                let p: CallToolParams = serde_json::from_str(decode(params)?.get())
                    .map_err(|e| ErrorObject::new(jsonrpc::INVALID_PARAMS, e.to_string()))?;
                if !self.names.get_or_init(|| self.tools.list().into_iter().map(|t| t.name).collect()).contains(&p.name) {
                    return Err(ErrorObject::new(jsonrpc::INVALID_PARAMS, format!("unknown tool {}", p.name)));
                }
                Ok(raw(&self.tools.call(&p.name, p.arguments).await))
            }
            other => Err(ErrorObject::new(jsonrpc::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}

/// Serves MCP on stdin and stdout until stdin closes. Each request is answered in its own task, so a
/// long clock call never delays `status`; lines are written by one writer and never interleave.
pub async fn serve_stdio<T: Tools + 'static>(tools: T) -> std::io::Result<()> {
    let server = Arc::new(Server::new(tools));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut tasks = Vec::new();
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if input.read_until(b'\n', &mut bytes).await? == 0 {
            break;
        }
        // A line that is not UTF-8 is a malformed message, answered like any other.
        let line = String::from_utf8_lossy(&bytes).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let server = server.clone();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            if let Some(answer) = server.handle(&line).await {
                let _ = tx.send(answer);
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}


#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
