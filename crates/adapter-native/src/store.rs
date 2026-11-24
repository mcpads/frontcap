use crate::request::{
    Completion, Identity, InvalidRequest, NativeRequest, Operation, canonical_uuid,
};
use frontcap_core::ids::RequestId;
use frontcap_core::state::{load, save_atomic, save_atomic_bytes};
use frontcap_core::write_budget::{Class, WriteBudget};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Invalid(#[from] InvalidRequest),
    #[error("native request store is already owned")]
    Busy,
    #[error("target identity or frozen implementation differs from the saved session")]
    IdentityChanged,
    #[error("request id was already bound to different arguments")]
    Conflict,
    #[error("another native request remains unresolved")]
    Pending,
    #[error("unknown recorder request")]
    Unknown,
    #[error("request was already dispatched; reconcile the exact saved client request")]
    AlreadyDispatched,
    #[error("request has not been dispatched")]
    NotDispatched,
    #[error("storage publication was uncertain; reopen and reconcile before continuing")]
    StorageUncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Progress {
    Prepared {},
    Dispatched {},
    Completed { receipt: Box<Completion> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    operation: Operation,
    request: NativeRequest,
    progress: Progress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    identity: Identity,
    controller_id: String,
    source_sha256: String,
    next_clock: u64,
    next_input: u64,
    #[serde(default = "initial_sequence")]
    next_text: u64,
    #[serde(default = "initial_sequence")]
    next_scroll: u64,
    bindings: BTreeMap<RequestId, Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget_sha256: Option<String>,
}

/// Prepared authorizes one bridge dispatch; Reconcile never authorizes a new native identity.
/// The bridge must apply the existing channel-specific prepared/dispatched/receipt rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    Prepared(NativeRequest),
    Reconcile(NativeRequest),
    Completed(Completion),
}

pub struct RequestStore {
    path: PathBuf,
    state: State,
    _lock: RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}

/// Releases ownership even when preparation returns before a store is constructed.
/// Forked children share the file description but must not release the parent's lease.
pub(crate) struct RequestLease {
    file: File,
    owner_pid: u32,
}

impl RequestLease {
    pub(crate) fn try_acquire(file: File) -> Result<Self, std::fs::TryLockError> {
        file.try_lock()?;
        Ok(Self { file, owner_pid: std::process::id() })
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        if std::process::id() == self.owner_pid {
            let _ = self.file.unlock();
        }
    }
}

pub(crate) fn lock(dir: &Path) -> Result<RequestLease, StoreError> {
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("requests.lock"))?;
    match RequestLease::try_acquire(f) {
        Ok(lease) => Ok(lease),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Busy),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn initial_sequence() -> u64 {
    1
}

impl State {
    fn validate(&self) -> Result<(), InvalidRequest> {
        self.identity.validate()?;
        if !canonical_uuid(&self.controller_id)
            || !digest(&self.source_sha256)
            || self.budget_sha256.as_ref().is_some_and(|value| !digest(value))
            || self.next_clock == 0
            || self.next_input == 0
            || self.next_text == 0
            || self.next_scroll == 0
        {
            return Err(InvalidRequest("invalid native session state"));
        }
        let mut seen = BTreeSet::new();
        let (mut clocks, mut inputs, mut texts, mut scrolls, mut pending) = (0, 0, 0, 0, 0);
        for binding in self.bindings.values() {
            let (channel, number, next) = match &binding.request {
                NativeRequest::Clock(r) => ("clock", r.request_id, self.next_clock),
                NativeRequest::ViewScroll(r) => ("scroll", r.request_id, self.next_scroll),
                NativeRequest::ViewTap(r) => ("input", r.request_id, self.next_input),
                NativeRequest::ViewText(r) => ("text", r.request_id, self.next_text),
            };
            if NativeRequest::build(
                &self.identity,
                &self.controller_id,
                number,
                &binding.operation,
            )? != binding.request
            {
                return Err(InvalidRequest(
                    "native request binding differs from saved intent",
                ));
            }
            match &binding.progress {
                Progress::Completed { receipt } => {
                    receipt.validate(&self.identity, &binding.request)?;
                    if let (Operation::ReplaceViewText {edit}, Completion::ViewText {receipt,..})=(&binding.operation,receipt.as_ref())
                        && receipt.state==frontcap_core::native::android_edit::State::Completed
                        && receipt.before.as_deref()!=Some(&edit.before) {
                        return Err(InvalidRequest("edit receipt differs from its observed text basis"));
                    }
                    if let (Operation::ScrollView {scroll},Completion::ViewScroll {receipt,..})=(&binding.operation,receipt.as_ref())
                        && receipt.state==frontcap_core::native::android_tap::State::Completed
                        && receipt.before_offset_px!=Some(scroll.before_offset_px) {
                        return Err(InvalidRequest("scroll receipt differs from its observed offset basis"));
                    }
                    if receipt.consumes_sequence() {
                        if number >= next || !seen.insert((channel, number)) {
                            return Err(InvalidRequest(
                                "completed native sequence is inconsistent",
                            ));
                        }
                        if channel == "clock" {
                            clocks += 1;
                        } else if channel == "input" {
                            inputs += 1;
                        } else if channel == "scroll" {
                            scrolls += 1;
                        } else {
                            texts += 1;
                        }
                    } else if number > next {
                        return Err(InvalidRequest("rejected native sequence is inconsistent"));
                    }
                }
                _ => {
                    pending += 1;
                    if number != next {
                        return Err(InvalidRequest("pending native sequence is inconsistent"));
                    }
                }
            }
        }
        if pending > 1
            || clocks != self.next_clock - 1
            || inputs != self.next_input - 1
            || texts != self.next_text - 1
            || scrolls != self.next_scroll - 1
        {
            return Err(InvalidRequest(
                "native request history has gaps or multiple pending operations",
            ));
        }
        Ok(())
    }
}

impl RequestStore {
    /// Create only for a freshly owned app with unused client sequences. Never opens an old session.
    pub fn create(
        dir: &Path,
        identity: Identity,
        controller_id: String,
        source_sha256: String,
    ) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(dir, identity, controller_id, source_sha256, budget.as_ref())
    }
    fn create_with_budget(dir: &Path, identity: Identity, controller_id: String, source_sha256: String,
        budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let state = State {
            identity,
            controller_id,
            source_sha256,
            next_clock: 1,
            next_input: 1,
            next_text: 1,
            next_scroll: 1,
            bindings: BTreeMap::new(),
            budget_sha256: budget.map(WriteBudget::binding_sha256).transpose()?,
        };
        state.validate()?;
        std::fs::create_dir(dir)?;
        let held = lock(dir)?;
        let path = dir.join("requests.json");
        match budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &path, &state)?,
            None => save_atomic(&path, &state)?,
        }
        Ok(Self {
            path,
            state,
            _lock: held,
            poisoned: false,
            budget: budget.cloned(),
        })
    }

    /// Missing/corrupt state, a replacement target or changed implementation never starts over.
    pub fn open(dir: &Path, identity: &Identity, source_sha256: &str) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::open_with_budget(dir, identity, source_sha256, budget.as_ref())
    }
    fn open_with_budget(dir: &Path, identity: &Identity, source_sha256: &str,
        budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let held = lock(dir)?;
        let path = dir.join("requests.json");
        let mut state: State = load(&path)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing native request state")
        })?;
        state.validate()?;
        if &state.identity != identity || state.source_sha256 != source_sha256 {
            return Err(StoreError::IdentityChanged);
        }
        let binding = budget.map(WriteBudget::binding_sha256).transpose()?;
        if state.budget_sha256.is_some() && state.budget_sha256 != binding {
            return Err(std::io::Error::other("native request write allowance differs").into());
        }
        // A legacy store first extended under an allowance binds that allowance on publication.
        if state.budget_sha256.is_none() { state.budget_sha256 = binding; }
        Ok(Self {
            path,
            state,
            _lock: held,
            poisoned: false,
            budget: budget.cloned(),
        })
    }

    /// Read immutable completion evidence without taking ownership, creating files or replaying.
    /// A concurrent writer can only add a completion; an older snapshot remains unresolved.
    pub fn read_completion(
        dir: &Path,
        identity: &Identity,
        controller: &str,
        source: &str,
        id: &RequestId,
        operation: &Operation,
    ) -> Result<Option<Completion>, StoreError> {
        let state: State = load(&dir.join("requests.json"))?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing native request state")
        })?;
        state.validate()?;
        if &state.identity != identity
            || state.controller_id != controller
            || state.source_sha256 != source
        {
            return Err(StoreError::IdentityChanged);
        }
        let Some(binding) = state.bindings.get(id) else {
            return Ok(None);
        };
        if &binding.operation != operation {
            return Err(StoreError::Conflict);
        }
        Ok(match &binding.progress {
            Progress::Completed { receipt } => Some((**receipt).clone()),
            _ => None,
        })
    }

    fn read_bound(dir: &Path, identity: &Identity, controller: &str, source: &str) -> Result<State, StoreError> {
        let state: State = load(&dir.join("requests.json"))?.ok_or(StoreError::Unknown)?;
        state.validate()?;
        if &state.identity != identity || state.controller_id != controller || state.source_sha256 != source {
            return Err(StoreError::IdentityChanged);
        }
        Ok(state)
    }
    pub fn has_request(dir: &Path, identity: &Identity, controller: &str, source: &str, id: &RequestId) -> Result<bool, StoreError> {
        Ok(Self::read_bound(dir, identity, controller, source)?.bindings.contains_key(id))
    }
    pub fn validate_replacement(dir: &Path, identity: &Identity, controller: &str, source: &str, id: &RequestId) -> Result<(), StoreError> {
        let state = Self::read_bound(dir, identity, controller, source)?;
        if state.bindings.contains_key(id) {return Err(StoreError::Conflict);}
        if state.bindings.values().any(|b| !matches!(b.progress, Progress::Completed {..})) {return Err(StoreError::Pending);}
        Ok(())
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
        // A directory-sync failure can follow a successful rename. Do not continue from stale memory.
        self.poisoned = true;
        save_atomic_bytes(&self.path, bytes.as_bytes())?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }

    pub fn recovery(&self, id: &RequestId) -> Result<Recovery, StoreError> {
        self.writable()?;
        let binding = self.state.bindings.get(id).ok_or(StoreError::Unknown)?;
        Ok(match &binding.progress {
            Progress::Prepared {} => Recovery::Prepared(binding.request.clone()),
            Progress::Dispatched {} => Recovery::Reconcile(binding.request.clone()),
            Progress::Completed { receipt } => Recovery::Completed((**receipt).clone()),
        })
    }

    /// Persist the exact request before allowing the caller to contact the bridge.
    pub fn prepare(&mut self, id: RequestId, operation: Operation) -> Result<Recovery, StoreError> {
        self.writable()?;
        if let Some(existing) = self.state.bindings.get(&id) {
            if existing.operation != operation {
                return Err(StoreError::Conflict);
            }
            return self.recovery(&id);
        }
        operation.validate(&self.state.identity)?;
        if self
            .state
            .bindings
            .values()
            .any(|b| !matches!(b.progress, Progress::Completed { .. }))
        {
            return Err(StoreError::Pending);
        }
        let number = match operation {
            Operation::ScrollView { .. } => self.state.next_scroll,
            Operation::TapView { .. } => self.state.next_input,
            Operation::ReplaceViewText { .. } => self.state.next_text,
            _ => self.state.next_clock,
        };
        let request = NativeRequest::build(
            &self.state.identity,
            &self.state.controller_id,
            number,
            &operation,
        )?;
        let mut state = self.state.clone();
        state.bindings.insert(
            id.clone(),
            Binding {
                operation,
                request,
                progress: Progress::Prepared {},
            },
        );
        self.publish(state)?;
        self.recovery(&id)
    }

    pub fn dispatch(&mut self, id: &RequestId) -> Result<NativeRequest, StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let binding = state.bindings.get_mut(id).ok_or(StoreError::Unknown)?;
        if !matches!(binding.progress, Progress::Prepared {}) {
            return Err(StoreError::AlreadyDispatched);
        }
        binding.progress = Progress::Dispatched {};
        let request = binding.request.clone();
        self.publish(state)?;
        Ok(request)
    }

    /// Publish an identity-bound terminal result already validated by the native bridge.
    pub fn complete(&mut self, id: &RequestId, receipt: Completion) -> Result<(), StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let binding = state.bindings.get_mut(id).ok_or(StoreError::Unknown)?;
        receipt.validate(&state.identity, &binding.request)?;
        match &binding.progress {
            Progress::Prepared {} => return Err(StoreError::NotDispatched),
            Progress::Completed { receipt: previous } => {
                return if **previous == receipt {
                    Ok(())
                } else {
                    Err(StoreError::Conflict)
                };
            }
            Progress::Dispatched {} => {}
        }
        if receipt.consumes_sequence() {
            match binding.request {
                NativeRequest::Clock(_) => state.next_clock += 1,
                NativeRequest::ViewScroll(_) => state.next_scroll += 1,
                NativeRequest::ViewTap(_) => state.next_input += 1,
                NativeRequest::ViewText(_) => state.next_text += 1,
            }
        }
        binding.progress = Progress::Completed { receipt: Box::new(receipt) };
        self.publish(state)
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
