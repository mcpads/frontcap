//! Explicit per-view authority inside a native host; never reacquires a replacement view.
use crate::{simulator_app::App, webview_store::RequestStore};
use frontcap_core::{
    ids::RequestId,
    native::{
        ios_host::{Attachment, Inventory, Owner, Page, PageOperation},
        request::ClockOperation,
        webview::{Owner as ClockOwner, Reply, Status},
    },
    state::load,
    write_budget::{Class, WriteBudget},
};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn bad(error: impl ToString) -> io::Error {
    io::Error::other(error.to_string())
}
fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| bad("original page evidence missing"))
}
struct Active {
    attachment: Attachment,
    requests: RequestStore,
}
pub(crate) struct Pages {
    budget: Option<WriteBudget>,
    directory: PathBuf,
    owner: Owner,
    previous: Option<Inventory>,
    attempted: HashSet<String>,
    active: Option<Active>,
}
impl Pages {
    fn save<T: serde::Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        match &self.budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
            None => frontcap_core::state::save_atomic(path, value),
        }
    }

    pub fn new(directory: &Path, owner: Owner) -> io::Result<Self> {
        let budget = WriteBudget::from_environment()?;
        std::fs::create_dir(directory)?;
        Ok(Self {
            budget,
            directory: directory.into(),
            owner,
            previous: None,
            attempted: HashSet::new(),
            active: None,
        })
    }
    pub fn discover(
        &mut self,
        app: &mut App,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Inventory> {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(bad("page discovery budget expired"));
        }
        app.ensure_running(deadline, cancelled)?;
        let bytes = crate::webview::read_receipt_bytes(
            &app.container().join("Documents/webview-pages.json"),
        )?;
        let inventory: Inventory = serde_json::from_slice(&bytes)?;
        inventory.validate(&self.owner).map_err(bad)?;
        if self.previous.as_ref().is_some_and(|old| {
            inventory.revision < old.revision
                || (inventory.revision == old.revision && inventory != *old)
                || (old.capacity_exceeded && !inventory.capacity_exceeded)
                || old.navigation.as_ref().is_some_and(|prior|
                    inventory.navigation.as_ref().is_none_or(|next| !next.follows(prior)))
                || inventory.pages.len() < old.pages.len()
                || old.pages.iter().zip(&inventory.pages).any(|(a, b)| {
                    a.id != b.id
                        || a.view != b.view
                        || (a.document.is_some() && a.document != b.document)
                        || (a.superseded && !b.superseded)
                        || a.endpoint != b.endpoint
                        || (!a.alive && b.alive)
                        || (a.control_stopped && !b.control_stopped)
                })
        }) {
            return Err(bad("page inventory regressed or rebound an identity"));
        }
        self.previous = Some(inventory.clone());
        Ok(inventory)
    }
    pub fn attach(
        &mut self,
        app: &mut App,
        id: &RequestId,
        candidate: &Page,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Attachment> {
        if self.active.is_some() {
            return Err(bad(
                "release or retire the original attachment before selecting another page",
            ));
        }
        let inventory = self.discover(app, deadline, cancelled)?;
        if inventory.capacity_exceeded || !candidate.alive || candidate.control_stopped || candidate.superseded {
            return Err(bad("selected page is unavailable for attachment"));
        }
        if self.attempted.contains(&candidate.id) {
            return Err(bad("selected page was already attempted; inspect the original attachment instead of retrying"));
        }
        if !inventory.pages.contains(candidate) {
            return Err(bad("selected page discovery changed before attachment; discover again and select a fresh candidate"));
        }
        self.attempted.insert(candidate.id.clone());
        let root = self.directory.join(id.as_str());
        std::fs::create_dir(&root)?;
        self.save(&root.join("candidate.json"), candidate)?;
        let clock_owner = ClockOwner {
            simulator: self.owner.simulator.clone(),
            process: self.owner.process.clone(),
            attachment_sha256: self.owner.attachment_sha256.clone(),
            endpoint: candidate.endpoint.clone(),
        };
        let mut requests =
            RequestStore::create(&root.join("requests"), clock_owner.clone()).map_err(bad)?;
        let mut sequence = 0u64;
        let document = loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(bad("selected page readiness expired"));
            }
            let current = self.discover(app, deadline, cancelled)?;
            if !current.pages.iter().any(|p| {
                p.id == candidate.id
                    && p.endpoint == candidate.endpoint
                    && p.alive
                    && !p.control_stopped
                    && !p.superseded
            }) {
                return Err(bad("selected page disappeared before readiness"));
            }
            sequence += 1;
            let reply = requests
                .execute(
                    RequestId::new(format!("mcp-page-acquire-{sequence}")).map_err(bad)?,
                    ClockOperation::Read,
                    0,
                    deadline.min(Instant::now() + Duration::from_secs(10)),
                )
                .map_err(bad)?;
            match reply.status {
                Status::Ok => {
                    break reply
                        .document
                        .ok_or_else(|| bad("selected page document missing"))?;
                }
                Status::NotReady => std::thread::sleep(Duration::from_millis(25)),
                _ => return Err(bad("selected page initialization rejected or uncertain")),
            }
        };
        let attachment = Attachment {
            id: id.clone(),
            owner: clock_owner,
            page: candidate.clone(),
            document,
        };
        attachment.validate(&self.owner).map_err(bad)?;
        self.save(&root.join("attachment.json"), &attachment)?;
        self.active = Some(Active {
            attachment: attachment.clone(),
            requests,
        });
        Ok(attachment)
    }
    pub fn execute(
        &mut self,
        app: &mut App,
        attachment: &RequestId,
        id: &RequestId,
        command: &PageOperation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Reply> {
        let active = self
            .active
            .as_ref()
            .filter(|a| a.attachment.id == *attachment)
            .ok_or_else(|| bad("page operation requires the original active attachment"))?;
        let page = active.attachment.page.clone();
        if !matches!(command, PageOperation::Reconcile { .. }) {
            let current = self.discover(app, deadline, cancelled)?;
            if current.capacity_exceeded
                || !current.pages.iter().any(|p| {
                    p.id == page.id && p.endpoint == page.endpoint && p.alive && !p.control_stopped
                    && !p.superseded
                })
            {
                return Err(bad("attached page is no longer available"));
            }
        }
        let active = self.active.as_mut().unwrap();
        match command {
            PageOperation::Reconcile { request_id } => active
                .requests
                .reconcile(request_id, &receipt(app, &page))
                .map_err(bad),
            _ => {
                let (operation, delta) = command.clock().unwrap();
                active
                    .requests
                    .execute(id.clone(), operation, delta, deadline)
                    .map_err(bad)
            }
        }
    }
    pub fn release(&mut self, app: &App, id: &RequestId, evidence: &Path) -> io::Result<Attachment> {
        let active = self.active.as_ref().filter(|a| a.attachment.id == *id)
            .ok_or_else(|| bad("release requires the original active attachment"))?;
        let journal = active.requests.inspect();
        if journal.storage_uncertain || journal.pending.is_some() {
            return Err(bad("resolve the original page request before releasing its attachment"));
        }
        let attachment = active.attachment.clone();
        self.retain_one(app, id, &attachment.page, Class::Ordinary)?;
        let requests_sha256 = journal_hash(&self.directory, id)?;
        self.save(&evidence.join("page-release.json"), &Release { attachment: attachment.clone(), requests_sha256 })?;
        // This revokes our selection only. It neither closes the page channel nor drains app work.
        self.active = None;
        Ok(attachment)
    }
    pub fn retire(
        &mut self,
        app: &mut App,
        id: &RequestId,
        evidence: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Attachment> {
        let attachment = self
            .active
            .as_ref()
            .filter(|a| a.attachment.id == *id)
            .ok_or_else(|| bad("retirement requires the original active attachment"))?
            .attachment
            .clone();
        let current = self.discover(app, deadline, cancelled)?;
        let retired = current.pages.iter().any(|p| {
            p.id == attachment.page.id
                && p.endpoint == attachment.page.endpoint
                && !p.alive
                && p.control_stopped
        });
        if !retired {
            return Err(bad(
                "selected view deallocation and channel shutdown are not established",
            ));
        }
        self.retain_one(app, &attachment.id, &attachment.page, Class::Ordinary)?;
        self.save(&evidence.join("page-retirement.json"), &current)?;
        self.active = None;
        Ok(attachment)
    }
    fn retain_one(&self, app: &App, id: &RequestId, page: &Page, class: Class) -> io::Result<()> {
        let path = self.directory.join(id.as_str()).join("last-native-receipt.json");
        let bytes = crate::webview::read_receipt_bytes(&receipt(app, page))?;
        match &self.budget {
            Some(budget) => budget.save_atomic_bytes(class, &path, &bytes),
            None => frontcap_core::state::save_atomic_bytes(&path, &bytes),
        }
    }
    pub fn retain(&self, app: &App) -> io::Result<()> {
        self.retain_with_class(app, Class::Ordinary)
    }
    pub fn retain_for_stop(&self, app: &App) -> io::Result<()> {
        self.retain_with_class(app, Class::Retirement)
    }
    fn retain_with_class(&self, app: &App, class: Class) -> io::Result<()> {
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            if !item.file_type()?.is_dir() {
                return Err(bad("invalid page evidence directory"));
            }
            let candidate: Page = required(&item.path().join("candidate.json"))?;
            let id = RequestId::new(item.file_name().to_string_lossy()).map_err(bad)?;
            self.retain_one(app, &id, &candidate, class)?;
        }
        Ok(())
    }
}
fn receipt(app: &App, page: &Page) -> PathBuf {
    app.container()
        .join("Documents/webview-pages")
        .join(&page.id)
        .join("clock-control.json")
}
pub(crate) fn attachment(
    directory: &Path,
    id: &RequestId,
    owner: &Owner,
) -> io::Result<Attachment> {
    let original: Attachment = required(&directory.join(id.as_str()).join("attachment.json"))?;
    original.validate(owner).map_err(bad)?;
    if original.id != *id {
        return Err(bad("page attachment request differs"));
    }
    Ok(original)
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    attachment: Attachment,
    requests_sha256: String,
}
fn journal_hash(directory: &Path, id: &RequestId) -> io::Result<String> {
    Ok(frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(
        directory.join(id.as_str()).join("requests/requests.json"))?))
}
pub(crate) fn released(directory: &Path, owner: &Owner, id: &RequestId, evidence: &Path)
    -> io::Result<Option<Attachment>> {
    let Some(saved): Option<Release> = load(&evidence.join("page-release.json"))? else { return Ok(None); };
    let original = attachment(directory, id, owner)?;
    if saved.attachment != original || saved.requests_sha256 != journal_hash(directory, id)? {
        return Err(bad("page release differs from original attachment or request journal"));
    }
    Ok(Some(original))
}
pub(crate) fn completed(
    directory: &Path,
    owner: &Owner,
    attachment_id: &RequestId,
    request_id: &RequestId,
    command: &PageOperation,
) -> io::Result<Option<Reply>> {
    let original = attachment(directory, attachment_id, owner)?;
    let (kind, delta) = command
        .clock()
        .ok_or_else(|| bad("reconciliation requires original command binding"))?;
    let saved = RequestStore::read_completion(
        &directory.join(attachment_id.as_str()).join("requests"),
        request_id,
        kind,
        delta,
    )
    .map_err(bad)?;
    match saved {
        Some((clock_owner, reply)) if clock_owner == original.owner => {
            if reply.status == Status::Ok && reply.document.as_ref() != Some(&original.document) {
                return Err(bad("page result document differs"));
            }
            Ok(Some(reply))
        }
        Some(_) => Err(bad("page result owner differs")),
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "ios_host_pages_tests.rs"]
mod tests;
