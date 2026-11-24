//! Android View app epochs. Reading a receipt never acquires a replacement process.
use frontcap_core::{
    ids::RequestId,
    native::{android_app::Owner, android_lifecycle::Replacement, request::canonical_uuid},
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
    completion: Option<Replacement>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    initial: Owner,
    entries: Vec<Entry>,
}
impl State {
    fn validate(&self) -> io::Result<()> {
        self.initial.validate().map_err(io::Error::other)?;
        let mut owner = &self.initial;
        let mut previous: Option<&Replacement> = None;
        let mut ids = HashSet::new();
        let mut generations = HashSet::from([&owner.generation]);
        for (index, entry) in self.entries.iter().enumerate() {
            if !ids.insert(&entry.id) || entry.generation != owner.generation {
                return Err(io::Error::other(
                    "Android restart intent differs from lineage",
                ));
            }
            frontcap_core::native::android_lifecycle::restart_entry(owner, entry.url.as_deref()).map_err(io::Error::other)?;
            if let Some(result) = &entry.completion {
                result.validate().map_err(io::Error::other)?;
                if result.entry.as_ref().map(|e| e.request.url.as_str()) != entry.url.as_deref()
                    || &result.previous != owner
                    || !generations.insert(&result.owner.generation)
                    || previous.is_some_and(|p| {
                        p.installation != result.installation
                            || p.process != result.previous_process
                    })
                {
                    return Err(io::Error::other("Android replacement lineage changed"));
                }
                owner = &result.owner;
                previous = Some(result);
            } else if index + 1 != self.entries.len() {
                return Err(io::Error::other(
                    "Android restart follows an unresolved effect",
                ));
            }
        }
        Ok(())
    }
    fn current(&self) -> &Owner {
        self.entries
            .iter()
            .rev()
            .find_map(|e| e.completion.as_ref())
            .map(|r| &r.owner)
            .unwrap_or(&self.initial)
    }
}

pub(crate) struct History {
    root: PathBuf,
    state: State,
    _lock: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}
impl History {
    pub(crate) fn create(root: &Path, initial: Owner) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(root, initial, budget.as_ref())
    }
    fn create_with_budget(root: &Path, initial: Owner, budget: Option<&WriteBudget>) -> io::Result<Self> {
        let state = State {
            initial,
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
    pub(crate) fn contains(&self, id: &RequestId) -> bool {
        self.state.entries.iter().any(|e| &e.id == id)
    }
    pub(crate) fn dispatch(&mut self, id: &RequestId, generation: &str, url: Option<&str>) -> io::Result<()> {
        if self.poisoned
            || self.contains(id)
            || self.state.entries.iter().any(|e| e.completion.is_none())
        {
            return Err(io::Error::other(
                "Android replacement is unresolved or request ID reused",
            ));
        }
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id: id.clone(),
            generation: generation.into(),
            url: url.map(str::to_owned),
            completion: None,
        });
        self.publish(state)
    }
    pub(crate) fn complete(&mut self, id: &RequestId, result: Replacement) -> io::Result<()> {
        let mut state = self.state.clone();
        let entry = state
            .entries
            .last_mut()
            .ok_or_else(|| io::Error::other("missing Android restart intent"))?;
        if &entry.id != id
            || entry.completion.is_some()
            || crate::device::read_view_replacement(
                &self.root.join("device"),
                id,
                &entry.generation,
                entry.url.as_deref(),
            )?
            .as_ref()
                != Some(&result)
        {
            return Err(io::Error::other(
                "Android restart differs from original evidence",
            ));
        }
        entry.completion = Some(result);
        self.publish(state)
    }
    fn publish(&mut self, state: State) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("Android restart publication uncertain"));
        }
        state.validate()?;
        let bytes = frontcap_core::canonical::canonical_of(&state);
        if let Some(budget) = &self.budget { budget.charge(Class::Ordinary, bytes.len() as u64)?; }
        self.persist(state, &bytes)
    }
    fn persist(&mut self, state: State, bytes: &str) -> io::Result<()> {
        self.poisoned = true;
        save_atomic_bytes(&self.root.join("restarts/history.json"), bytes.as_bytes())?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
}
fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| io::Error::other("missing Android epoch evidence"))
}
fn read(root: &Path) -> io::Result<State> {
    let initial = required::<Owner>(&root.join("view-ready.json"))?;
    let state: State = load(&root.join("restarts/history.json"))?.unwrap_or(State {
        initial: initial.clone(),
        entries: vec![],
    });
    state.validate()?;
    if initial != state.initial {
        return Err(io::Error::other("Android history differs from acquisition"));
    }
    for entry in &state.entries {
        if let Some(result) = &entry.completion
            && crate::device::read_view_replacement(
                &root.join("device"),
                &entry.id,
                &entry.generation,
                entry.url.as_deref(),
            )?
            .as_ref()
                != Some(result)
        {
            return Err(io::Error::other(
                "Android original replacement evidence changed",
            ));
        }
    }
    Ok(state)
}
pub fn epoch_directory(root: &Path, generation: &str) -> io::Result<PathBuf> {
    if !canonical_uuid(generation) {
        return Err(io::Error::other("invalid Android generation"));
    }
    Ok(root.join("epochs").join(generation))
}
pub fn epochs(root: &Path) -> io::Result<Vec<(Owner, PathBuf)>> {
    let state = read(root)?;
    let mut result = vec![(state.initial, root.into())];
    for entry in state.entries {
        if let Some(completion) = entry.completion {
            let directory = epoch_directory(root, &completion.owner.generation)?;
            if required::<Owner>(&directory.join("view-ready.json"))? != completion.owner {
                return Err(io::Error::other("Android epoch differs from its owner"));
            }
            result.push((completion.owner, directory));
        }
    }
    Ok(result)
}
pub fn current(root: &Path) -> io::Result<(Owner, PathBuf)> {
    if read(root)?.entries.iter().any(|e| e.completion.is_none()) {
        return Err(io::Error::other(
            "Android replacement unresolved; no current authority",
        ));
    }
    epochs(root)?
        .pop()
        .ok_or_else(|| io::Error::other("missing Android epoch"))
}
pub fn read_completion(
    root: &Path,
    id: &RequestId,
    generation: &str,
    url: Option<&str>,
) -> io::Result<Option<Replacement>> {
    let state = read(root)?;
    let Some(entry) = state.entries.iter().find(|e| &e.id == id) else {
        return Ok(None);
    };
    if generation != entry.generation || url != entry.url.as_deref() {
        return Err(io::Error::other("Android restart generation or URL conflicts"));
    }
    let result = crate::device::read_view_replacement(&root.join("device"), id, generation, url)?;
    if let Some(result) = &result {
        let mut verified = state.clone();
        let e = verified.entries.iter_mut().find(|e| &e.id == id).unwrap();
        e.completion = Some(result.clone());
        verified.validate()?;
    }
    Ok(result)
}
pub fn validate_restart(root: &Path, id: &RequestId, generation: &str, url: Option<&str>) -> io::Result<()> {
    let state = read(root)?;
    if let Some(entry) = state.entries.iter().find(|e| &e.id == id) {
        if entry.generation == generation && entry.url.as_deref() == url && read_completion(root, id, generation, url)?.is_some() {
            return Ok(());
        }
        return Err(io::Error::other(
            "Android restart is conflicting or unresolved",
        ));
    }
    if state.entries.iter().any(|e| e.completion.is_none())
        || state.current().generation != generation
    {
        return Err(io::Error::other(
            "Android restart requires the current resolved generation",
        ));
    }
    crate::android_app_state::require_foreground(root)?;
    if crate::android_app_state::has_request(root, id)? {
        return Err(io::Error::other("Android restart ID already names a host transition"));
    }
    frontcap_core::native::android_lifecycle::restart_entry(state.current(), url).map_err(io::Error::other)?;
    for (owner, directory) in epochs(root)? {
        if crate::external_entry::has_request(
            &device_epoch(root, &owner)?.join("external-entry"),
            id,
        )? || directory.join("views").join(id.as_str()).exists()
            || crate::store::RequestStore::has_request(
                &directory.join("requests"),
                &owner.identity(),
                &owner.generation,
                &owner.source_sha256,
                id,
            )
            .map_err(io::Error::other)?
        {
            return Err(io::Error::other(
                "Android restart ID already names an operation",
            ));
        }
    }
    let (owner, directory) = current(root)?;
    crate::store::RequestStore::validate_replacement(
        &directory.join("requests"),
        &owner.identity(),
        &owner.generation,
        &owner.source_sha256,
        id,
    )
    .map_err(io::Error::other)
}
pub(crate) fn has_request(root: &Path, id: &RequestId) -> io::Result<bool> {
    Ok(read(root)?.entries.iter().any(|e| &e.id == id))
}
/// The device's bridge clients are separate from the session's caller request stores.
pub fn device_epoch(root: &Path, owner: &Owner) -> io::Result<PathBuf> {
    let initial: Owner = required(&root.join("view-ready.json"))?;
    if owner == &initial {
        Ok(root.join("device"))
    } else {
        epoch_directory(&root.join("device"), &owner.generation)
    }
}
pub fn request_epoch(root: &Path, id: &RequestId) -> io::Result<Option<(Owner, PathBuf)>> {
    let mut found = None;
    for (owner, directory) in epochs(root)? {
        if directory.join("requests/requests.json").exists()
            && crate::store::RequestStore::has_request(
                &directory.join("requests"),
                &owner.identity(),
                &owner.generation,
                &owner.source_sha256,
                id,
            )
            .map_err(io::Error::other)?
        {
            if found.is_some() {
                return Err(io::Error::other(
                    "Android request occurs in multiple epochs",
                ));
            }
            found = Some((owner, directory));
        }
    }
    Ok(found)
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { replacement: Box<Replacement> },
    Failed { error: String },
}

#[cfg(test)]
#[path = "android_restarts_tests.rs"]
mod budget_tests;
