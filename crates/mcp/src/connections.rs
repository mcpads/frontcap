//! Connections to recorders. Every tool call opens its own connection and presents the agent's lease;
//! one idle lease connection per generation keeps the lease alive exactly as long as this process.
use crate::tools::ToolError;
use frontcap_core::ids::LaunchId;
use frontcap_recorder::launcher::Workspace;
use frontcap_recorder::runtime;
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use frontcap_recorder_protocol::{Call, ErrorCode, ErrorReply, LeaseToken, Reply};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Connections {
    /// The lease each held connection carries, by generation.
    held: Mutex<BTreeMap<LaunchId, (LeaseToken, RecorderClient)>>,
}

pub fn from_reply(e: ErrorReply) -> ToolError {
    let code = serde_json::to_value(e.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "internal".into());
    let recovery = recovery_of(e.code).map_or(e.recovery, str::to_string);
    ToolError::new(code, e.message, recovery)
}

/// The next action of a recorder error, in this server's tool names; `None` keeps the recorder's.
fn recovery_of(code: ErrorCode) -> Option<&'static str> {
    Some(match code {
        ErrorCode::LeaseRequired => "take the lease with session reattach, then send the call again",
        ErrorCode::LeaseHeld => "release it from its controller with session release, or wait for it to expire",
        ErrorCode::Busy => "read status: session cancel its running request, answer its suspension with session answer_dialog, or wait for the request to complete",
        ErrorCode::NoSuchHold => "list held requests with network list_held and answer one of them",
        ErrorCode::StaleRef => "take a fresh observe snapshot and use its references",
        ErrorCode::StaleCursor => "take a fresh observe snapshot and diff against it",
        ErrorCode::ScriptFailed => "fix the expression and send debug evaluate again",
        ErrorCode::Stopped => "launch a new generation; read this one's evidence with debug bundle",
        ErrorCode::NoFrame => "wait {op: advance, by_ms: 16} so a frame is rendered, then capture again",
        ErrorCode::StaleFrame => "wait {op: advance, by_ms: 16} so the new document is rendered, then capture again",
        ErrorCode::PageWorkPending => "let the page finish that work (wait {op: settle} or {op: advance}), then read again",
        _ => return None,
    })
}

pub fn transport_lost(id: &LaunchId, detail: impl std::fmt::Display) -> ToolError {
    ToolError::new("transport_lost", format!("the connection to {id}'s recorder failed: {detail}"), "call session reattach")
}

impl Connections {
    pub fn hold(&self, id: LaunchId, lease: LeaseToken, client: RecorderClient) {
        self.held.lock().unwrap().insert(id, (lease, client));
    }

    pub fn forget(&self, id: &LaunchId) {
        self.held.lock().unwrap().remove(id);
    }

    /// The lease this process holds for `id`, if any.
    pub fn lease_of(&self, id: &LaunchId) -> Option<LeaseToken> {
        self.held.lock().unwrap().get(id).map(|(l, _)| l.clone())
    }

    /// One call on a fresh connection to the generation's recorder.
    pub async fn call(&self, ws: &Workspace, id: &LaunchId, call: Call) -> Result<Reply, ToolError> {
        let dir = ws.runtime_dir(id);
        let record = runtime::load_runtime(&dir)
            .map_err(|e| ToolError::new("internal", e.to_string(), "stop the generation and report the defect"))?
            .ok_or_else(|| ToolError::new("not_found", format!("no generation {id}"), "session list names the generations; launch one if none is yours"))?;
        if let Call::Result { request_id } = &call
            && (record.state == runtime::RuntimeState::Stopped || !record.recorder.is_alive())
        {
            let result = frontcap_recorder::launcher::saved_result(ws, id, request_id).map_err(|e| from_reply(e.0))?;
            return Ok(Reply::Result { request_id: request_id.clone(), result });
        }
        if record.state == runtime::RuntimeState::Stopped {
            return Err(ToolError::new("stopped", format!("generation {id} is stopped"), "read its evidence with debug bundle"));
        }
        let (mut client, hello) = RecorderClient::connect(&record.socket).await.map_err(|e| self.lost(id, e))?;
        if record.launch_id != *id || !matches!(hello, Reply::Hello { launch_id } if launch_id == *id) {
            self.forget(id);
            return Err(ToolError::new("identity_mismatch", "recorder identity differs from the requested generation", "inspect runtime metadata; do not use this connection"));
        }
        client.call(call).await.map_err(|e| match e {
            ClientError::Recorder(reply) => from_reply(reply),
            other => self.lost(id, other),
        })
    }

    fn lost(&self, id: &LaunchId, e: ClientError) -> ToolError {
        self.forget(id);
        transport_lost(id, e)
    }
}

#[cfg(test)]
#[path = "connections_tests.rs"]
mod tests;
