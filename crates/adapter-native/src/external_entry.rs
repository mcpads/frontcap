//! One OS dispatch under an existing live target owner. Saved records never grant ownership.
use frontcap_core::{
    canonical::{canonical_of, sha256_hex},
    ids::RequestId,
    journal::{Begin, Journal, RequestState},
    native::external_entry::{Accepted, Request, Target},
    state::{load, save_atomic},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{io, path::{Path, PathBuf}};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner { target: Target, generation: String }

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Accepted { acceptance: Accepted },
    Failed { error: String },
}

/// This journal supplements, and does not replace, the caller's live device/session lease.
pub struct Dispatches {
    directory: PathBuf,
    owner: Owner,
    journal: Journal,
    _lock: crate::store::RequestLease,
    poisoned: bool,
    budget: Option<WriteBudget>,
}


impl Dispatches {
    pub fn create(directory: &Path, target: Target, generation: String) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(directory, target, generation, budget.as_ref())
    }
    pub fn create_with_budget(directory: &Path, target: Target, generation: String, budget: Option<&WriteBudget>) -> io::Result<Self> {
        Request { target: target.clone(), generation: generation.clone(), url: "check:owner".into() }
            .validate().map_err(io::Error::other)?;
        std::fs::create_dir(directory)?;
        let lock = crate::store::lock(directory).map_err(io::Error::other)?;
        let owner = Owner { target, generation };
        match budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &directory.join("owner.json"), &owner)?,
            None => save_atomic(&directory.join("owner.json"), &owner)?,
        }
        let journal = Journal::open(&directory.join("requests.jsonl")).map_err(io::Error::other)?;
        Ok(Self { directory: directory.into(), owner, journal, _lock: lock, poisoned: false, budget: budget.cloned() })
    }

    /// A cached acceptance is returned without calling `send`, including after the device stops.
    /// An uncertain command or publication blocks new dispatches; there is no automatic retry.
    pub fn dispatch(&mut self, id: &RequestId, request: Request,
        send: impl FnOnce(&Request) -> io::Result<()>) -> io::Result<Accepted> {
        request.validate().map_err(io::Error::other)?;
        if self.owner.target != request.target || self.owner.generation != request.generation {
            return Err(io::Error::other("external entry owner changed"));
        }
        if self.poisoned { return Err(io::Error::other("external entry publication uncertain")); }
        let hash = sha256_hex(&canonical_of(&request));
        if let Some(binding) = self.journal.argument_hash(id) {
            if binding != hash { return Err(io::Error::other("external entry request ID conflict")); }
            return match self.journal.state(id) {
                Some(RequestState::Completed(value)) => decode(value.clone(), &request),
                _ => Err(io::Error::other("external entry unresolved; never redispatch")),
            };
        }
        if !self.journal.unresolved().is_empty() {
            return Err(io::Error::other("earlier external entry unresolved"));
        }
        // Acceptance only echoes this validated request. Admit its exact completion before OS
        // dispatch, so concurrent ordinary writers cannot consume that payload allowance later.
        let accepted = Accepted::new(request.clone());
        let completion = serde_json::to_value(&accepted)?;
        if let Some(budget) = &self.budget {
            budget.charge(Class::Ordinary, Journal::admission_bytes(id, &hash)
                + Journal::completion_bytes(id, &completion))?;
        }
        self.poisoned = true;
        let begun = self.journal.begin(id, &hash).map_err(io::Error::other)?;
        if begun != Begin::New { return Err(io::Error::other("external entry intent already exists")); }
        self.journal.dispatched(id).map_err(io::Error::other)?;
        self.poisoned = false;
        send(&request)?;
        self.poisoned = true;
        self.journal.complete(id, completion).map_err(io::Error::other)?;
        self.poisoned = false;
        Ok(accepted)
    }

    pub fn directory(&self) -> &Path { &self.directory }
    pub(crate) fn contains(&self, id: &RequestId) -> bool { self.journal.state(id).is_some() }
    pub(crate) fn ensure_resolved(&self) -> io::Result<()> {
        if self.poisoned || !self.journal.unresolved().is_empty() {
            Err(io::Error::other("external entry unresolved; retire the target"))
        } else { Ok(()) }
    }
}

fn decode(value: serde_json::Value, request: &Request) -> io::Result<Accepted> {
    let result: Accepted = serde_json::from_value(value)?;
    if result.request != *request { return Err(io::Error::other("external entry receipt mismatch")); }
    Ok(result)
}

/// Offline lookup reads complete records only. It cannot repair, dispatch or confirm a URL.
pub(crate) fn has_request(directory: &Path, id: &RequestId) -> io::Result<bool> {
    if !directory.join("owner.json").exists() {return Ok(false);}
    Ok(Journal::read_record(&directory.join("requests.jsonl"), id).map_err(io::Error::other)?.is_some())
}

pub fn read_accepted(directory: &Path, id: &RequestId, request: &Request) -> io::Result<Option<Accepted>> {
    request.validate().map_err(io::Error::other)?;
    let owner: Owner = load(&directory.join("owner.json"))?
        .ok_or_else(|| io::Error::other("external entry owner unavailable"))?;
    if owner.target != request.target || owner.generation != request.generation {
        return Err(io::Error::other("external entry owner changed"));
    }
    match Journal::read_record(&directory.join("requests.jsonl"), id).map_err(io::Error::other)? {
        Some((hash, state)) => {
            if hash != sha256_hex(&canonical_of(request)) {
                return Err(io::Error::other("external entry request ID conflict"));
            }
            match state { RequestState::Completed(value) => decode(value, request).map(Some), _ => Ok(None) }
        }
        None => Ok(None),
    }
}

/// ADB joins shell arguments. Quote the URL even though the host process uses argv directly.
pub(crate) fn android_arguments(package: &str, url: &str) -> Vec<String> {
    ["shell", "am", "start", "-W", "-a", "android.intent.action.VIEW", "-d"]
        .into_iter().map(str::to_owned)
        .chain([format!("'{}'", url.replace('\'', "'\"'\"'")), "-p".into(), package.into()]).collect()
}

pub(crate) fn android_accepted(output: &str, package: &str, require_new: bool) -> io::Result<()> {
    // `am` can exit zero while printing an error; process exit alone is not OS acceptance.
    let statuses: Vec<_> = output.lines().filter_map(|s| s.trim().strip_prefix("Status: ")).collect();
    let activities: Vec<_> = output.lines().filter_map(|s| s.trim().strip_prefix("Activity: ")).collect();
    // Android 15 reports its visibility-wait timeout separately from successful start acceptance.
    // Cold entry permits that receipt, but cannot accept an existing/task-to-front disposition.
    let accepted_status = statuses == ["ok"] || (require_new && statuses == ["timeout"]);
    let rejected = output.lines().map(str::trim).any(|s| s.starts_with("Error:")
        || (require_new && s.starts_with("Warning:")));
    if !accepted_status || rejected || activities.len() != 1 || !activities[0].starts_with(&format!("{package}/")) {
        return Err(io::Error::other("Android URL acceptance was not established"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "external_entry_tests.rs"]
mod tests;
