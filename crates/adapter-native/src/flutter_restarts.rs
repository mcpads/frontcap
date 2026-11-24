//! Same-install lineage and original-result recovery. Reading it cannot adopt a worker.
use crate::{
    flutter_app::{Acquired, Restarted},
    flutter_session::Ready,
};
use frontcap_core::{
    ids::RequestId,
    native::flutter_lifecycle::Replacement,
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    command_number: u64,
    completion: Option<Restarted>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cold: Option<cold::Cold>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    initial: Ready,
    application: Acquired,
    entries: Vec<Entry>,
}
impl State {
    fn validate(&self) -> io::Result<()> {
        self.application.validate(&self.initial)?;
        let mut ready = &self.initial;
        let mut app = &self.application;
        let mut ids = HashSet::new();
        let mut generations = HashSet::from([&ready.owner.generation]);
        let mut previous_number = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            if !ids.insert(&entry.id)
                || entry.generation != ready.owner.generation
                || entry.command_number <= previous_number
            {
                return Err(io::Error::other("invalid Flutter restart sequence"));
            }
            previous_number = entry.command_number;
            if let Some(cold) = &entry.cold {
                cold.validate(entry, ready, app, &mut ids, &mut previous_number)?;
            } else {
                frontcap_core::native::flutter_lifecycle::restart_entry(&ready.owner, ready.external_url_target.as_ref(), entry.url.as_deref())
                    .map_err(io::Error::other)?;
            }
            if let Some(completion) = &entry.completion {
                validate_entry(completion, entry)?;
                completion.validate(app, ready)?;
                if !generations.insert(&completion.ready.owner.generation) {
                    return Err(io::Error::other("Flutter generation reused"));
                }
                ready = &completion.ready;
                app = &completion.current;
            } else if index + 1 != self.entries.len() {
                return Err(io::Error::other(
                    "Flutter restart follows unresolved effect",
                ));
            }
        }
        Ok(())
    }
    fn current(&self) -> (&Ready, &Acquired) {
        self.entries
            .iter()
            .rev()
            .find_map(|e| e.completion.as_ref())
            .map(|c| (&c.ready, &c.current))
            .unwrap_or((&self.initial, &self.application))
    }
}
pub struct History {
    root: PathBuf,
    state: State,
    _lock: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}
impl History {
    pub fn create(root: &Path, initial: Ready, application: Acquired) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(root, initial, application, budget.as_ref())
    }
    pub fn create_with_budget(root: &Path, initial: Ready, application: Acquired, budget: Option<&WriteBudget>) -> io::Result<Self> {
        let state = State {
            initial,
            application,
            entries: vec![],
        };
        state.validate()?;
        std::fs::create_dir(root.join("restarts"))?;
        let lock = crate::store::lock(&root.join("restarts")).map_err(io::Error::other)?;
        match budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &root.join("restarts/history.json"), &state)?,
            None => save_atomic(&root.join("restarts/history.json"), &state)?,
        }
        Ok(Self {
            root: root.into(),
            state,
            _lock: lock,
            poisoned: false,
            budget: budget.cloned(),
        })
    }
    pub fn contains(&self, id: &RequestId) -> bool {
        self.state.entries.iter().any(|e| &e.id == id || e.cold.as_ref().is_some_and(|c| c.contains(id)))
    }
    pub fn current_application(&self) -> &Acquired { self.state.current().1 }
    pub fn ensure_resolved(&self) -> io::Result<()> {
        if self.poisoned || self.state.entries.iter().any(|e| e.completion.is_none()) {
            Err(io::Error::other(
                "Flutter replacement unresolved; no dispatch authority",
            ))
        } else {
            Ok(())
        }
    }
    fn publish(&mut self, state: State) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("Flutter restart publication uncertain"));
        }
        state.validate()?;
        let bytes = frontcap_core::canonical::canonical_of(&state);
        if let Some(budget) = &self.budget { budget.charge(Class::Ordinary, bytes.len() as u64)?; }
        self.poisoned = true;
        save_atomic_bytes(&self.root.join("restarts/history.json"), bytes.as_bytes())?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    pub fn dispatch(
        &mut self,
        id: RequestId,
        generation: String,
        command_number: u64,
        url: Option<String>,
    ) -> io::Result<()> {
        self.ensure_resolved()?;
        if self.contains(&id) || self.state.current().0.owner.generation != generation {
            return Err(io::Error::other("Flutter restart ID or generation differs"));
        }
        let ready = self.state.current().0;
        frontcap_core::native::flutter_lifecycle::restart_entry(&ready.owner, ready.external_url_target.as_ref(), url.as_deref())
            .map_err(io::Error::other)?;
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id,
            generation,
            url,
            command_number,
            completion: None,
            cold: None,
        });
        self.publish(state)
    }
    pub fn validate_completion(&self, result: &Restarted) -> io::Result<Replacement> {
        let entry = self.state.entries.last().ok_or_else(|| io::Error::other("missing Flutter replacement intent"))?;
        validate_entry(result, entry)?;
        if result.ready.owner.generation == self.state.initial.owner.generation
            || self
                .state
                .entries
                .iter()
                .filter_map(|e| e.completion.as_ref())
                .any(|c| c.ready.owner.generation == result.ready.owner.generation)
        {
            return Err(io::Error::other("Flutter replacement reused a generation"));
        }
        let (ready, app) = self.state.current();
        result.validate(app, ready)
    }
    pub fn complete(&mut self, id: &RequestId, result: Restarted) -> io::Result<Replacement> {
        let replacement = self.validate_completion(&result)?;
        let mut state = self.state.clone();
        let entry = state
            .entries
            .last_mut()
            .ok_or_else(|| io::Error::other("missing Flutter restart intent"))?;
        if &entry.id != id || entry.completion.is_some() {
            return Err(io::Error::other("Flutter restart cannot complete twice"));
        }
        if entry.cold.is_some() { return Err(io::Error::other("cold entry requires its original readiness poll")); }
        if read_worker(&self.root, entry)?.as_ref() != Some(&result) {
            return Err(io::Error::other(
                "Flutter restart differs from original worker result",
            ));
        }
        entry.completion = Some(result);
        self.publish(state)?;
        Ok(replacement)
    }
}
fn read(root: &Path) -> io::Result<Option<State>> {
    let Some(state): Option<State> = load(&root.join("restarts/history.json"))? else {
        return Ok(None);
    };
    state.validate()?;
    let ready: Ready = required(&root.join("session/worker/ready.json"))?;
    let application: Acquired = required(&root.join("session/worker/application.json"))?;
    let owner: frontcap_core::native::flutter::Owner = required(&root.join("ready.json"))?;
    if state.initial != ready || state.application != application || owner != ready.owner {
        return Err(io::Error::other(
            "Flutter restart history differs from initial acquisition",
        ));
    }
    let mut current = (&state.initial, &state.application);
    for entry in &state.entries {
        if let Some(cold) = &entry.cold { cold.verify_worker(root, entry, current.0, current.1)?; }
        if let Some(saved) = &entry.completion
            && read_worker(root, entry)?.as_ref() != Some(saved)
        {
            return Err(io::Error::other("Flutter restart worker evidence changed"));
        }
        if let Some(saved) = &entry.completion { current = (&saved.ready, &saved.current); }
    }
    Ok(Some(state))
}
fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| io::Error::other("missing Flutter lineage evidence"))
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestartOperation {
    RestartApp,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    number: u64,
    timeout_ms: u64,
    operation: RestartOperation,
    generation: String,
    #[serde(default)]
    url: Option<String>,
}
fn validate_entry(result: &Restarted, entry: &Entry) -> io::Result<()> {
    if result.entry.as_ref().map(|e| e.request.url.as_str()) != entry.url.as_deref() {
        return Err(io::Error::other("Flutter replacement URL differs from original request"));
    }
    Ok(())
}
fn read_worker(root: &Path, entry: &Entry) -> io::Result<Option<Restarted>> {
    if entry.cold.is_some() { return cold::read_replacement(root, entry); }
    let directory = root
        .join("session/worker/commands")
        .join(entry.command_number.to_string());
    let Some(request): Option<WorkerRequest> = load(&directory.join("request.json"))? else {
        return Ok(None);
    };
    if request.number != entry.command_number
        || request.generation != entry.generation
        || request.url != entry.url
        || !(1..=120_000).contains(&request.timeout_ms)
        || !matches!(request.operation, RestartOperation::RestartApp)
    {
        return Err(io::Error::other("Flutter restart worker binding differs"));
    }
    let result = load(&directory.join("command.log"))?;
    if let Some(value) = &result { validate_entry(value, entry)?; }
    Ok(result)
}
pub fn read_completion(
    root: &Path,
    id: &RequestId,
    generation: &str,
    url: Option<&str>,
) -> io::Result<Option<Replacement>> {
    let Some(state) = read(root)? else {
        return Ok(None);
    };
    let mut ready = &state.initial;
    let mut app = &state.application;
    let mut generations = HashSet::from([&ready.owner.generation]);
    for entry in &state.entries {
        if &entry.id == id {
            if entry.cold.is_some() { return Err(io::Error::other("request belongs to two-phase cold entry")); }
            if entry.generation != generation || entry.url.as_deref() != url {
                return Err(io::Error::other("Flutter restart request conflicts"));
            }
            let result = read_worker(root, entry)?;
            if let Some(saved) = &entry.completion
                && result.as_ref() != Some(saved)
            {
                return Err(io::Error::other("Flutter restart result changed"));
            }
            return result
                .map(|r| {
                    if generations.contains(&r.ready.owner.generation) {
                        return Err(io::Error::other("Flutter restart reused a generation"));
                    }
                    r.validate(app, ready)
                })
                .transpose();
        }
        if let Some(completion) = &entry.completion {
            ready = &completion.ready;
            app = &completion.current;
            generations.insert(&ready.owner.generation);
        }
    }
    Ok(None)
}

#[path = "flutter_cold_history.rs"]
mod cold;
pub use cold::{read_entry, read_poll};
/// Read the source-bound original acquisition of this generation, without host queries.
pub(crate) fn application_at(root:&Path, owner:&frontcap_core::native::flutter::Owner) -> io::Result<Option<(Ready,Acquired)>> {
    let Some(state)=read(root)? else { return Ok(None); };
    if &state.initial.owner==owner { return Ok(Some((state.initial,state.application))); }
    Ok(state.entries.into_iter().filter_map(|entry| entry.completion)
        .find(|done| &done.ready.owner==owner).map(|done|(done.ready,done.current)))
}
/// Initial request storage is preserved in place; each replacement starts its own clock/basis store.
pub fn epoch_directory(root: &Path, generation: &str) -> PathBuf {
    root.join("epochs").join(generation)
}
pub fn epochs(root: &Path) -> io::Result<Vec<(Ready, PathBuf)>> {
    if let Some(state) = read(root)? {
        let mut epochs = vec![(state.initial, root.into())];
        for entry in state.entries {
            if let Some(completion) = entry.completion {
                let directory = epoch_directory(root, &completion.ready.owner.generation);
                let owner: frontcap_core::native::flutter::Owner =
                    required(&directory.join("ready.json"))?;
                if owner != completion.ready.owner {
                    return Err(io::Error::other("Flutter epoch owner differs"));
                }
                epochs.push((completion.ready, directory));
            }
        }
        Ok(epochs)
    } else {
        let ready: Ready = required(&root.join("session/worker/ready.json"))?;
        if required::<frontcap_core::native::flutter::Owner>(&root.join("ready.json"))?
            != ready.owner
        {
            return Err(io::Error::other("Flutter initial owner differs"));
        }
        Ok(vec![(ready, root.into())])
    }
}

#[cfg(test)]
#[path = "flutter_restarts_tests.rs"]
mod tests;
