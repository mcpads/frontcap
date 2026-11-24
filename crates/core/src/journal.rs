use crate::canonical::{canonical_of, sha256_hex, to_canonical_string};
use crate::ids::RequestId;
use serde::{Deserialize, Serialize};
use crate::lines::{Appender, LinesError, open_append};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub fn args_sha256(args: &serde_json::Value) -> String {
    sha256_hex(&to_canonical_string(args))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    New,
    Completed(serde_json::Value),
    NotApplied,
    Unresolved,
    Conflict,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error(transparent)]
    Lines(#[from] LinesError),
    #[error("line {line} of {path} is not a journal entry: {reason}")]
    Corrupt { path: String, line: usize, reason: String },
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("unknown request {0}")]
    UnknownRequest(RequestId),
    #[error("request {id} is not at step {expected}")]
    WrongStep { id: RequestId, expected: &'static str },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum Line {
    Intent { request_id: RequestId, args_sha256: String },
    DependentIntent { parent: RequestId, request_id: RequestId, args_sha256: String },
    Dispatched { request_id: RequestId },
    Completed { request_id: RequestId, result: serde_json::Value },
    ResolvedNotApplied { request_id: RequestId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestState {
    Intent,
    Dispatched,
    Completed(serde_json::Value),
    NotApplied,
}

pub struct Journal {
    path: PathBuf,
    out: Appender<File>,
    entries: HashMap<RequestId, (String, RequestState)>,
    last_completed: Option<RequestId>,
    parents: HashMap<RequestId, RequestId>,
    dependents: HashMap<RequestId, Vec<RequestId>>,
}

impl Journal {
    /// Exact payload for a new root intent plus dispatch (including JSONL newlines).
    /// Does not write or authorize either transition.
    pub fn admission_bytes(id: &RequestId, args_sha256: &str) -> u64 {
        canonical_of(&Line::Intent { request_id: id.clone(), args_sha256: args_sha256.to_owned() }).len() as u64
            + canonical_of(&Line::Dispatched { request_id: id.clone() }).len() as u64 + 2
    }

    /// Exact append payload for completion, including the JSONL newline.
    pub fn completion_bytes(id: &RequestId, result: &serde_json::Value) -> u64 {
        canonical_of(&Line::Completed { request_id: id.clone(), result: result.clone() }).len() as u64 + 1
    }

    pub fn open(path: &Path) -> Result<Journal, JournalError> {
        let opened = open_append(path)?;
        let snapshot = Snapshot::parse(path, &opened.lines)?;
        Ok(Journal {
            path: path.to_path_buf(), out: Appender::new(opened.file, opened.len),
            entries: snapshot.entries, last_completed: snapshot.last_completed,
            parents: snapshot.parents, dependents: snapshot.dependents,
        })
    }

    /// Reads complete durable evidence without opening a writer or repairing a partial tail.
    /// A missing file is an error, rather than evidence that the request never existed.
    pub fn read_state(path: &Path, id: &RequestId) -> Result<Option<RequestState>, JournalError> {
        Ok(Self::read_record(path, id)?.map(|(_, state)| state))
    }

    /// Reads the argument binding and progress from one coherent, read-only journal snapshot.
    pub fn read_record(path: &Path, id: &RequestId) -> Result<Option<(String, RequestState)>, JournalError> {
        let mut snapshot = Snapshot::parse(path, &crate::lines::read_complete(path)?)?;
        Ok(snapshot.entries.remove(id))
    }

    fn write(&mut self, line: &Line) -> Result<(), JournalError> {
        let text = canonical_of(line);
        let io = |source| JournalError::Io { path: self.path.display().to_string(), source };
        self.out.append_line(&text).map_err(io)?;
        self.out.sync().map_err(|source| JournalError::Io { path: self.path.display().to_string(), source })
    }

    pub fn begin(&mut self, id: &RequestId, args_sha256: &str) -> Result<Begin, JournalError> {
        self.begin_request(id, args_sha256, None)
    }

    /// Atomically records the answer's intent and its position under a dispatched parent. No effect
    /// may be sent before this succeeds. Retrying the same id cannot move it or change its parent.
    pub fn begin_dependent(&mut self, parent: &RequestId, id: &RequestId, args_sha256: &str) -> Result<Begin, JournalError> {
        self.begin_request(id, args_sha256, Some(parent))
    }

    /// The admission fingerprint, retained even when an interrupted intent has no saved plan.
    pub fn argument_hash(&self, id: &RequestId) -> Option<&str> { self.entries.get(id).map(|(hash, _)| hash.as_str()) }

    pub fn parent(&self, id: &RequestId) -> Option<&RequestId> { self.parents.get(id) }

    /// Admission order, including intents interrupted before dispatch; never hash-map or completion order.
    pub fn dependents(&self, parent: &RequestId) -> &[RequestId] {
        self.dependents.get(parent).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn pending_dependents(&self, parent: &RequestId) -> Vec<RequestId> {
        self.dependents(parent).iter().filter(|id| matches!(self.state(id), Some(RequestState::Intent | RequestState::Dispatched))).cloned().collect()
    }

    fn begin_request(&mut self, id: &RequestId, args_sha256: &str, parent: Option<&RequestId>) -> Result<Begin, JournalError> {
        if let Some((args, state)) = self.entries.get(id) {
            if args != args_sha256 || self.parents.get(id) != parent {
                return Ok(Begin::Conflict);
            }
            return Ok(match state {
                RequestState::Intent | RequestState::NotApplied => Begin::NotApplied,
                RequestState::Dispatched => Begin::Unresolved,
                RequestState::Completed(v) => Begin::Completed(v.clone()),
            });
        }
        if let Some(parent) = parent {
            if parent == id || self.parents.contains_key(parent) || !matches!(self.state(parent), Some(RequestState::Dispatched)) {
                return Err(JournalError::WrongStep { id: parent.clone(), expected: "dispatched root parent" });
            }
            self.write(&Line::DependentIntent { parent: parent.clone(), request_id: id.clone(), args_sha256: args_sha256.to_string() })?;
            self.parents.insert(id.clone(), parent.clone());
            self.dependents.entry(parent.clone()).or_default().push(id.clone());
        } else {
            self.write(&Line::Intent { request_id: id.clone(), args_sha256: args_sha256.to_string() })?;
        }
        self.entries.insert(id.clone(), (args_sha256.to_string(), RequestState::Intent));
        Ok(Begin::New)
    }

    pub fn dispatched(&mut self, id: &RequestId) -> Result<(), JournalError> {
        match self.entries.get(id).map(|e| &e.1) {
            None => return Err(JournalError::UnknownRequest(id.clone())),
            Some(RequestState::Intent) => {}
            Some(_) => return Err(JournalError::WrongStep { id: id.clone(), expected: "intent" }),
        }
        self.write(&Line::Dispatched { request_id: id.clone() })?;
        self.entries.get_mut(id).unwrap().1 = RequestState::Dispatched;
        Ok(())
    }

    /// Checks completion eligibility while the caller holds exclusive journal access, before it
    /// publishes any replay input or other parent outcome.
    pub fn validate_completion(&self, id: &RequestId) -> Result<(), JournalError> {
        match self.entries.get(id).map(|e| &e.1) {
            None => return Err(JournalError::UnknownRequest(id.clone())),
            Some(RequestState::Intent | RequestState::Dispatched) => {}
            Some(_) => return Err(JournalError::WrongStep { id: id.clone(), expected: "intent or dispatched" }),
        }
        if !self.pending_dependents(id).is_empty() {
            return Err(JournalError::WrongStep { id: id.clone(), expected: "all dependent requests resolved" });
        }
        Ok(())
    }

    pub fn complete(&mut self, id: &RequestId, result: serde_json::Value) -> Result<(), JournalError> {
        self.validate_completion(id)?;
        self.write(&Line::Completed { request_id: id.clone(), result: result.clone() })?;
        self.entries.get_mut(id).unwrap().1 = RequestState::Completed(result);
        self.last_completed = Some(id.clone());
        Ok(())
    }

    /// Records that recovery established a dispatched request was never applied.
    pub fn resolve_unapplied(&mut self, id: &RequestId) -> Result<(), JournalError> {
        match self.entries.get(id).map(|e| &e.1) {
            None => return Err(JournalError::UnknownRequest(id.clone())),
            Some(RequestState::Dispatched) => {}
            Some(_) => return Err(JournalError::WrongStep { id: id.clone(), expected: "dispatched" }),
        }
        if !self.dependents(id).is_empty() {
            return Err(JournalError::WrongStep { id: id.clone(), expected: "no dependent requests" });
        }
        self.write(&Line::ResolvedNotApplied { request_id: id.clone() })?;
        self.entries.get_mut(id).unwrap().1 = RequestState::NotApplied;
        Ok(())
    }

    pub fn unresolved(&self) -> Vec<RequestId> {
        let mut v: Vec<RequestId> =
            self.entries.iter().filter(|(_, (_, s))| *s == RequestState::Dispatched).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    /// Reads durable request evidence without creating an intent, dispatching, or resolving anything.
    pub fn state(&self, id: &RequestId) -> Option<&RequestState> {
        self.entries.get(id).map(|(_, state)| state)
    }

    pub fn last_completed(&self) -> Option<&RequestId> {
        self.last_completed.as_ref()
    }
}

struct Snapshot {
    entries: HashMap<RequestId, (String, RequestState)>,
    last_completed: Option<RequestId>,
    parents: HashMap<RequestId, RequestId>,
    dependents: HashMap<RequestId, Vec<RequestId>>,
}

impl Snapshot {
    fn parse(path: &Path, lines: &[String]) -> Result<Self, JournalError> {
        let mut entries = HashMap::new();
        let mut last_completed = None;
        let mut parents = HashMap::new();
        let mut dependents: HashMap<RequestId, Vec<RequestId>> = HashMap::new();
        let corrupt = |line: usize, reason: String| JournalError::Corrupt { path: path.display().to_string(), line, reason };
        for (i, l) in lines.iter().enumerate() {
            let line: Line = serde_json::from_str(l).map_err(|e| corrupt(i + 1, e.to_string()))?;
            let (id, state) = match line {
                Line::Intent { request_id, args_sha256 } => {
                    if entries.insert(request_id, (args_sha256, RequestState::Intent)).is_some() {
                        return Err(corrupt(i + 1, "duplicate request intent".into()));
                    }
                    continue;
                }
                Line::DependentIntent { parent, request_id, args_sha256 } => {
                    if parent == request_id || parents.contains_key(&parent) || !matches!(entries.get(&parent), Some((_, RequestState::Dispatched))) {
                        return Err(corrupt(i + 1, "dependent intent requires a dispatched root parent".into()));
                    }
                    if entries.insert(request_id.clone(), (args_sha256, RequestState::Intent)).is_some() {
                        return Err(corrupt(i + 1, "duplicate dependent intent".into()));
                    }
                    parents.insert(request_id.clone(), parent.clone());
                    dependents.entry(parent).or_default().push(request_id);
                    continue;
                }
                Line::Dispatched { request_id } => (request_id, RequestState::Dispatched),
                Line::Completed { request_id, result } => {
                    last_completed = Some(request_id.clone());
                    (request_id, RequestState::Completed(result))
                }
                Line::ResolvedNotApplied { request_id } => (request_id, RequestState::NotApplied),
            };
            let pending_children = dependents.get(&id).is_some_and(|children| children.iter().any(|child|
                matches!(entries.get(child), Some((_, RequestState::Intent | RequestState::Dispatched)))));
            if (matches!(state, RequestState::NotApplied) && dependents.contains_key(&id))
                || (matches!(state, RequestState::Completed(_)) && pending_children)
            {
                return Err(corrupt(i + 1, "parent resolution precedes its dependent requests".into()));
            }
            let entry = entries.get_mut(&id).ok_or_else(|| corrupt(i + 1, format!("step for unknown request {id}")))?;
            let valid = matches!((&entry.1, &state),
                (RequestState::Intent, RequestState::Dispatched | RequestState::Completed(_)) |
                (RequestState::Dispatched, RequestState::Completed(_) | RequestState::NotApplied));
            if !valid { return Err(corrupt(i + 1, format!("invalid request transition for {id}"))); }
            entry.1 = state;
        }
        Ok(Self { entries, last_completed, parents, dependents })
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
