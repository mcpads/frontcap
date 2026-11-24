//! Durable lifecycle intent. Reopening evidence never grants ownership or permission to resend.
use crate::store::{StoreError, lock};
pub use frontcap_core::native::webview_lifecycle::{
    Arguments, Operation, Owner, Reply, Request, Status,
};
use frontcap_core::{
    ids::RequestId,
    native::request::InvalidRequest,
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recovery {
    Prepared { request: Request },
    Reconcile { request: Request },
    Completed { reply: Box<Reply> },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    request: Request,
    recovery: Recovery,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    owner: Owner,
    entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_sha256: Option<String>,
}
impl State {
    fn validate(&self) -> Result<bool, InvalidRequest> {
        self.owner.validate()?;
        if self.budget_sha256.as_ref().is_some_and(|value| value.len()!=64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) {
            return Err(InvalidRequest("invalid WebView allowance fingerprint"));
        }
        if self.entries.len() > 4096 {
            return Err(InvalidRequest("lifecycle request capacity exceeded"));
        }
        let mut previous = None;
        let mut pending = false;
        let mut closed = false;
        let mut ids = HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            entry.request.validate(&self.owner)?;
            let allowed = matches!(
                (previous, entry.request.operation),
                (None, Operation::Launch)
                    | (
                        Some(Operation::Launch | Operation::Foreground),
                        Operation::Background
                    )
                    | (Some(Operation::Background), Operation::Foreground)
            );
            if pending
                || closed
                || !allowed
                || entry.request.request_id != index as u64 + 1
                || !ids.insert(&entry.id)
            {
                return Err(InvalidRequest("invalid lifecycle request history"));
            }
            match &entry.recovery {
                Recovery::Prepared { request } | Recovery::Reconcile { request } => {
                    if request != &entry.request {
                        return Err(InvalidRequest("lifecycle binding differs"));
                    }
                    pending = true;
                }
                Recovery::Completed { reply } => {
                    reply.validate_completion(&self.owner, &entry.request)?;
                    closed = reply.status == Status::Rejected;
                    previous = Some(entry.request.operation);
                }
            }
        }
        Ok(pending || closed)
    }
}
pub struct RequestStore {
    path: PathBuf,
    state: State,
    _held: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}
impl RequestStore {
    pub fn create(directory: &Path, owner: Owner) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(directory, owner, budget.as_ref())
    }
    fn create_with_budget(directory: &Path, owner: Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let state = State {
            owner,
            budget_sha256: budget.map(WriteBudget::binding_sha256).transpose()?,
            entries: vec![],
        };
        state.validate()?;
        std::fs::create_dir(directory)?;
        let held = lock(directory)?;
        let path = directory.join("requests.json");
        match budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &path, &state)?,
            None => save_atomic(&path, &state)?,
        }
        Ok(Self {
            path,
            state,
            _held: held,
            poisoned: false,
            budget: budget.cloned(),
        })
    }
    pub fn open(directory: &Path, owner: &Owner) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::open_with_budget(directory, owner, budget.as_ref())
    }
    fn open_with_budget(directory: &Path, owner: &Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let held = lock(directory)?;
        let path = directory.join("requests.json");
        let mut state: State = load(&path)?.ok_or(StoreError::Unknown)?;
        if &state.owner != owner {
            return Err(StoreError::IdentityChanged);
        }
        state.validate()?;
        let binding = budget.map(WriteBudget::binding_sha256).transpose()?;
        if state.budget_sha256.is_some() && state.budget_sha256 != binding {
            return Err(std::io::Error::other("WebView write allowance differs").into());
        }
        if state.budget_sha256.is_none() { state.budget_sha256 = binding; }
        Ok(Self {
            path,
            state,
            _held: held,
            poisoned: false,
            budget: budget.cloned(),
        })
    }
    fn save_receipt(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        match &self.budget {
            Some(budget) => budget.save_atomic_bytes(Class::Ordinary, path, bytes),
            None => save_atomic_bytes(path, bytes),
        }
    }
    fn writable(&self) -> Result<(), StoreError> {
        if self.poisoned {
            Err(StoreError::StorageUncertain)
        } else {
            Ok(())
        }
    }
    fn publish(&mut self, state: State) -> Result<(), StoreError> {
        self.writable()?;
        state.validate()?;
        let bytes = frontcap_core::canonical::canonical_of(&state);
        if let Some(budget) = &self.budget { budget.charge(Class::Ordinary, bytes.len() as u64)?; }
        self.poisoned = true;
        save_atomic_bytes(&self.path, bytes.as_bytes())?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    /// Native foreground evidence only; this does not imply page resume delivery.
    pub fn is_foreground(&self) -> bool {
        !self.poisoned
            && self.state.entries.last().is_some_and(|entry| {
                matches!(&entry.recovery, Recovery::Completed { reply }
                if reply.status == Status::Ok && matches!(reply.request.operation,
                    Operation::Launch | Operation::Foreground))
            })
    }
    /// Inspect retained journal state without sending, recovering or advancing anything.
    pub fn inspect(&self) -> frontcap_core::native::webview_progress::Journal<Request, Reply> {
        use frontcap_core::native::webview_progress::{Completed, Dispatch, Journal, Pending};
        let last_completed = self.state.entries.iter().rev().find_map(|entry| match &entry.recovery {
            Recovery::Completed { reply } => Some(Completed { id: entry.id.clone(),
                native_request_id: entry.request.request_id, request: entry.request.clone(), reply: *reply.clone() }),
            _ => None,
        });
        let pending = self.state.entries.iter().rev().find_map(|entry| {
            let dispatch = match entry.recovery {
                Recovery::Prepared { .. } => Dispatch::Prepared,
                Recovery::Reconcile { .. } => Dispatch::Unresolved,
                Recovery::Completed { .. } => return None,
            };
            Some(Pending { id: entry.id.clone(), native_request_id: entry.request.request_id,
                request: entry.request.clone(), dispatch })
        });
        Journal { storage_uncertain: self.poisoned, last_completed, pending }
    }
    pub fn recovery(&self, id: &RequestId) -> Result<Recovery, StoreError> {
        self.writable()?;
        self.state
            .entries
            .iter()
            .find(|e| &e.id == id)
            .map(|e| e.recovery.clone())
            .ok_or(StoreError::Unknown)
    }
    pub fn prepare(&mut self, id: RequestId, operation: Operation) -> Result<Recovery, StoreError> {
        self.writable()?;
        if let Some(entry) = self.state.entries.iter().find(|e| e.id == id) {
            if entry.request.operation != operation {
                return Err(StoreError::Conflict);
            }
            return Ok(entry.recovery.clone());
        }
        if self.state.validate()? {
            return Err(StoreError::Pending);
        }
        let request = Request {
            generation: self.state.owner.runner.generation.clone(),
            request_id: self.state.entries.len() as u64 + 1,
            operation,
            arguments: Arguments {},
        };
        let recovery = Recovery::Prepared {
            request: request.clone(),
        };
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id,
            request,
            recovery: recovery.clone(),
        });
        self.publish(state)?;
        Ok(recovery)
    }
    /// Publish before any socket write. An observation timeout never authorizes redispatch.
    pub fn dispatch(&mut self, id: &RequestId) -> Result<Request, StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        if !matches!(entry.recovery, Recovery::Prepared { .. }) {
            return Err(StoreError::AlreadyDispatched);
        }
        let request = entry.request.clone();
        entry.recovery = Recovery::Reconcile {
            request: request.clone(),
        };
        self.publish(state)?;
        Ok(request)
    }
    pub fn complete(&mut self, id: &RequestId, reply: Reply) -> Result<(), StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        match &entry.recovery {
            Recovery::Prepared { .. } => return Err(StoreError::NotDispatched),
            Recovery::Completed { reply: old } => {
                return if **old == reply {
                    Ok(())
                } else {
                    Err(StoreError::Conflict)
                };
            }
            Recovery::Reconcile { .. } => (),
        }
        entry.recovery = Recovery::Completed {
            reply: Box::new(reply),
        };
        self.publish(state)
    }
    /// Caller holds the live target lease. Persist dispatch before a single socket exchange.
    /// Unknown effects require original-receipt reconciliation, never retransmission.
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: std::time::Instant,
    ) -> Result<Reply, StoreError> {
        match self.prepare(id.clone(), operation)? {
            Recovery::Completed { reply } => return Ok(*reply),
            Recovery::Reconcile { .. } => return Err(StoreError::AlreadyDispatched),
            Recovery::Prepared { .. } => (),
        }
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "lifecycle dispatch deadline expired",
            )
            .into());
        }
        if self.state.owner.app.process.has_exited()?
            || self.state.owner.runner_process.has_exited()?
        {
            return Err(StoreError::IdentityChanged);
        }
        let request = self.dispatch(&id)?;
        let bytes = crate::loopback::exchange(
            self.state.owner.port,
            &serde_json::to_vec(&request).map_err(std::io::Error::other)?,
            deadline,
        )?;
        let reply: Reply = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        reply.validate_completion(&self.state.owner, &request)?;
        let path = self
            .path
            .parent()
            .unwrap()
            .join(format!("receipt-{}.json", request.request_id));
        self.save_receipt(&path, &bytes)?;
        self.complete(&id, reply.clone())?;
        Ok(reply)
    }
    /// Read only the owned runner's original receipt file; never sends a lifecycle command.
    pub fn reconcile(&mut self, id: &RequestId, receipt_path: &Path) -> Result<Reply, StoreError> {
        match self.recovery(id)? {
            Recovery::Completed { reply } => return Ok(*reply),
            Recovery::Prepared { .. } => return Err(StoreError::NotDispatched),
            Recovery::Reconcile { .. } => (),
        }
        let bytes = crate::webview::read_receipt_bytes(receipt_path)?;
        let reply: Reply = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        // Validate before writing retained raw evidence or completing the request.
        let entry = self
            .state
            .entries
            .iter()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        reply.validate_completion(&self.state.owner, &entry.request)?;
        let path = self
            .path
            .parent()
            .unwrap()
            .join(format!("receipt-{}.json", entry.request.request_id));
        self.save_receipt(&path, &bytes)?;
        self.complete(id, reply.clone())?;
        Ok(reply)
    }
}
#[cfg(test)]
#[path = "webview_lifecycle_store_tests.rs"]
mod tests;
