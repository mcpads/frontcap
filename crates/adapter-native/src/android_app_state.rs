//! Same-process visibility history on the Android View session's existing ownership lease.
use frontcap_core::{
    ids::RequestId,
    native::{android_app::Owner, android_lifecycle::Transition, app_state::State},
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io, path::Path};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { transition: Box<Transition> },
    Failed { error: String },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    state: State,
    completion: Option<Transition>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    owner: Owner,
    entries: Vec<Entry>,
}

fn read(root: &Path, epoch: &Path, owner: &Owner) -> io::Result<Option<History>> {
    let Some(history): Option<History> = load(&epoch.join("app-state/history.json"))? else {
        return Ok(None);
    };
    if history.owner != *owner {
        return Err(io::Error::other("Android visibility owner differs"));
    }
    owner.validate().map_err(io::Error::other)?;
    let mut state = State::Foreground;
    let mut previous_process = None;
    let mut ids = HashSet::new();
    for (index, entry) in history.entries.iter().enumerate() {
        if !ids.insert(&entry.id) || entry.state == state || index >= 128 {
            return Err(io::Error::other("invalid Android visibility history"));
        }
        if let Some(completion) = &entry.completion {
            completion.validate().map_err(io::Error::other)?;
            if completion.owner != *owner
                || completion.requested != entry.state
                || previous_process.is_some_and(|previous| previous != &completion.process_before)
                || crate::device::read_view_transition(
                    &root.join("device"),
                    &entry.id,
                    owner,
                    entry.state,
                )?
                .as_ref()
                    != Some(completion)
            {
                return Err(io::Error::other(
                    "Android visibility source evidence changed",
                ));
            }
            state = entry.state;
            previous_process = Some(&completion.process_after);
        } else if index + 1 != history.entries.len() {
            return Err(io::Error::other(
                "Android visibility follows unresolved transition",
            ));
        }
    }
    Ok(Some(history))
}
pub(crate) fn create(epoch: &Path, owner: &Owner) -> io::Result<()> {
    std::fs::create_dir(epoch.join("app-state"))?;
    save_evidence(
        &epoch.join("app-state/history.json"),
        &History {
            owner: owner.clone(),
            entries: vec![],
        },
    )
}
pub fn has_request(root: &Path, id: &RequestId) -> io::Result<bool> {
    for (owner, epoch) in crate::android_restarts::epochs(root)? {
        if read(root, &epoch, &owner)?.is_some_and(|h| h.entries.iter().any(|e| &e.id == id)) {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Cached results retain their epoch; this function neither adopts a process nor repairs history.
pub fn read_completion(
    root: &Path,
    id: &RequestId,
    generation: &str,
    state: State,
) -> io::Result<Option<Transition>> {
    let mut found = None;
    let mut matched = false;
    for (owner, epoch) in crate::android_restarts::epochs(root)? {
        let Some(history) = read(root, &epoch, &owner)? else {
            continue;
        };
        let Some((index, entry)) = history
            .entries
            .iter()
            .enumerate()
            .find(|(_, e)| &e.id == id)
        else {
            continue;
        };
        if matched || generation != owner.generation || state != entry.state {
            return Err(io::Error::other(
                "Android visibility request binding differs",
            ));
        }
        matched = true;
        let result = crate::device::read_view_transition(&root.join("device"), id, &owner, state)?;
        if let Some(result) = result {
            if index > 0
                && history.entries[index - 1]
                    .completion
                    .as_ref()
                    .is_none_or(|previous| previous.process_after != result.process_before)
            {
                return Err(io::Error::other(
                    "Android host return differs from previous process",
                ));
            }
            found = Some(result);
        }
    }
    Ok(found)
}
pub(crate) fn require_foreground(root: &Path) -> io::Result<()> {
    let (owner, epoch) = crate::android_restarts::current(root)?;
    if let Some(history) = read(root, &epoch, &owner)?
        && history
            .entries
            .last()
            .is_some_and(|e| e.completion.is_none() || e.state != State::Foreground)
    {
        return Err(io::Error::other(
            "Android app work requires confirmed foreground",
        ));
    }
    Ok(())
}
pub(crate) fn available(root: &Path) -> io::Result<bool> {
    let (owner, epoch) = crate::android_restarts::current(root)?;
    Ok(read(root, &epoch, &owner)?.is_some())
}
pub fn validate(root: &Path, id: &RequestId, generation: &str, state: State) -> io::Result<()> {
    if has_request(root, id)? {
        return read_completion(root, id, generation, state)?
            .map(|_| ())
            .ok_or_else(|| io::Error::other("Android host transition unresolved; never replay"));
    }
    let (owner, epoch) = crate::android_restarts::current(root)?;
    let history = read(root, &epoch, &owner)?
        .ok_or_else(|| io::Error::other("Android app lifecycle unavailable"))?;
    let previous = history.entries.last();
    if owner.generation != generation
        || history.entries.len() >= 128
        || previous.is_some_and(|e| e.completion.is_none())
        || previous.map(|e| e.state).unwrap_or(State::Foreground) == state
    {
        return Err(io::Error::other(
            "Android visibility requires current resolved owner and another state",
        ));
    }
    for (owner, epoch) in crate::android_restarts::epochs(root)? {
        if crate::android_restarts::has_request(root, id)?
            || crate::external_entry::has_request(
                &crate::android_restarts::device_epoch(root, &owner)?.join("external-entry"),
                id,
            )?
            || epoch.join("views").join(id.as_str()).exists()
            || crate::store::RequestStore::has_request(
                &epoch.join("requests"),
                &owner.identity(),
                &owner.generation,
                &owner.source_sha256,
                id,
            )
            .map_err(io::Error::other)?
        {
            return Err(io::Error::other(
                "Android visibility ID already names another operation",
            ));
        }
    }
    crate::store::RequestStore::validate_replacement(
        &epoch.join("requests"),
        &owner.identity(),
        &owner.generation,
        &owner.source_sha256,
        id,
    )
    .map_err(io::Error::other)
}
pub(crate) fn dispatch(
    root: &Path,
    id: &RequestId,
    generation: &str,
    state: State,
) -> io::Result<Owner> {
    validate(root, id, generation, state)?;
    if has_request(root, id)? {
        return Err(io::Error::other("Android host request already dispatched"));
    }
    let (owner, epoch) = crate::android_restarts::current(root)?;
    let mut history = read(root, &epoch, &owner)?.unwrap();
    history.entries.push(Entry {
        id: id.clone(),
        state,
        completion: None,
    });
    publish_dispatch(&epoch, &history, WriteBudget::from_environment()?.as_ref())?;
    Ok(owner)
}
pub(crate) fn complete(root: &Path, id: &RequestId, transition: &Transition) -> io::Result<()> {
    let (owner, epoch) = crate::android_restarts::current(root)?;
    let mut history = read(root, &epoch, &owner)?
        .ok_or_else(|| io::Error::other("Android visibility history absent"))?;
    let entry = history
        .entries
        .last_mut()
        .ok_or_else(|| io::Error::other("Android visibility intent absent"))?;
    if &entry.id != id
        || entry.completion.is_some()
        || read_completion(root, id, &owner.generation, entry.state)?.as_ref() != Some(transition)
    {
        return Err(io::Error::other(
            "Android visibility completion differs from intent",
        ));
    }
    entry.completion = Some(transition.clone());
    save_evidence(&epoch.join("app-state/history.json"), &history)
}

fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    match WriteBudget::from_environment()? {
        Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
        None => save_atomic(path, value),
    }
}

fn publish_dispatch(epoch: &Path, history: &History, budget: Option<&WriteBudget>) -> io::Result<()> {
    let bytes = frontcap_core::canonical::canonical_of(history);
    let invalidated = frontcap_core::canonical::canonical_of(&Option::<RequestId>::None);
    if let Some(budget) = budget {
        budget.charge(Class::Ordinary, (bytes.len() + invalidated.len()) as u64)?;
    }
    save_atomic_bytes(&epoch.join("app-state/history.json"), bytes.as_bytes())?;
    save_atomic_bytes(&epoch.join("latest-view.json"), invalidated.as_bytes())
}

#[cfg(test)]
#[path = "android_app_state_tests.rs"]
mod tests;
