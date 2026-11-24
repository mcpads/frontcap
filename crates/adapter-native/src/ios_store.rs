//! Durable caller IDs with independent app-clock and XCTest sequences.
//! Reopening a store never grants process ownership or permission to redispatch.
use crate::store::{StoreError, lock};
pub use frontcap_core::native::ios_request::{Completion, Operation, Owner, Request};
use frontcap_core::{
    ids::RequestId,
    native::request::InvalidRequest,
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Progress {
    Prepared,
    Dispatched,
    Completed { receipt: Completion },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    operation: Operation,
    request: Request,
    progress: Progress,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    owner: Owner,
    next_clock: u64,
    next_ui: u64,
    #[serde(default = "initial_ui")]
    first_ui: u64,
    /// External OS actions consume no XCTest sequence, but invalidate earlier input bases.
    #[serde(default)]
    invalidated_through_ui: u64,
    bindings: BTreeMap<RequestId, Binding>,
}
fn initial_ui() -> u64 { 2 }
impl State {
    fn app_state(&self) -> frontcap_core::native::app_state::State {
        self.bindings.values().filter_map(|b| match &b.progress {
            Progress::Completed { receipt: Completion::Ui(c) } => match &c.outcome {
                frontcap_core::native::ios_ui::UiOutcome::AppStateChanged { change, .. } => Some((c.request.request_id, change.requested)),
                _ => None,
            },
            _ => None,
        }).max_by_key(|(n,_)| *n).map(|(_,s)| s).unwrap_or(frontcap_core::native::app_state::State::Foreground)
    }
    fn admit_operation(&self, id: &RequestId, operation: &Operation) -> Result<(), StoreError> {
        if let Some(b) = self.bindings.get(id) {
            return if &b.operation == operation { Ok(()) } else { Err(StoreError::Conflict) };
        }
        if self.bindings.values().any(|b| !matches!(b.progress, Progress::Completed {..})) { return Err(StoreError::Pending); }
        if let Operation::SetAppState { state, .. } = operation {
            if *state == self.app_state() { return Err(InvalidRequest("UIKit app is already in the requested host state").into()); }
        } else if self.app_state() == frontcap_core::native::app_state::State::Background
            && !matches!(operation, Operation::ObserveSystemAlerts | Operation::TapSystemButton {..} | Operation::Finish) {
            return Err(InvalidRequest("foreground the same UIKit app before app work").into());
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), InvalidRequest> {
        self.owner.validate()?;
        let mut clocks = BTreeSet::new();
        let mut ui = BTreeSet::new();
        let mut pending = 0;
        for b in self.bindings.values() {
            let number = b.request.number();
            let clock_number=match &b.request { Request::Press { clock, .. } => clock.request_id, _ => 0 };
            if Request::build_pair(&self.owner, &b.operation, number, clock_number)? != b.request {
                return Err(InvalidRequest("iOS binding differs from intent"));
            }
            let next = if b.request.is_clock() {
                self.next_clock
            } else {
                self.next_ui
            };
            match &b.progress {
                Progress::Completed { receipt } => {
                    receipt.validate(&self.owner, &b.request)?;
                    if let Completion::Ui(result) = receipt
                        && let frontcap_core::native::ios_ui::UiOutcome::SystemButtonTapped { reference, observation } = &result.outcome {
                        let prior = self.bindings.values().find(|p| !p.request.is_clock() && p.request.number() + 1 == number);
                        if let Some(Binding { progress: Progress::Completed { receipt: Completion::Ui(p) }, .. }) = prior
                            && let frontcap_core::native::ios_ui::UiOutcome::SystemAlertsObserved { catalog, .. } = &p.outcome {
                            catalog.target(observation, reference)?;
                        } else { return Err(InvalidRequest("system input has no preceding alert observation")); }
                    }
                    if let (Operation::ScrollTarget { reference,observation,direction },Completion::Ui(result))=(&b.operation,receipt)
                        && let frontcap_core::native::ios_ui::UiOutcome::Scrolled { before,now_ns,.. }=&result.outcome {
                        let prior=self.bindings.values().find(|p| !p.request.is_clock() && p.request.number()+1==number);
                        if let Some(Binding { progress:Progress::Completed { receipt:Completion::Ui(p) },.. })=prior
                            && let frontcap_core::native::ios_ui::UiOutcome::ObservedTargets { catalog,now_ns:observed_now,.. }=&p.outcome {
                            if before!=frontcap_core::native::ios_scroll::target(catalog,reference,*direction)?
                                || observation!=&catalog.observation || now_ns!=observed_now {
                                return Err(InvalidRequest("scroll differs from stored observation basis"));
                            }
                        } else { return Err(InvalidRequest("scroll has no preceding observation")); }
                    }
                    if let (Operation::ReplaceText { reference,observation,start,end,text }, Completion::Ui(result))=(&b.operation,receipt)
                        && let frontcap_core::native::ios_ui::UiOutcome::TextEdit {
                            now_ns,delivery:frontcap_core::native::ios_edit::Delivery::UikitTextControl { receipt:source },..
                        }=&result.outcome {
                        let previous=self.bindings.values().find(|prior| !prior.request.is_clock() && prior.request.number()+1==number);
                        if let Some(Binding { progress:Progress::Completed { receipt:Completion::Ui(prior) },.. })=previous
                            && let frontcap_core::native::ios_ui::UiOutcome::ObservedTargets { catalog,now_ns:observed_now,.. }=&prior.outcome {
                            let basis=frontcap_core::native::ios_edit::validate_target(catalog,reference,*start,*end,text)?;
                            if basis!=&source.request.arguments.basis || observation!=&catalog.observation || now_ns!=observed_now {
                                return Err(InvalidRequest("text edit differs from stored observation basis"));
                            }
                        } else { return Err(InvalidRequest("text edit has no preceding observation")); }
                    }
                    let used = if b.request.is_clock() {
                        &mut clocks
                    } else {
                        &mut ui
                    };
                    if number >= next || !used.insert(number) {
                        return Err(InvalidRequest("duplicate iOS completion sequence"));
                    }
                    if let Completion::Press { clock, .. } = receipt {
                        use frontcap_core::native::ios_request::PressClock;
                        if clock_number>self.next_clock || (matches!(clock,PressClock::Delivered { .. })
                            && (clock_number>=self.next_clock || !clocks.insert(clock_number))) {
                            return Err(InvalidRequest("inconsistent press clock sequence"));
                        }
                    }
                    if b.operation == Operation::Finish
                        && number.checked_add(1) != Some(self.next_ui)
                    {
                        return Err(InvalidRequest("UI work after Finish"));
                    }
                }
                _ => {
                    pending += 1;
                    if number != next {
                        return Err(InvalidRequest("pending iOS sequence differs"));
                    }
                    if matches!(b.request,Request::Press { .. }) && clock_number!=self.next_clock {
                        return Err(InvalidRequest("pending press clock sequence differs"));
                    }
                }
            }
        }
        // The shared UI sequence orders visibility and app UI work, including pending intents.
        let mut ordered: Vec<_> = self.bindings.values().filter(|b| !b.request.is_clock()).collect();
        ordered.sort_by_key(|b| b.request.number());
        let mut visibility = frontcap_core::native::app_state::State::Foreground;
        for binding in ordered {
            if let Operation::SetAppState { state, .. } = binding.operation {
                if state == visibility { return Err(InvalidRequest("repeated UIKit host state in history")); }
                if matches!(binding.progress, Progress::Completed { .. }) { visibility = state; }
            } else if visibility == frontcap_core::native::app_state::State::Background
                && !matches!(binding.operation, Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. } | Operation::Finish) {
                return Err(InvalidRequest("app UI work while UIKit history is background"));
            }
        }
        // XCTest sequence 1 is the already-verified initial Launch. Every terminal iOS clock
        // receipt, including an unapplied or applied failure, consumes its sequence.
        if clocks.iter().copied().ne(1..self.next_clock)
            || ui.iter().copied().ne(self.first_ui..self.next_ui)
            || self.next_clock == 0
            || self.first_ui < 2 || self.next_ui < self.first_ui
            || self.invalidated_through_ui >= self.next_ui
            || pending > 1
            || (self.finished() && pending != 0)
        {
            return Err(InvalidRequest("inconsistent iOS request history"));
        }
        Ok(())
    }
    fn finished(&self) -> bool {
        self.bindings.values().any(|b| {
            b.operation == Operation::Finish && matches!(b.progress, Progress::Completed { .. })
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    Prepared(Request),
    Reconcile(Request),
    Completed(Completion),
}
#[path = "ios_history.rs"]
mod history;
pub struct RequestStore {
    path: PathBuf,
    state: State,
    _held: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
    head: Option<history::Head>,
}
impl RequestStore {
    pub fn validate_operation(directory: &Path, owner: &Owner, id: &RequestId, operation: &Operation) -> Result<(), StoreError> {
        Self::read(directory, owner)?.admit_operation(id, operation)
    }
    pub fn validate_foreground(directory: &Path, owner: &Owner) -> Result<(), StoreError> {
        if Self::read(directory, owner)?.app_state() != frontcap_core::native::app_state::State::Foreground {
            return Err(InvalidRequest("UIKit app must be foreground for external entry or restart").into());
        }
        Ok(())
    }
    /// Read-only admission check before recorder or executor allocates a new tap intent.
    /// Exact prior IDs keep their recovery path even after their observation was consumed.
    pub fn validate_ui_input(directory: &Path, owner: &Owner, id: &RequestId,
        operation: &Operation) -> Result<(), StoreError> {
        use frontcap_core::native::ios_ui::UiOutcome;
        let (Operation::TapSystemButton { reference, observation } | Operation::TapTarget { reference, observation } | Operation::PressTarget { reference, observation, .. }
            | Operation::ReplaceText { reference,observation,.. } | Operation::ScrollTarget { reference,observation,.. }) = operation else { return Ok(()); };
        let state = Self::read(directory, owner)?;
        if let Some(binding) = state.bindings.get(id) {
            return if &binding.operation == operation { Ok(()) } else { Err(StoreError::Conflict) };
        }
        if state.bindings.values().any(|b| !matches!(b.progress, Progress::Completed { .. })) {
            return Err(StoreError::Pending);
        }
        let latest = state.bindings.values().filter(|b| !b.request.is_clock())
            .max_by_key(|b| b.request.number());
        if matches!(operation, Operation::TapSystemButton { .. }) {
            if let Some(Binding { progress: Progress::Completed { receipt: Completion::Ui(receipt) }, .. }) = latest
                && receipt.request.request_id > state.invalidated_through_ui
                && let UiOutcome::SystemAlertsObserved { catalog, .. } = &receipt.outcome {
                catalog.target(observation, reference)?;
                return Ok(());
            }
            return Err(InvalidRequest("system input requires the latest stable alert observation").into());
        }
        if let Some(Binding { progress: Progress::Completed { receipt: Completion::Ui(receipt) }, .. }) = latest
            && receipt.request.request_id > state.invalidated_through_ui
            && let UiOutcome::ObservedTargets { catalog, .. } = &receipt.outcome
            && &catalog.observation == observation
            && catalog.targets.iter().any(|target| &target.reference == reference
                && !catalog.transitioning && target.unique && target.enabled && target.hittable
                && (!matches!(operation,Operation::PressTarget { .. }) || matches!(target.role,
                    frontcap_core::native::ios_targets::Role::TextField | frontcap_core::native::ios_targets::Role::TextView))
                && target.role != frontcap_core::native::ios_targets::Role::StaticText)
        {
            if let Operation::ReplaceText { start,end,text,.. }=operation {
                frontcap_core::native::ios_edit::validate_target(catalog,reference,*start,*end,text)?;
            }
            if let Operation::ScrollTarget { direction,.. }=operation {
                frontcap_core::native::ios_scroll::target(catalog,reference,*direction)?;
            }
            return Ok(());
        }
        Err(InvalidRequest("UIKit tap requires a ready target in the latest observation").into())
    }
    /// Requires a verified Launch receipt at UI sequence 1 and unused initialized clock client.
    pub fn create(directory: &Path, owner: Owner) -> Result<Self, StoreError> {
        Self::create_epoch(directory, owner, 2)
    }
    /// Called only after the live owner verifies the preceding replacement receipt.
    pub(crate) fn create_epoch(directory: &Path, owner: Owner, first_ui: u64) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::create_epoch_with_budget(directory, owner, first_ui, budget.as_ref())
    }
    fn create_epoch_with_budget(directory: &Path, owner: Owner, first_ui: u64, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let state = State {
            owner,
            next_clock: 1,
            next_ui: first_ui,
            first_ui,
            invalidated_through_ui: 0,
            bindings: BTreeMap::new(),
        };
        state.validate()?;
        std::fs::create_dir(directory)?;
        let held = lock(directory)?;
        let path = directory.join("requests.json");
        std::fs::create_dir(directory.join("events"))?;
        let head = history::Head { owner: state.owner.clone(), first_ui, event_count: 0,
            budget_sha256: budget.map(WriteBudget::binding_sha256).transpose()? };
        match budget { Some(budget) => budget.save_atomic(Class::Ordinary, &path, &head)?, None => save_atomic(&path, &head)? }
        Ok(Self { path, state, _held: held, poisoned: false, budget: budget.cloned(), head: Some(head) })
    }
    pub fn open(directory: &Path, owner: &Owner) -> Result<Self, StoreError> {
        let budget = WriteBudget::from_environment()?;
        Self::open_with_budget(directory, owner, budget.as_ref())
    }
    fn open_with_budget(directory: &Path, owner: &Owner, budget: Option<&WriteBudget>) -> Result<Self, StoreError> {
        let held = lock(directory)?;
        let (state, head) = history::read(directory, owner)?;
        if let Some(head) = &head
            && head.budget_sha256 != budget.map(WriteBudget::binding_sha256).transpose()? {
            return Err(StoreError::IdentityChanged);
        }
        Ok(Self { path: directory.join("requests.json"), state, _held: held, poisoned: false,
            budget: budget.cloned(), head })
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
        let Some(binding) = state.bindings.get(id) else {
            return Ok(None);
        };
        if &binding.operation != operation {
            return Err(StoreError::Conflict);
        }
        Ok(match &binding.progress {
            Progress::Completed { receipt } => Some(receipt.clone()),
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
        // Finish must remain publishable after ordinary admission is exhausted.
        let retirement = state.bindings.iter().any(|(id, binding)| binding.operation == Operation::Finish
            && self.state.bindings.get(id) != Some(binding));
        let class = if retirement { Class::Retirement } else { Class::Ordinary };
        if let Some(head) = &self.head {
            let event = history::Event::between(&self.state, &state)?;
            let mut next = head.clone();
            next.event_count = next.event_count.checked_add(1).ok_or(StoreError::StorageUncertain)?;
            let path = self.path.parent().unwrap().join("events").join(format!("{}.json", head.event_count));
            if path.try_exists()? { self.poisoned = true; return Err(StoreError::StorageUncertain); }
            let payload = frontcap_core::canonical::canonical_of(&event);
            let summary = frontcap_core::canonical::canonical_of(&next);
            if let Some(budget) = &self.budget { budget.charge(class, (payload.len() + summary.len()) as u64)?; }
            self.poisoned = true;
            save_atomic_bytes(&path, payload.as_bytes())?;
            save_atomic_bytes(&self.path, summary.as_bytes())?;
            self.head = Some(next);
        } else {
            // Preserve legacy snapshot layout in place; never silently migrate old evidence.
            let bytes = frontcap_core::canonical::canonical_of(&state);
            if let Some(budget) = &self.budget { budget.charge(class, bytes.len() as u64)?; }
            self.poisoned = true;
            save_atomic_bytes(&self.path, bytes.as_bytes())?;
        }
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    pub fn recovery(&self, id: &RequestId) -> Result<Recovery, StoreError> {
        self.writable()?;
        let b = self.state.bindings.get(id).ok_or(StoreError::Unknown)?;
        Ok(match &b.progress {
            Progress::Prepared => Recovery::Prepared(b.request.clone()),
            Progress::Dispatched => Recovery::Reconcile(b.request.clone()),
            Progress::Completed { receipt } => Recovery::Completed(receipt.clone()),
        })
    }
    pub fn validate_replacement(directory:&Path,owner:&Owner,id:&RequestId)->Result<(),StoreError> {
        let state=Self::read(directory,owner)?;
        Self::validate_foreground(directory,owner)?;
        if state.bindings.contains_key(id) {return Err(StoreError::Conflict);}
        if state.finished() || state.bindings.values().any(|b|!matches!(b.progress,Progress::Completed {..})) {return Err(StoreError::Pending);}
        Ok(())
    }
    pub(crate) fn next_ui(&self) -> u64 { self.state.next_ui }
    pub(crate) fn contains(&self, id: &RequestId) -> bool { self.state.bindings.contains_key(id) }
    pub fn has_request(directory: &Path, owner: &Owner, id: &RequestId) -> Result<bool, StoreError> {
        Ok(Self::read(directory, owner)?.bindings.contains_key(id))
    }
    pub(crate) fn validate_external_entry(&self, id: &RequestId) -> Result<(), StoreError> {
        self.writable()?;
        if self.state.app_state() != frontcap_core::native::app_state::State::Foreground {
            return Err(InvalidRequest("foreground the UIKit app before external entry").into());
        }
        if self.state.bindings.contains_key(id) { return Err(StoreError::Conflict); }
        if self.state.finished() || self.state.bindings.values().any(|b| !matches!(b.progress, Progress::Completed { .. })) {
            return Err(StoreError::Pending);
        }
        Ok(())
    }
    /// Called by the live target before a fresh external dispatch, never for cached result reads.
    pub(crate) fn invalidate_for_external_entry(&mut self, id: &RequestId) -> Result<(), StoreError> {
        self.validate_external_entry(id)?;
        let mut state = self.state.clone();
        state.invalidated_through_ui = state.next_ui - 1;
        self.publish(state)
    }
    pub fn prepare(&mut self, id: RequestId, operation: Operation) -> Result<Recovery, StoreError> {
        self.writable()?;
        self.state.admit_operation(&id, &operation)?;
        if let Some(b) = self.state.bindings.get(&id) {
            if b.operation != operation {
                return Err(StoreError::Conflict);
            }
            return self.recovery(&id);
        }
        if self.state.finished() {
            return Err(InvalidRequest("iOS app already finished").into());
        }
        if self
            .state
            .bindings
            .values()
            .any(|b| !matches!(b.progress, Progress::Completed { .. }))
        {
            return Err(StoreError::Pending);
        }
        let number = if matches!(
            operation,
            Operation::ReadClock | Operation::AdvanceClock { .. }
        ) {
            self.state.next_clock
        } else {
            self.state.next_ui
        };
        let request = Request::build_pair(&self.state.owner, &operation, number, self.state.next_clock)?;
        let mut state = self.state.clone();
        state.bindings.insert(
            id.clone(),
            Binding {
                operation,
                request,
                progress: Progress::Prepared,
            },
        );
        self.publish(state)?;
        self.recovery(&id)
    }
    pub fn dispatch(&mut self, id: &RequestId) -> Result<Request, StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let b = state.bindings.get_mut(id).ok_or(StoreError::Unknown)?;
        if b.progress != Progress::Prepared {
            return Err(StoreError::AlreadyDispatched);
        }
        b.progress = Progress::Dispatched;
        let request = b.request.clone();
        self.publish(state)?;
        Ok(request)
    }
    /// Caller independently verifies raw receipt bytes and any observation artifacts before this.
    pub fn complete(&mut self, id: &RequestId, receipt: Completion) -> Result<(), StoreError> {
        self.writable()?;
        let mut state = self.state.clone();
        let b = state.bindings.get_mut(id).ok_or(StoreError::Unknown)?;
        receipt.validate(&state.owner, &b.request)?;
        match &b.progress {
            Progress::Prepared => return Err(StoreError::NotDispatched),
            Progress::Completed { receipt: old } => {
                return if old == &receipt {
                    Ok(())
                } else {
                    Err(StoreError::Conflict)
                };
            }
            Progress::Dispatched => (),
        }
        if b.request.is_clock() {
            state.next_clock += 1;
        } else {
            state.next_ui += 1;
            if matches!(&receipt, Completion::Press { clock: frontcap_core::native::ios_request::PressClock::Delivered { .. }, .. }) {
                state.next_clock += 1;
            }
        }
        b.progress = Progress::Completed { receipt };
        self.publish(state)
    }
}
#[cfg(test)]
#[path = "ios_store_tests.rs"]
mod tests;
