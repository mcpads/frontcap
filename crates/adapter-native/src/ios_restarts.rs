//! Retain UIKit app epochs and recover original replacement results without adopting a process.
use frontcap_core::{
    ids::RequestId,
    native::{
        ios_lifecycle::{Installation, Replacement},
        ios_request::Owner,
        ios_ui::{UiCompletion, UiOperation, UiOutcome, UiRequest},
    },
    state::{load, save_atomic, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

fn restart_url(request: &UiRequest) -> Option<&str> {
    match &request.operation {
        UiOperation::Restart { url, .. } => url.as_deref(),
        _ => None,
    }
}
fn entry_acceptance(
    owner: &Owner,
    bundle: &str,
    request: &UiRequest,
) -> io::Result<Option<frontcap_core::native::external_entry::Accepted>> {
    frontcap_core::native::ios_lifecycle::restart_entry(owner, bundle, restart_url(request))
        .map(|r| r.map(frontcap_core::native::external_entry::Accepted::new))
        .map_err(io::Error::other)
}
pub(crate) fn inspect_installation(
    target: &mut crate::xctest::Xctest,
    bundle: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Installation> {
    let app = target.installed_container(bundle, "app", deadline, cancelled)?;
    let data = target.app_container(bundle, deadline, cancelled)?;
    let meta = std::fs::symlink_metadata(&data)?;
    if !meta.is_dir() {
        return Err(io::Error::other("UIKit data container changed"));
    }
    let (directories, paths) = crate::products::inventory(&app)?;
    let mut files = BTreeMap::new();
    for path in paths {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other("UIKit installation inspection expired"));
        }
        files.insert(
            path.clone(),
            crate::products::read_file(&app.join(path), None)?,
        );
    }
    Ok(Installation {
        bundle: bundle.into(),
        app,
        data,
        data_device: meta.dev(),
        data_inode: meta.ino(),
        app_sha256: frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(
            &(directories, files),
        )),
    })
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: RequestId,
    request: UiRequest,
    completion: Option<Replacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admission_sha256: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    initial: Owner,
    installation: Installation,
    entries: Vec<Entry>,
}
impl State {
    fn validate(&self) -> io::Result<()> {
        self.initial.validate().map_err(io::Error::other)?;
        self.installation
            .validate(&self.initial)
            .map_err(io::Error::other)?;
        let mut previous = &self.initial;
        let mut ids = HashSet::new();
        let mut generations = HashSet::from([&previous.app.generation]);
        let mut number = 1;
        for (index, entry) in self.entries.iter().enumerate() {
            entry
                .request
                .validate(&previous.runner)
                .map_err(io::Error::other)?;
            if !ids.insert(&entry.id)
                || entry.request.request_id <= number
                || entry.request.operation
                    != (UiOperation::Restart {
                        app: previous.app.clone(),
                        url: restart_url(&entry.request).map(str::to_owned),
                    })
            {
                return Err(io::Error::other("UIKit restart binding differs"));
            }
            let url = restart_url(&entry.request);
            frontcap_core::native::ios_lifecycle::restart_entry(
                previous,
                &self.installation.bundle,
                url,
            )
            .map_err(io::Error::other)?;
            if url.is_some() != entry.admission_sha256.is_some()
                || entry.admission_sha256.as_ref().is_some_and(|s| {
                    s.len() != 64
                        || !s
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
            {
                return Err(io::Error::other("UIKit entry admission binding differs"));
            }
            number = entry.request.request_id;
            if let Some(result) = &entry.completion {
                result.validate().map_err(io::Error::other)?;
                if &result.previous != previous
                    || result.installation != self.installation
                    || result.completion.request != entry.request
                    || !generations.insert(&result.owner.app.generation)
                {
                    return Err(io::Error::other("UIKit restart lineage differs"));
                }
                previous = &result.owner;
            } else if index + 1 != self.entries.len() {
                return Err(io::Error::other(
                    "UIKit restart follows an unresolved effect",
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
    pub(crate) fn create(
        root: &Path,
        initial: Owner,
        installation: Installation,
    ) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        Self::create_with_budget(root, initial, installation, budget.as_ref())
    }
    fn create_with_budget(root: &Path, initial: Owner, installation: Installation, budget: Option<&WriteBudget>) -> io::Result<Self> {
        let state = State {
            initial,
            installation,
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
    pub(crate) fn ensure_resolved(&self) -> io::Result<()> {
        if self.poisoned || self.state.entries.iter().any(|e| e.completion.is_none()) {
            Err(io::Error::other(
                "UIKit replacement unresolved; no new dispatch authority",
            ))
        } else {
            Ok(())
        }
    }
    fn publish(&mut self, state: State) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("UIKit replacement storage uncertain"));
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
    pub(crate) fn dispatch(
        &mut self,
        id: RequestId,
        request: UiRequest,
        before: Installation,
        admission: Option<&crate::ios_external_entry::Admission>,
    ) -> io::Result<()> {
        self.ensure_resolved()?;
        if self.contains(&id) || before != self.state.installation {
            return Err(io::Error::other("UIKit restart ID or installation changed"));
        }
        let admission_bytes = admission.map(frontcap_core::canonical::canonical_of);
        let admission_sha256 = admission_bytes.as_ref().map(|bytes| frontcap_core::canonical::sha256_bytes_hex(bytes.as_bytes()));
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id: id.clone(),
            request,
            completion: None,
            admission_sha256,
        });
        state.validate()?;
        let bytes = frontcap_core::canonical::canonical_of(&state);
        if let Some(budget) = &self.budget {
            budget.charge(Class::Ordinary, (bytes.len() + admission_bytes.as_ref().map_or(0, String::len)) as u64)?;
        }
        self.poisoned = true;
        let attempt = self.root.join("restarts").join(id.as_str());
        std::fs::create_dir(&attempt)?;
        if let Some(admission) = admission_bytes {
            save_atomic_bytes(&attempt.join("admission.json"), admission.as_bytes())?;
        }
        self.persist(state, &bytes)
    }
    pub(crate) fn observe_completion(
        &self,
        id: &RequestId,
        completion: UiCompletion,
        after: Installation,
    ) -> io::Result<Replacement> {
        let entry = self
            .state
            .entries
            .last()
            .ok_or_else(|| io::Error::other("missing UIKit restart intent"))?;
        if &entry.id != id
            || entry.completion.is_some()
            || entry.request != completion.request
            || after != self.state.installation
        {
            return Err(io::Error::other(
                "UIKit replacement installation or intent differs",
            ));
        }
        let UiOutcome::Restarted { app, .. } = &completion.outcome else {
            return Err(io::Error::other("missing UIKit replacement app"));
        };
        let previous = self.state.current().clone();
        let owner = Owner {
            app: app.clone(),
            ..previous.clone()
        };
        let entry = entry_acceptance(&previous, &after.bundle, &completion.request)?;
        let result = Replacement {
            entry,
            previous,
            owner,
            installation: after,
            completion,
        };
        result.validate().map_err(io::Error::other)?;
        if result.owner.app.generation == self.state.initial.app.generation
            || self
                .state
                .entries
                .iter()
                .filter_map(|e| e.completion.as_ref())
                .any(|r| r.owner.app.generation == result.owner.app.generation)
        {
            return Err(io::Error::other(
                "UIKit replacement reused an app generation",
            ));
        }
        let path = self.root.join("restarts").join(id.as_str()).join("installation-after.json");
        match &self.budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, &path, &result.installation)?,
            None => save_atomic(&path, &result.installation)?,
        }
        Ok(result)
    }
    pub(crate) fn complete(&mut self, id: &RequestId, result: Replacement) -> io::Result<()> {
        let mut state = self.state.clone();
        let entry = state
            .entries
            .last_mut()
            .ok_or_else(|| io::Error::other("missing UIKit restart intent"))?;
        if &entry.id != id
            || entry.completion.is_some()
            || read_original(&self.root, &self.state, entry, &result.previous)?.as_ref()
                != Some(&result)
        {
            return Err(io::Error::other(
                "UIKit completion differs from original replacement evidence",
            ));
        }
        entry.completion = Some(result);
        self.publish(state)
    }
}
fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| io::Error::other("missing UIKit lineage evidence"))
}
pub fn epoch_directory(root: &Path, generation: &str) -> PathBuf {
    root.join("epochs").join(generation)
}
fn owner_directory(root: &Path, state: &State, owner: &Owner) -> PathBuf {
    if owner == &state.initial {
        root.into()
    } else {
        epoch_directory(root, &owner.app.generation)
    }
}
fn read_original(
    root: &Path,
    state: &State,
    entry: &Entry,
    previous: &Owner,
) -> io::Result<Option<Replacement>> {
    if let Some(expected) = &entry.admission_sha256 {
        let bytes = std::fs::read(
            root.join("restarts")
                .join(entry.id.as_str())
                .join("admission.json"),
        )?;
        if frontcap_core::canonical::sha256_bytes_hex(&bytes) != *expected {
            return Err(io::Error::other("UIKit entry admission changed"));
        }
    }
    let directory = owner_directory(root, state, previous);
    let Some(completion): Option<UiCompletion> = load(
        &directory
            .join("ui-client/restarts")
            .join(format!("{}.json", entry.request.request_id)),
    )?
    else {
        return Ok(None);
    };
    if completion.request != entry.request {
        return Err(io::Error::other(
            "UIKit replacement proof belongs to another request",
        ));
    }
    crate::ios_receipt::verify_restart(&directory, previous, &completion)?;
    let Some(installation): Option<Installation> = load(
        &root
            .join("restarts")
            .join(entry.id.as_str())
            .join("installation-after.json"),
    )?
    else {
        return Ok(None);
    };
    if installation != state.installation {
        return Err(io::Error::other("UIKit replacement changed installation"));
    }
    let UiOutcome::Restarted { app, .. } = &completion.outcome else {
        return Err(io::Error::other("missing UIKit replacement outcome"));
    };
    let result = Replacement {
        entry: entry_acceptance(previous, &installation.bundle, &completion.request)?,
        previous: previous.clone(),
        owner: Owner {
            app: app.clone(),
            ..previous.clone()
        },
        installation,
        completion,
    };
    result.validate().map_err(io::Error::other)?;
    Ok(Some(result))
}
fn read(root: &Path) -> io::Result<Option<State>> {
    let Some(state): Option<State> = load(&root.join("restarts/history.json"))? else {
        return Ok(None);
    };
    state.validate()?;
    if required::<Owner>(&root.join("request-owner.json"))? != state.initial {
        return Err(io::Error::other(
            "UIKit history differs from initial acquisition",
        ));
    }
    let mut previous = &state.initial;
    for entry in &state.entries {
        if let Some(result) = &entry.completion {
            if read_original(root, &state, entry, previous)?.as_ref() != Some(result) {
                return Err(io::Error::other(
                    "UIKit original replacement evidence changed",
                ));
            }
            previous = &result.owner;
        }
    }
    Ok(Some(state))
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
    let mut previous = &state.initial;
    let mut generations = HashSet::from([&previous.app.generation]);
    for entry in &state.entries {
        if &entry.id == id {
            if previous.app.generation != generation || restart_url(&entry.request) != url {
                return Err(io::Error::other(
                    "UIKit restart generation or URL conflicts",
                ));
            }
            let result = read_original(root, &state, entry, previous)?;
            if result
                .as_ref()
                .is_some_and(|r| generations.contains(&r.owner.app.generation))
            {
                return Err(io::Error::other("UIKit replacement reused a generation"));
            }
            return Ok(result);
        }
        if let Some(result) = &entry.completion {
            previous = &result.owner;
            generations.insert(&previous.app.generation);
        }
    }
    Ok(None)
}
pub fn epochs(root: &Path) -> io::Result<Vec<(Owner, PathBuf)>> {
    let Some(state) = read(root)? else {
        return Ok(vec![(
            required(&root.join("request-owner.json"))?,
            root.into(),
        )]);
    };
    let mut result = vec![(state.initial.clone(), root.into())];
    for entry in state.entries {
        if let Some(completion) = entry.completion {
            let directory = epoch_directory(root, &completion.owner.app.generation);
            if required::<Owner>(&directory.join("request-owner.json"))? != completion.owner {
                return Err(io::Error::other("UIKit epoch differs from its owner"));
            }
            result.push((completion.owner, directory));
        }
    }
    Ok(result)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { replacement: Box<Replacement> },
    Failed { error: String },
}

pub fn request_epoch(root: &Path, id: &RequestId) -> io::Result<Option<(Owner, PathBuf)>> {
    let mut found = None;
    for (owner, directory) in epochs(root)? {
        if directory.join("requests/requests.json").exists()
            && crate::ios_store::RequestStore::has_request(&directory.join("requests"), &owner, id)
                .map_err(io::Error::other)?
        {
            if found.is_some() {
                return Err(io::Error::other(
                    "UIKit request ID occurs in multiple epochs",
                ));
            }
            found = Some((owner, directory));
        }
    }
    Ok(found)
}

#[cfg(test)]
#[path = "ios_restarts_tests.rs"]
mod budget_tests;
