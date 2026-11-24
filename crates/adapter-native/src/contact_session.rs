//! Owned file transport for the existing injected HID driver. Never adopts or redispatches input.
use frontcap_core::{
    ids::RequestId,
    native::{
        contacts::{Endpoint, Operation, Receipt, Window},
        webview::Owner,
    },
    state::save_atomic,
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Existing page owners retain their wire shape; native hosts do not fabricate a page endpoint.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Consumer {
    WebView(Owner),
    Host(frontcap_core::native::ios_host::Owner),
}
impl From<Owner> for Consumer { fn from(owner: Owner) -> Self { Self::WebView(owner) } }
impl From<frontcap_core::native::ios_host::Owner> for Consumer {
    fn from(owner: frontcap_core::native::ios_host::Owner) -> Self { Self::Host(owner) }
}
impl Consumer {
    fn validate(&self) -> Result<(), frontcap_core::native::request::InvalidRequest> {
        match self { Self::WebView(owner) => owner.validate(), Self::Host(owner) => owner.validate() }
    }
    fn process(&self) -> &frontcap_core::process::ProcessIdentity {
        match self { Self::WebView(owner) => &owner.process, Self::Host(owner) => &owner.process }
    }
}

// Foundation may re-encode integral doubles as JSON integers. Compare the typed
// request, preserving every field and rejecting unknown keys, rather than Number variants.
#[derive(Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct NativeRequest {
    tap: Option<TapPoint>,
    generation: String,
    request_id: u64,
    expected_revision: u64,
    contacts: Option<Vec<frontcap_core::native::contacts::Contact>>,
    cancel: Option<bool>,
    expected_window: Option<Window>,
}
#[derive(Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct TapPoint { x: f64, y: f64 }

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: RequestId,
    operation: Operation,
    window: Option<Window>,
    request: serde_json::Value,
    receipt: Option<Receipt>,
}
#[derive(Clone, Serialize, Deserialize)]
struct State {
    // Only a cancellation admitted after this boundary may release the interruption fence.
    #[serde(default)]
    cancel_after: Option<usize>,
    owner: Consumer,
    endpoint: Endpoint,
    entries: Vec<Entry>,
}
/// Construct only while retaining the app/simulator owner. Reopening evidence cannot grant authority.
pub struct Session {
    directory: PathBuf,
    evidence: PathBuf,
    state: State,
    _lock: crate::store::RequestLease,
    _native_lock: crate::store::RequestLease,
    poisoned: bool,
}
fn bad(message: &str) -> io::Error {
    io::Error::other(message)
}
fn read(path: &Path) -> io::Result<serde_json::Value> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(bad("linked native contact evidence"));
    }
    serde_json::from_slice(&crate::webview::read_receipt_bytes(path)?).map_err(io::Error::other)
}
impl Session {
    pub fn create(directory: &Path, evidence: &Path, owner: impl Into<Consumer>) -> io::Result<Self> {
        let owner = owner.into();
        owner.validate().map_err(io::Error::other)?;
        if owner.process().has_exited()? {
            return Err(bad("contact owner has exited"));
        }
        if std::fs::symlink_metadata(directory)?
            .file_type()
            .is_symlink()
        {
            return Err(bad("linked contact directory"));
        }
        let native_lock = crate::store::lock(directory).map_err(io::Error::other)?;
        let endpoint: Endpoint = serde_json::from_value(read(&directory.join("endpoint.json"))?)
            .map_err(io::Error::other)?;
        endpoint
            .validate(owner.process().pid)
            .map_err(io::Error::other)?;
        // No second controller may begin numbering in an already used native endpoint.
        for entry in std::fs::read_dir(directory)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("request-") || name.starts_with("reply-") {
                return Err(bad("contact endpoint already used"));
            }
        }
        std::fs::create_dir(evidence)?;
        let lock = crate::store::lock(evidence).map_err(io::Error::other)?;
        let state = State {
            cancel_after: None,
            owner,
            endpoint,
            entries: Vec::new(),
        };
        save_atomic(&evidence.join("contacts.json"), &state)?;
        Ok(Self {
            directory: directory.canonicalize()?,
            evidence: evidence.canonicalize()?,
            state,
            _lock: lock,
            _native_lock: native_lock,
            poisoned: false,
        })
    }
    fn publish(&mut self, state: State) -> io::Result<()> {
        if self.poisoned {
            return Err(bad("contact evidence publication uncertain"));
        }
        self.poisoned = true;
        save_atomic(&self.evidence.join("contacts.json"), &state)?;
        self.state = state;
        self.poisoned = false;
        Ok(())
    }
    fn live(&self) -> io::Result<()> {
        if self.poisoned || self.state.owner.process().has_exited()? {
            return Err(bad("contact owner unavailable"));
        }
        let endpoint: Endpoint =
            serde_json::from_value(read(&self.directory.join("endpoint.json"))?)
                .map_err(io::Error::other)?;
        if endpoint != self.state.endpoint {
            return Err(bad("contact endpoint changed"));
        }
        Ok(())
    }
    /// Same-ID requests inspect the original receipt only, including after a timeout.
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Receipt> {
        self.execute_with_window(id, operation, None, deadline, cancelled)
    }
    pub fn endpoint(&self) -> &Endpoint { &self.state.endpoint }
    /// Inspect requested input and original receipts; does not sample actual page contacts.
    pub fn inspect(&self) -> frontcap_core::native::webview_progress::Journal<Operation, Receipt> {
        use frontcap_core::native::webview_progress::{Completed, Dispatch, Journal, Pending};
        let last_completed = self.state.entries.iter().enumerate().rev().find_map(|(index, entry)| {
            entry.receipt.as_ref().map(|reply| Completed { id: entry.id.clone(),
                native_request_id: index as u64 + 1, request: entry.operation.clone(), reply: reply.clone() })
        });
        let pending = self.state.entries.iter().enumerate().find_map(|(index, entry)| {
            entry.receipt.is_none().then(|| Pending { id: entry.id.clone(),
                native_request_id: index as u64 + 1, request: entry.operation.clone(), dispatch: Dispatch::Unresolved })
        });
        Journal { storage_uncertain: self.poisoned, last_completed, pending }
    }
    pub fn requires_cancel(&self) -> bool {
        self.state.cancel_after.is_some()
    }
    /// Persist before a native lifecycle effect can cancel delivered touches independently.
    pub fn interrupt(&mut self) -> io::Result<()> {
        let unresolved = self
            .state
            .entries
            .iter()
            .any(|entry| entry.receipt.is_none());
        let held = self
            .state
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.receipt {
                Some(Receipt::Enqueued { contacts, .. }) => Some(!contacts.is_empty()),
                _ => None,
            })
            .unwrap_or(false);
        if unresolved || held {
            let mut state = self.state.clone();
            state.cancel_after = Some(state.entries.len());
            self.publish(state)?;
        }
        Ok(())
    }
    pub fn validate_new_input(&self, operation: &Operation) -> io::Result<()> {
        if matches!(operation, Operation::Tap { .. }) {
            if !self.state.endpoint.short_tap {
                return Err(bad("short tap is unavailable on this contact producer"));
            }
            let held = self.state.entries.iter().rev().find_map(|entry| match &entry.receipt {
                Some(Receipt::Enqueued { contacts, .. }) => Some(!contacts.is_empty()),
                _ => None,
            }).unwrap_or(false);
            if held || self.state.entries.iter().any(|entry| entry.receipt.is_none()) {
                return Err(bad("resolve and release contacts before short tap"));
            }
        }
        if self.requires_cancel() && !matches!(operation, Operation::Cancel { .. }) {
            return Err(bad(
                "contacts interrupted; cancel with a fresh foreground capture before new input",
            ));
        }
        Ok(())
    }
    pub fn admission(&self) -> Option<(u64, u64)> {
        if self.poisoned
            || self.state.entries.len() as u64 >= self.state.endpoint.request_capacity
            || self
                .state
                .entries
                .iter()
                .any(|entry| entry.receipt.is_none())
        {
            return None;
        }
        let revision = self
            .state
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.receipt {
                Some(Receipt::Enqueued { revision, .. }) => Some(*revision),
                _ => None,
            })
            .unwrap_or(0);
        Some((
            revision,
            self.state.endpoint.request_capacity - self.state.entries.len() as u64,
        ))
    }
    pub fn window(&self) -> io::Result<Window> {
        self.live()?;
        let raw = read(&self.directory.join("window.json"))?;
        save_atomic(&self.evidence.join("window.json"), &raw)?;
        let window: Window = serde_json::from_value(raw).map_err(io::Error::other)?;
        window
            .validate(&self.state.endpoint)
            .map_err(io::Error::other)?;
        Ok(window)
    }
    pub fn execute_observed(
        &mut self,
        id: RequestId,
        operation: Operation,
        window: Window,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Receipt> {
        window
            .validate(&self.state.endpoint)
            .map_err(io::Error::other)?;
        self.execute_with_window(id, operation, Some(window), deadline, cancelled)
    }
    fn execute_with_window(
        &mut self,
        id: RequestId,
        operation: Operation,
        window: Option<Window>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Receipt> {
        operation.validate().map_err(io::Error::other)?;
        if self.poisoned {
            return Err(bad("contact evidence publication uncertain"));
        }
        if let Some(index) = self.state.entries.iter().position(|e| e.id == id) {
            if self.state.entries[index].operation != operation
                || self.state.entries[index].window != window
            {
                return Err(bad("contact request identity conflict"));
            }
            return self.wait_reply(index, deadline, cancelled);
        }
        self.validate_new_input(&operation)?;
        if matches!(operation, Operation::Tap { .. }) && window.is_none() {
            return Err(bad("short tap requires a fresh observed window"));
        }
        self.live()?;
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(bad("contact dispatch budget expired"));
        }
        if self.state.entries.len() as u64 >= self.state.endpoint.request_capacity
            || self.state.entries.iter().any(|e| e.receipt.is_none())
        {
            return Err(bad(
                "contact capacity exhausted or earlier dispatch unresolved",
            ));
        }
        let revision = self
            .state
            .entries
            .iter()
            .rev()
            .find_map(|e| match &e.receipt {
                Some(Receipt::Enqueued { revision, .. }) => Some(*revision),
                _ => None,
            })
            .unwrap_or(0);
        if operation.revision() != revision {
            return Err(bad("stale contact revision"));
        }
        let number = self.state.entries.len() + 1;
        let mut request = serde_json::json!({"generation":self.state.endpoint.generation,"request_id":number,"expected_revision":revision});
        match &operation {
            Operation::Set { contacts, .. } => {
                request["contacts"] = serde_json::to_value(contacts).map_err(io::Error::other)?
            }
            Operation::Cancel { .. } => request["cancel"] = true.into(),
            Operation::Tap { x, y, .. } => request["tap"] = serde_json::json!({"x":x,"y":y}),
        }
        if let Some(window) = &window {
            if &self.window()? != window {
                return Err(bad("observation window changed before contact publication"));
            }
            request["expected_window"] = serde_json::to_value(window).map_err(io::Error::other)?;
        }
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id,
            operation,
            window,
            request: request.clone(),
            receipt: None,
        });
        // Persist uncertainty before exposing bytes to the native poller. Failure here or after
        // publication never permits allocating another sequence or overwriting its request.
        self.publish(state)?;
        let staged = self.evidence.join(format!("request-{number}.json"));
        save_atomic(&staged, &request)?;
        // Stage on the consumer filesystem: its data volume may differ from evidence storage.
        // Only the final no-clobber link exposes a complete request to the native poller.
        let pending = self.directory.join(format!(".frontcap-request-{number}.pending"));
        let mut file = std::fs::File::options().write(true).create_new(true).open(&pending)?;
        let published = (|| {
            use std::io::Write;
            file.write_all(frontcap_core::canonical::canonical_of(&request).as_bytes())?;
            file.sync_all()?;
            std::fs::hard_link(&pending, self.directory.join(format!("request-{number}.json")))?;
            std::fs::File::open(&self.directory)?.sync_all()
        })();
        drop(file);
        let removed = std::fs::remove_file(&pending);
        published?;
        removed?;
        self.wait_reply(number - 1, deadline, cancelled)
    }
    /// Preserve the latest unresolved native evidence before owned device deletion; no promotion.
    pub fn retain_pending(&self) -> io::Result<()> {
        for (index, entry) in self.state.entries.iter().enumerate() {
            if entry.receipt.is_some() {
                continue;
            }
            let name = format!("reply-{}.json", index + 1);
            match read(&self.directory.join(&name)) {
                Ok(raw) => save_atomic(&self.evidence.join(name), &raw)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// Inspect only the original reply. A new recovery command never grants redispatch authority.
    pub fn reconcile(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Receipt> {
        if self.poisoned {
            return Err(bad("contact evidence publication uncertain"));
        }
        let index = self
            .state
            .entries
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or_else(|| bad("unknown native contact request"))?;
        self.wait_reply(index, deadline, cancelled)
    }
    fn wait_reply(
        &mut self,
        index: usize,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Receipt> {
        if let Some(receipt) = &self.state.entries[index].receipt {
            return Ok(receipt.clone());
        }
        loop {
            self.live()?;
            let path = self.directory.join(format!("reply-{}.json", index + 1));
            match read(&path) {
                Ok(raw) => {
                    // Retain even unknown/foreign evidence before returning; never interpret an
                    // executing, missing or mismatched record as a no-effect acknowledgement.
                    save_atomic(
                        &self.evidence.join(format!("reply-{}.json", index + 1)),
                        &raw,
                    )?;
                    if let Some(receipt) = self.validate(index, &raw)? {
                        let mut state = self.state.clone();
                        state.entries[index].receipt = Some(receipt.clone());
                        if state.cancel_after.is_some_and(|boundary| index >= boundary)
                            && matches!(state.entries[index].operation, Operation::Cancel { .. })
                            && matches!(&receipt, Receipt::Enqueued { contacts, .. } if contacts.is_empty())
                        {
                            state.cancel_after = None;
                        }
                        self.publish(state)?;
                        return Ok(receipt);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(bad(
                    "native contact result unresolved; inspect original request, never redispatch",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn validate(&self, index: usize, raw: &serde_json::Value) -> io::Result<Option<Receipt>> {
        let entry = &self.state.entries[index];
        if raw["generation"].as_str() != Some(&self.state.endpoint.generation)
            || raw["request_id"].as_u64() != Some(index as u64 + 1)
        {
            return Err(bad("foreign native contact receipt"));
        }
        match raw["status"].as_str() {
            Some("executing") => Ok(None),
            Some("rejected") => Ok(Some(Receipt::Rejected {
                reason: raw["error"]
                    .as_str()
                    .ok_or_else(|| bad("missing contact rejection reason"))?
                    .into(),
            })),
            Some("enqueued") => {
                let echoed: NativeRequest =
                    serde_json::from_value(raw["request"].clone()).map_err(io::Error::other)?;
                let original: NativeRequest =
                    serde_json::from_value(entry.request.clone()).map_err(io::Error::other)?;
                if echoed != original {
                    return Err(bad("contact receipt request differs"));
                }
                let mut contacts: Vec<frontcap_core::native::contacts::Contact> =
                    serde_json::from_value(raw["contacts"].clone()).map_err(io::Error::other)?;
                let mut expected = entry.operation.contacts().to_vec();
                contacts.sort_by_key(|p| p.id);
                expected.sort_by_key(|p| p.id);
                let revision = raw["contact_revision"]
                    .as_u64()
                    .ok_or_else(|| bad("missing contact revision"))?;
                if contacts != expected || revision != entry.operation.revision() + 1 {
                    return Err(bad("contact receipt state differs"));
                }
                let tap = match &entry.operation {
                    Operation::Tap { x, y, .. } => {
                        let proof: frontcap_core::native::contacts::TapEvents =
                            serde_json::from_value(raw["tap"].clone()).map_err(io::Error::other)?;
                        let window = entry.window.as_ref().ok_or_else(|| bad("short tap window missing"))?;
                        if !proof.valid(x * window.width, y * window.height) || raw["changed"] != true
                            || raw["mach_timestamp"].as_u64().is_none_or(|t| t < proof.frames[1].mach_timestamp) {
                            return Err(bad("short tap lacks a bounded native DOWN/UP pair"));
                        }
                        Some(proof)
                    }
                    _ if raw.get("tap").is_some() => return Err(bad("unexpected short tap proof")),
                    _ => None,
                };
                Ok(Some(Receipt::Enqueued {
                    tap,
                    revision,
                    contacts,
                    changed: raw["changed"]
                        .as_bool()
                        .ok_or_else(|| bad("missing contact change evidence"))?,
                    mach_timestamp: raw["mach_timestamp"]
                        .as_u64()
                        .filter(|v| *v > 0)
                        .ok_or_else(|| bad("missing contact timestamp"))?,
                }))
            }
            _ => Err(bad(
                "native contact outcome unknown; retain ownership for recovery or retirement",
            )),
        }
    }
}

/// Original completed input evidence, without opening a writer or contacting the native process.
pub(crate) fn read_host_completion(evidence:&Path, owner:&frontcap_core::native::ios_host::Owner, id:&RequestId, operation:Option<&Operation>) -> io::Result<Option<Receipt>> {
    let Some(state):Option<State>=frontcap_core::state::load(&evidence.join("contacts.json"))? else {return Ok(None)};
    if !matches!(&state.owner,Consumer::Host(saved) if saved==owner) {return Err(bad("native input original owner differs"));}
    state.endpoint.validate(owner.process.pid).map_err(io::Error::other)?;
    let Some(entry)=state.entries.iter().find(|entry| &entry.id==id) else {return Ok(None)};
    if operation.is_some_and(|op|op!=&entry.operation) {return Err(bad("native input original operation differs"));}
    Ok(entry.receipt.clone())
}

#[cfg(test)]
#[path = "contact_session_tests.rs"]
mod tests;
