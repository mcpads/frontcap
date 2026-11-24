//! Durable Flutter request bindings. Reopening evidence never grants target ownership or redispatch.
use crate::store::{StoreError, lock};
use frontcap_core::{
    canonical::sha256_hex,
    ids::RequestId,
    native::{
        flutter::{Completion, MAX_LOGICAL_US, Observation, Operation, Request},
        request::InvalidRequest,
    },
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub use frontcap_core::native::flutter::{Owner, Platform};
fn bind(owner: &Owner, id: &RequestId, operation: Operation) -> Request {
    // Managed recorder IDs can exceed the endpoint's 100-byte limit. Preserve
    // the exact caller ID separately and use its stable digest only on the wire.
    Request {
        generation: owner.generation.clone(),
        id: format!("r{}", sha256_hex(id.as_str())),
        operation,
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Progress {
    Prepared {},
    Dispatched {},
    Completed { receipt: Box<Completion> },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    request: Request,
    progress: Progress,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    owner: Owner,
    entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    external_entries: Vec<ExternalEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    system_entries: Vec<ExternalEntry>,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalEntry { id: RequestId, after_entries: usize }
#[derive(Default)]
struct Cursor {
    time: u64,
    basis: Option<Observation>,
    faulted: bool,
    stopped: bool,
    pending: bool,
}
impl Cursor {
    fn admit(&self, request: &Request) -> Result<(), InvalidRequest> {
        if self.pending || self.stopped {
            return Err(InvalidRequest("Flutter history has no admission authority"));
        }
        if self.faulted && !matches!(request.operation, Operation::Read {} | Operation::Stop {}) {
            return Err(InvalidRequest("Flutter timeline is faulted"));
        }
        request.validate_basis(self.basis.as_ref())?;
        if let Operation::Advance { delta_us } = request.operation
            && self
                .time
                .checked_add(delta_us)
                .is_none_or(|v| v > MAX_LOGICAL_US)
            {
                return Err(InvalidRequest("Flutter requested instant overflow"));
            }
        Ok(())
    }
    fn complete(&mut self, request: &Request, receipt: &Completion) -> Result<(), InvalidRequest> {
        receipt.validate(request, self.time, self.basis.as_ref())?;
        if self.faulted && !receipt.faulted {
            return Err(InvalidRequest(
                "Flutter fault cannot clear in the same generation",
            ));
        }
        self.time = receipt.logical_us;
        self.faulted = receipt.faulted;
        match request.operation {
            Operation::Observe {} => self.basis = receipt.surface.clone(),
            Operation::Tap { .. }
            | Operation::Scroll { .. }
            | Operation::ReplaceText { .. }
            | Operation::Advance { .. }
            | Operation::Stop {} => self.basis = None,
            _ => {}
        }
        if receipt.not_applied.is_some() {
            self.basis = None;
        }
        self.stopped = matches!(request.operation, Operation::Stop {});
        Ok(())
    }
}
impl State {
    fn validate(&self) -> Result<Cursor, InvalidRequest> {
        self.owner.validate()?;
        let mut cursor = Cursor::default();
        let mut ids = HashSet::new();
        for history in [&self.external_entries, &self.system_entries] {
            let mut previous = 0;
            for entry in history {
                if entry.after_entries < previous || entry.after_entries > self.entries.len() || !ids.insert(&entry.id) {
                    return Err(InvalidRequest("invalid Flutter host-operation history"));
                }
                previous = entry.after_entries;
            }
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if self.external_entries.iter().chain(&self.system_entries).any(|external| external.after_entries == index) {
                if cursor.pending || cursor.stopped || cursor.faulted {
                    return Err(InvalidRequest("external entry follows unavailable Flutter history"));
                }
                cursor.basis = None;
            }
            if !ids.insert(&entry.id)
                || bind(&self.owner, &entry.id, entry.request.operation.clone()) != entry.request
            {
                return Err(InvalidRequest(
                    "Flutter binding differs from saved owner or caller",
                ));
            }
            cursor.admit(&entry.request)?;
            match &entry.progress {
                Progress::Completed { receipt } => cursor.complete(&entry.request, receipt)?,
                _ => cursor.pending = true,
            }
        }
        if self.external_entries.iter().chain(&self.system_entries).any(|entry| entry.after_entries == self.entries.len()) {
            if cursor.pending || cursor.stopped || cursor.faulted {
                return Err(InvalidRequest("external entry follows unavailable Flutter history"));
            }
            cursor.basis = None;
        }
        Ok(cursor)
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Recovery {
    Prepared(Request),
    Reconcile(Request),
    Completed(Box<Completion>),
}
#[path = "flutter_history.rs"]
mod history;

pub struct RequestStore {
    path: PathBuf,
    state: State,
    _held: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
    events: Option<u64>,
}
impl RequestStore {
    pub(crate) fn write_budget(&self) -> Option<&WriteBudget> { self.budget.as_ref() }
    pub fn contains(&self, id: &RequestId) -> bool {
        self.state.entries.iter().any(|e| &e.id == id)
            || self.state.external_entries.iter().chain(&self.state.system_entries).any(|e| &e.id == id)
    }
    pub fn has_request(directory: &Path, owner: &Owner, id: &RequestId) -> Result<bool, StoreError> {
        Ok(Self::read(directory, owner)?.entries.iter().any(|entry| &entry.id == id))
    }
    pub fn create(directory: &Path, owner: Owner) -> Result<Self, StoreError> {
        Self::create_with_budget(directory, owner, None)
    }
    pub fn create_with_budget(directory: &Path, owner: Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let state = State {
            owner,
            entries: vec![],
            external_entries: vec![],
            system_entries: vec![],
        };
        state.validate()?;
        std::fs::create_dir(directory)?;
        let held = lock(directory)?;
        let path = directory.join("requests.json");
        std::fs::create_dir(directory.join("events"))?;
        let head = history::Head { owner: state.owner.clone(), event_count: 0 };
        match budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &path, &head)?,
            None => save_atomic(&path, &head)?,
        }
        Ok(Self {
            path,
            state,
            _held: held,
            poisoned: false,
            budget: budget.cloned(),
            events: Some(0),
        })
    }
    pub fn open(directory: &Path, owner: &Owner) -> Result<Self, StoreError> {
        Self::open_with_budget(directory, owner, None)
    }
    pub fn open_with_budget(directory: &Path, owner: &Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let held = lock(directory)?;
        let (state, events) = history::read(directory, owner)?;
        Ok(Self {
            path: directory.join("requests.json"),
            state,
            _held: held,
            poisoned: false,
            budget: budget.cloned(),
            events,
        })
    }
    fn read(directory: &Path, owner: &Owner) -> Result<State, StoreError> {
        history::read(directory, owner).map(|(state, _)| state)
    }

    pub fn read_completion(
        directory: &Path,
        owner: &Owner,
        id: &RequestId,
        operation: &Operation,
    ) -> Result<Option<Completion>, StoreError> {
        let state = Self::read(directory, owner)?;
        let Some(entry) = state.entries.iter().find(|e| &e.id == id) else {
            return Ok(None);
        };
        if &entry.request.operation != operation {
            return Err(StoreError::Conflict);
        }
        Ok(match &entry.progress {
            Progress::Completed { receipt } => Some((**receipt).clone()),
            _ => None,
        })
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
        if let Some(count) = self.events {
            let event = history::Event::between(&self.state, &state)?;
            let next = count.checked_add(1).ok_or(StoreError::StorageUncertain)?;
            let path = self.path.parent().unwrap().join("events").join(format!("{count}.json"));
            // An unpublished event is evidence, not permission to overwrite or continue.
            if path.try_exists()? { self.poisoned = true; return Err(StoreError::StorageUncertain); }
            let payload = frontcap_core::canonical::canonical_of(&event);
            let head = frontcap_core::canonical::canonical_of(&history::Head {
                owner: state.owner.clone(), event_count: next,
            });
            if let Some(budget) = &self.budget {
                budget.charge(Class::Ordinary, (payload.len() + head.len()) as u64)?;
            }
            self.poisoned = true;
            save_atomic_bytes(&path, payload.as_bytes())?;
            save_atomic_bytes(&self.path, head.as_bytes())?;
            self.events = Some(next);
        } else {
            let bytes = frontcap_core::canonical::canonical_of(&state);
            if let Some(budget) = &self.budget {
                budget.charge(Class::Ordinary, bytes.len() as u64)?;
            }
            self.poisoned = true;
            save_atomic_bytes(&self.path, bytes.as_bytes())?;
        }
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    pub fn recovery(&self, id: &RequestId) -> Result<Recovery, StoreError> {
        self.writable()?;
        let entry = self
            .state
            .entries
            .iter()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        Ok(match &entry.progress {
            Progress::Prepared {} => Recovery::Prepared(entry.request.clone()),
            Progress::Dispatched {} => Recovery::Reconcile(entry.request.clone()),
            Progress::Completed { receipt } => Recovery::Completed(receipt.clone()),
        })
    }
    pub fn validate_operation(
        &self,
        id: &RequestId,
        operation: &Operation,
    ) -> Result<(), StoreError> {
        self.writable()?;
        if self.state.external_entries.iter().chain(&self.state.system_entries).any(|entry| &entry.id == id) {
            return Err(StoreError::Conflict);
        }
        if let Some(entry) = self.state.entries.iter().find(|e| &e.id == id) {
            return if &entry.request.operation == operation {
                Ok(())
            } else {
                Err(StoreError::Conflict)
            };
        }
        let cursor = self.state.validate()?;
        if cursor.pending {
            return Err(StoreError::Pending);
        }
        cursor.admit(&bind(&self.state.owner, id, operation.clone()))?;
        Ok(())
    }
    pub fn prepare(&mut self, id: RequestId, operation: Operation) -> Result<Recovery, StoreError> {
        self.validate_operation(&id, &operation)?;
        if let Some(entry) = self.state.entries.iter().find(|e| e.id == id) {
            if entry.request.operation != operation {
                return Err(StoreError::Conflict);
            }
            return self.recovery(&id);
        }
        let cursor = self.state.validate()?;
        if cursor.pending {
            return Err(StoreError::Pending);
        }
        let request = bind(&self.state.owner, &id, operation);
        cursor.admit(&request)?;
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id: id.clone(),
            request,
            progress: Progress::Prepared {},
        });
        self.publish(state)?;
        self.recovery(&id)
    }
    pub fn validate_external_entry(&self, id: &RequestId) -> Result<(), StoreError> {
        self.writable()?;
        if self.state.entries.iter().any(|entry| &entry.id == id)
            || self.state.external_entries.iter().chain(&self.state.system_entries).any(|entry| &entry.id == id) {
            return Err(StoreError::Conflict);
        }
        let cursor = self.state.validate()?;
        if cursor.pending || cursor.stopped || cursor.faulted { return Err(StoreError::Pending); }
        Ok(())
    }
    pub fn validate_replacement(&self, id: &RequestId) -> Result<(), StoreError> {
        self.writable()?;
        if self.contains(id) { return Err(StoreError::Conflict); }
        let cursor = self.state.validate()?;
        if cursor.pending || cursor.stopped { return Err(StoreError::Pending); }
        // A completed fault may be abandoned explicitly; an uncertain effect may not.
        Ok(())
    }
    /// Persist before an OS action, without allocating or sending a Flutter endpoint request.
    pub fn invalidate_for_external_entry(&mut self, id: &RequestId) -> Result<(), StoreError> {
        self.validate_external_entry(id)?;
        let mut state = self.state.clone();
        state.external_entries.push(ExternalEntry { id: id.clone(), after_entries: state.entries.len() });
        self.publish(state)
    }
    pub fn invalidate_for_system_operation(&mut self, id: &RequestId) -> Result<(), StoreError> {
        self.validate_external_entry(id)?;
        let mut state = self.state.clone();
        state.system_entries.push(ExternalEntry { id: id.clone(), after_entries: state.entries.len() });
        self.publish(state)
    }
    pub fn dispatch(&mut self, id: &RequestId) -> Result<Request, StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        if !matches!(entry.progress, Progress::Prepared {}) {
            return Err(StoreError::AlreadyDispatched);
        }
        entry.progress = Progress::Dispatched {};
        let request = entry.request.clone();
        self.publish(state)?;
        Ok(request)
    }
    /// Call only after independently validating response transport and raster artifacts.
    pub fn complete(&mut self, id: &RequestId, receipt: Completion) -> Result<(), StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let entry = state
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or(StoreError::Unknown)?;
        match &entry.progress {
            Progress::Prepared {} => return Err(StoreError::NotDispatched),
            Progress::Completed { receipt: old } => {
                return if old.as_ref() == &receipt {
                    Ok(())
                } else {
                    Err(StoreError::Conflict)
                };
            }
            Progress::Dispatched {} => {}
        }
        entry.progress = Progress::Completed { receipt: Box::new(receipt) };
        self.publish(state)
    }
}
#[cfg(test)]
#[path = "flutter_store_tests.rs"]
mod tests;
