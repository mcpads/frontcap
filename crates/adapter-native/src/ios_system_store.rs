//! Durable system UI request authority shared by Flutter and ordinary native hosts.
use frontcap_core::{
    ids::RequestId,
    native::{
        flutter::{Owner, Platform},
        ios_request::Operation,
        ios_system_ui::{self, Catalog},
        ios_ui::{UiCompletion, UiIdentity, UiOperation, UiOutcome, UiRequest},
    },
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};

/// A real consumer identity on the runner's simulator; no synthetic clock identity is needed.
pub trait Consumer: Clone + PartialEq + Serialize + serde::de::DeserializeOwned {
    fn validate_runner(&self, runner: &UiIdentity) -> io::Result<()>;
}
impl Consumer for Owner {
    fn validate_runner(&self, runner: &UiIdentity) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        runner.validate().map_err(io::Error::other)?;
        if self.platform != Platform::Ios || !self.device.eq_ignore_ascii_case(&runner.simulator) {
            return Err(io::Error::other("system UI owner differs from Flutter"));
        }
        Ok(())
    }
}
impl Consumer for frontcap_core::native::ios_host::Owner {
    fn validate_runner(&self, runner: &UiIdentity) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        runner.validate().map_err(io::Error::other)?;
        if !self.simulator.eq_ignore_ascii_case(&runner.simulator) || self.process.pid == runner.pid {
            return Err(io::Error::other("system UI runner differs from native host"));
        }
        Ok(())
    }
}
pub fn client_directory(target: &Path) -> PathBuf {
    target.join("session/worker/system-ui/client")
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    request: UiRequest,
    completion: Option<UiCompletion>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invalidation {
    id: RequestId,
    after_entries: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State<O = Owner> {
    owner: O,
    runner: UiIdentity,
    entries: Vec<Entry>,
    invalidations: Vec<Invalidation>,
}
impl<O: Consumer> State<O> {
    fn cursor(&self) -> io::Result<(Option<Catalog>, bool)> {
        self.owner.validate_runner(&self.runner)?;
        let mut ids = HashSet::new();
        let mut previous = 0;
        for row in &self.invalidations {
            if row.after_entries < previous
                || row.after_entries > self.entries.len()
                || !ids.insert(&row.id)
            {
                return Err(io::Error::other(
                    "invalid system input invalidation history",
                ));
            }
            previous = row.after_entries;
        }
        let mut catalog: Option<Catalog> = None;
        let mut pending = false;
        for index in 0..=self.entries.len() {
            if self.invalidations.iter().any(|r| r.after_entries == index) {
                if pending {
                    return Err(io::Error::other("host action follows pending system input"));
                }
                catalog = None;
            }
            let Some(row) = self.entries.get(index) else {
                break;
            };
            if pending || !ids.insert(&row.id) || row.request.request_id != index as u64 + 1 {
                return Err(io::Error::other("invalid system request history"));
            }
            row.request
                .validate(&self.runner)
                .map_err(io::Error::other)?;
            admit(&row.request.operation, catalog.as_ref())?;
            catalog = None; // Every attempt consumes the basis, including explicit no-effect input.
            if let Some(c) = &row.completion {
                c.validate_system(&self.runner, &row.request)
                    .map_err(io::Error::other)?;
                if let UiOutcome::SystemAlertsObserved {
                    catalog: observed, ..
                } = &c.outcome
                {
                    catalog = Some(observed.clone());
                }
            } else {
                pending = true;
            }
        }
        Ok((catalog, pending))
    }
}
fn admit(operation: &UiOperation, catalog: Option<&Catalog>) -> io::Result<()> {
    match operation {
        UiOperation::ObserveSystemAlerts {} | UiOperation::SetHostAppState { .. } => Ok(()),
        UiOperation::TapSystemButton {
            observation,
            reference,
        } => catalog
            .ok_or_else(|| io::Error::other("system input requires a fresh alert observation"))?
            .target(observation, reference)
            .map(|_| ())
            .map_err(io::Error::other),
        _ => Err(io::Error::other("operation is not system UI")),
    }
}
pub struct Store<O: Consumer = Owner> {
    path: PathBuf,
    state: State<O>,
    _lock: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}
impl<O: Consumer> Store<O> {
    pub fn create(directory: &Path, owner: O, runner: UiIdentity) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(directory, owner, runner, budget.as_ref())
    }
    pub fn create_with_budget(directory: &Path, owner: O, runner: UiIdentity, budget: Option<&WriteBudget>) -> io::Result<Self> {
        let state = State {
            owner,
            runner,
            entries: vec![],
            invalidations: vec![],
        };
        state.cursor()?;
        std::fs::create_dir(directory)?;
        let lock = crate::store::lock(directory).map_err(io::Error::other)?;
        let path = directory.join("requests.json");
        match budget { Some(budget) => budget.save_atomic(Class::Ordinary, &path, &state)?, None => save_atomic(&path, &state)? }
        Ok(Self {
            path,
            state,
            _lock: lock,
            poisoned: false,
            budget: budget.cloned(),
        })
    }
    fn publish(&mut self, state: State<O>) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("system UI store publication unresolved"));
        }
        state.cursor()?;
        let bytes = frontcap_core::canonical::canonical_of(&state);
        if let Some(budget) = &self.budget { budget.charge(Class::Ordinary, bytes.len() as u64)?; }
        self.poisoned = true;
        save_atomic_bytes(&self.path, bytes.as_bytes())?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    pub fn contains(&self, id: &RequestId) -> bool {
        self.state.entries.iter().any(|r| &r.id == id)
    }
    pub fn ensure_resolved(&self) -> io::Result<()> {
        if self.poisoned || self.state.cursor()?.1 {
            Err(io::Error::other("system UI request remains unresolved"))
        } else {
            Ok(())
        }
    }
    pub fn validate(&self, id: &RequestId, operation: &Operation) -> io::Result<()> {
        self.validate_ui(id, &ios_system_ui::request_operation(operation).map_err(io::Error::other)?)
    }
    pub(crate) fn validate_ui(&self, id: &RequestId, operation: &UiOperation) -> io::Result<()> {
        if let Some(row) = self.state.entries.iter().find(|r| &r.id == id) {
            return if &row.request.operation == operation {
                Ok(())
            } else {
                Err(io::Error::other("system UI request ID conflict"))
            };
        }
        self.ensure_resolved()?;
        if self.state.invalidations.iter().any(|r| &r.id == id) {
            return Err(io::Error::other(
                "system UI ID belongs to an app/URL operation",
            ));
        }
        let request = UiRequest {
            generation: self.state.runner.generation.clone(),
            request_id: self.state.entries.len() as u64 + 1,
            operation: operation.clone(),
        };
        request
            .validate(&self.state.runner)
            .map_err(io::Error::other)?;
        admit(&request.operation, self.state.cursor()?.0.as_ref())
    }
    /// A persisted intent grants one send now; reopening it grants only saved-result lookup.
    pub fn dispatch(&mut self, id: RequestId, operation: &Operation) -> io::Result<UiRequest> {
        self.dispatch_ui(id, &ios_system_ui::request_operation(operation).map_err(io::Error::other)?)
    }
    pub(crate) fn dispatch_ui(&mut self, id: RequestId, operation: &UiOperation) -> io::Result<UiRequest> {
        self.validate_ui(&id, operation)?;
        if self.contains(&id) {
            return Err(io::Error::other(
                "system UI intent cannot be dispatched again",
            ));
        }
        let request = UiRequest {
            generation: self.state.runner.generation.clone(),
            request_id: self.state.entries.len() as u64 + 1,
            operation: operation.clone(),
        };
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id,
            request: request.clone(),
            completion: None,
        });
        self.publish(state)?;
        Ok(request)
    }
    pub fn complete(&mut self, id: &RequestId, completion: UiCompletion) -> io::Result<()> {
        let mut state = self.state.clone();
        let row = state
            .entries
            .iter_mut()
            .find(|r| &r.id == id)
            .ok_or_else(|| io::Error::other("unknown system UI request"))?;
        if row
            .completion
            .as_ref()
            .is_some_and(|old| old != &completion)
        {
            return Err(io::Error::other("system result changed"));
        }
        if row.completion.as_ref() == Some(&completion) { return Ok(()); }
        row.completion = Some(completion);
        self.publish(state)
    }
    pub fn invalidate(&mut self, id: &RequestId) -> io::Result<()> {
        self.ensure_resolved()?;
        if self.contains(id) {
            return Err(io::Error::other("app/URL ID belongs to system UI"));
        }
        if self.state.invalidations.iter().any(|r| &r.id == id) {
            return Ok(());
        }
        let mut state = self.state.clone();
        state.invalidations.push(Invalidation {
            id: id.clone(),
            after_entries: state.entries.len(),
        });
        self.publish(state)
    }
}
pub fn read_completion(
    target: &Path,
    owner: &Owner,
    runner: &UiIdentity,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<UiCompletion>> {
    read_ui_completion(target,owner,runner,id,&ios_system_ui::request_operation(operation).map_err(io::Error::other)?)
}
pub(crate) fn read_ui_completion(target:&Path, owner:&Owner, runner:&UiIdentity, id:&RequestId,
    operation:&UiOperation) -> io::Result<Option<UiCompletion>> {
    read_at(&target.join("system-ui/requests.json"), &client_directory(target),
        &target.join("session/worker/system-ui/results"), owner, runner, id, operation)
}
pub(crate) fn read_at<O: Consumer>(state_path: &Path, client: &Path, results: &Path,
    owner: &O, runner: &UiIdentity, id: &RequestId, operation: &UiOperation) -> io::Result<Option<UiCompletion>> {
    let Some(state): Option<State<O>> = load(state_path)? else {
        return Ok(None);
    };
    if &state.owner != owner || &state.runner != runner {
        return Err(io::Error::other("system UI saved owner differs"));
    }
    state.cursor()?;
    let Some(row) = state.entries.iter().find(|r| &r.id == id) else {
        return Ok(None);
    };
    if &row.request.operation != operation
    {
        return Err(io::Error::other("system UI saved request differs"));
    }
    let completion = if let Some(c) = &row.completion {
        c.clone()
    } else {
        let path = results.join(format!("{}.json", row.request.request_id));
        let Some(reply): Option<crate::ios::UiReply> = load(&path)? else {
            return Ok(None);
        };
        let crate::ios::UiReply::Completed { completion } = reply else {
            return Ok(None);
        };
        *completion
    };
    completion
        .validate_system(runner, &row.request)
        .map_err(io::Error::other)?;
    crate::ios_receipt::verify_system(client, runner, &completion)?;
    Ok(Some(completion))
}

#[cfg(test)]
#[path = "ios_system_store_tests.rs"]
mod tests;
