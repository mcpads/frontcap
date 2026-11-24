//! Durable external WebView requests. Opening evidence does not acquire a simulator or app.
use crate::{
    request::{ClockOperation, ClockRequest, InvalidRequest},
    store::{StoreError, lock},
    webview::{Reply, Status, exchange},
};
use frontcap_core::{
    ids::RequestId,
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Instant,
};

pub use frontcap_core::native::webview::Owner;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recovery {
    Prepared { request: ClockRequest },
    Reconcile { request: ClockRequest },
    Completed { reply: Box<Reply> },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    request: ClockRequest,
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
#[derive(Default)]
struct Cursor {
    document: Option<String>,
    now: u64,
    pending: bool,
    fenced: bool,
}
impl State {
    fn validate(&self) -> Result<Cursor, InvalidRequest> {
        self.owner.validate()?;
        if self.budget_sha256.as_ref().is_some_and(|value| value.len()!=64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) {
            return Err(InvalidRequest("invalid WebView allowance fingerprint"));
        }
        let mut cursor = Cursor::default();
        let mut ids = HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            self.owner.endpoint.validate(&entry.request)?;
            if cursor
                .now
                .checked_add(entry.request.delta_ns)
                .is_none_or(|n| n > 9_007_199_254_740_991)
                || cursor.pending
                || cursor.fenced
                || !ids.insert(&entry.id)
                || entry.request.request_id != index as u64 + 1
                || (cursor.document.is_none() && entry.request.operation != ClockOperation::Read)
            {
                return Err(InvalidRequest("invalid WebView request history"));
            }
            match &entry.recovery {
                Recovery::Prepared { request } | Recovery::Reconcile { request } => {
                    if request != &entry.request {
                        return Err(InvalidRequest("WebView binding differs"));
                    }
                    cursor.pending = true;
                }
                Recovery::Completed { reply } => {
                    reply.validate(
                        &self.owner.endpoint,
                        &entry.request,
                        cursor.document.as_deref(),
                    )?;
                    match reply.status {
                        Status::Ok => {
                            let expected = cursor
                                .now
                                .checked_add(entry.request.delta_ns)
                                .ok_or(InvalidRequest("WebView frontier overflow"))?;
                            if reply.now_ns != expected {
                                return Err(InvalidRequest(
                                    "WebView frontier differs from history",
                                ));
                            }
                            cursor.now = reply.now_ns;
                            cursor.document = reply.document.clone();
                        }
                        Status::NotReady => {
                            if cursor.document.is_some()
                                || reply.now_ns != 0
                                || reply.applied != Some(false)
                                || reply.state.is_some()
                                || reply.document.is_some()
                                || reply.progress.is_some()
                            {
                                return Err(InvalidRequest("WebView readiness regressed"));
                            }
                        }
                        Status::Unknown | Status::Rejected => cursor.fenced = true,
                    }
                }
            }
        }
        Ok(cursor)
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
    /// Read a validated original clock completion without opening a writer or creating lock files.
    pub fn read_completion(
        directory: &Path,
        id: &RequestId,
        kind: ClockOperation,
        delta_ns: u64,
    ) -> Result<Option<(Owner, Reply)>, StoreError> {
        let Some(state): Option<State> = load(&directory.join("requests.json"))? else {
            return Ok(None);
        };
        state.validate()?;
        let Some(entry) = state.entries.iter().find(|entry| &entry.id == id) else {
            return Ok(None);
        };
        if entry.request.operation != kind || entry.request.delta_ns != delta_ns {
            return Err(StoreError::IdentityChanged);
        }
        Ok(match &entry.recovery {
            Recovery::Completed { reply } => Some((state.owner, *reply.clone())),
            _ => None,
        })
    }

    pub fn create(directory: &Path, owner: Owner) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(directory, owner, budget.as_ref())
    }
    fn create_with_budget(directory: &Path, owner: Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let state = State {
            owner,
            budget_sha256: budget.map(WriteBudget::binding_sha256).transpose()?,
            entries: Vec::new(),
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
    fn save_receipt<T: Serialize>(&self, path: &Path, value: &T) -> std::io::Result<()> {
        match &self.budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
            None => save_atomic(path, value),
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
    /// Inspect retained journal state without sending, recovering or advancing anything.
    pub fn inspect(&self) -> frontcap_core::native::webview_progress::Journal<ClockRequest, Reply> {
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
    pub fn prepare(
        &mut self,
        id: RequestId,
        operation: ClockOperation,
        delta_ns: u64,
    ) -> Result<Recovery, StoreError> {
        self.writable()?;
        if let Some(entry) = self.state.entries.iter().find(|e| e.id == id) {
            if entry.request.operation != operation || entry.request.delta_ns != delta_ns {
                return Err(StoreError::Conflict);
            }
            return Ok(entry.recovery.clone());
        }
        let cursor = self.state.validate()?;
        if cursor.pending {
            return Err(StoreError::Pending);
        }
        if cursor.fenced {
            return Err(InvalidRequest("WebView generation requires replacement").into());
        }
        let request = ClockRequest {
            generation: self.state.owner.endpoint.generation.clone(),
            request_id: self.state.entries.len() as u64 + 1,
            operation,
            delta_ns,
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
    pub fn dispatch(&mut self, id: &RequestId) -> Result<ClockRequest, StoreError> {
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
    /// Accept independently retrieved evidence for the exact dispatched request; never sends it again.
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
            Recovery::Reconcile { .. } => {}
        }
        entry.recovery = Recovery::Completed {
            reply: Box::new(reply),
        };
        self.publish(state)
    }
    /// Inspect the app's atomic last-receipt file. Never contacts the endpoint or advances its cursor.
    /// The caller binds this path to the owned app container, not a replacement target.
    pub fn reconcile(&mut self, id: &RequestId, receipt_path: &Path) -> Result<Reply, StoreError> {
        match self.recovery(id)? {
            Recovery::Completed { reply } => return Ok(*reply),
            Recovery::Prepared { .. } => return Err(StoreError::NotDispatched),
            Recovery::Reconcile { .. } => {}
        }
        let bytes = crate::webview::read_receipt_bytes(receipt_path)?;
        let reply: Reply = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        // Validate the entire proposed history before preserving bytes as a recovered reply.
        let mut proposed = self.state.clone();
        let entry = proposed
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        entry.recovery = Recovery::Completed {
            reply: Box::new(reply.clone()),
        };
        proposed.validate()?;
        let raw = String::from_utf8(bytes).map_err(std::io::Error::other)?;
        self.save_receipt(
            &self
                .path
                .parent()
                .unwrap()
                .join(format!("{}.reply.json", id.as_str())),
            &raw,
        )?;
        self.publish(proposed)?;
        Ok(reply)
    }
    /// Caller holds target ownership. A reopened dispatched request only reports uncertainty.
    /// Retained completed replies need no live process or network access.
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: ClockOperation,
        delta_ns: u64,
        deadline: Instant,
    ) -> Result<Reply, StoreError> {
        match self.prepare(id.clone(), operation, delta_ns)? {
            Recovery::Completed { reply } => return Ok(*reply),
            Recovery::Reconcile { .. } => return Err(StoreError::AlreadyDispatched),
            Recovery::Prepared { .. } => {}
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "WebView dispatch deadline expired",
            )
            .into());
        }
        if self.state.owner.process.has_exited()? {
            return Err(StoreError::IdentityChanged);
        }
        let document = self.state.validate()?.document;
        let request = self.dispatch(&id)?;
        let observation = exchange(
            &self.state.owner.endpoint,
            &request,
            document.as_deref(),
            deadline,
        )?;
        // Preserve all producer observations before recording the interpreted completion.
        // A failed publication leaves this request dispatched and blocks further effects.
        let raw = String::from_utf8(observation.bytes).map_err(std::io::Error::other)?;
        self.save_receipt(
            &self
                .path
                .parent()
                .unwrap()
                .join(format!("{}.reply.json", id.as_str())),
            &raw,
        )?;
        self.complete(&id, observation.reply.clone())?;
        Ok(observation.reply)
    }
}
#[cfg(test)]
#[path = "webview_store_tests.rs"]
mod tests;
