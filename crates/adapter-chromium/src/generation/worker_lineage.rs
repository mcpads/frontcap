//! Constructor identity outlives live session routing for already-issued requests.
use super::*;
use crate::bindings::{BindingEntry, DocumentPlace, WorkerLineage};

/// Where a request's issuer stands among independent issuers at one instant (time-model.md, Ordering
/// at the same instant): the renderer's main thread, which runs every document, then dedicated
/// workers by their creator document's place and constructor path (a nested worker after its
/// creator), then workers whose ownership could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Issuer {
    MainThread,
    Worker { document: DocumentPlace, constructors: Vec<u64> },
    UnresolvedWorker,
}

fn retain_owner(owners: &mut BTreeMap<String, WorkerLineage>, network: &str, lineage: &WorkerLineage) -> Result<bool, ChromiumError> {
    if network.is_empty() || lineage.creator_document.is_empty() || lineage.constructors.is_empty()
        || lineage.constructors.contains(&0) {
        return Err(ChromiumError::Shape("invalid worker request owner".into()));
    }
    if let Some(previous) = owners.get(network) {
        if previous != lineage { return Err(ChromiumError::Shape("worker request changed owner".into())); }
        return Ok(false);
    }
    owners.insert(network.into(), lineage.clone());
    Ok(true)
}

pub(super) fn recovered_owners(entries: &[BindingEntry]) -> Result<BTreeMap<String, WorkerLineage>, ChromiumError> {
    let mut owners = BTreeMap::new();
    for entry in entries {
        match entry {
            BindingEntry::WorkerRequestOwner { network_id, lineage, .. } => { retain_owner(&mut owners, network_id, lineage)?; }
            // Older journals only retain ownership at selection time.
            BindingEntry::Bound(binding) => {
                if let (Some(network), Some(lineage)) = (&binding.network_id, &binding.worker_lineage) {
                    retain_owner(&mut owners, network, lineage)?;
                }
            }
            _ => {}
        }
    }
    Ok(owners)
}

pub(super) fn recovered_places(entries: &[BindingEntry]) -> Result<BTreeMap<String, DocumentPlace>, ChromiumError> {
    let mut places = BTreeMap::new();
    for entry in entries {
        if let BindingEntry::WorkerRequestOwner { network_id, document: Some(document), .. } = entry
            && places.insert(network_id.clone(), document.clone()).is_some_and(|previous| previous != *document)
        {
            return Err(ChromiumError::Shape("frozen worker request changed document place".into()));
        }
    }
    Ok(places)
}

pub(super) fn recovered_issues(entries: &[BindingEntry]) -> Result<BTreeMap<String, VirtualTime>, ChromiumError> {
    let mut issues = BTreeMap::new();
    for entry in entries {
        if let BindingEntry::WorkerRequestOwner { network_id, issued_us, .. } = entry {
            let issued = VirtualTime(*issued_us);
            if issues.insert(network_id.clone(), issued).is_some_and(|previous| previous != issued) {
                return Err(ChromiumError::Shape("frozen worker request changed issue instant".into()));
            }
        }
    }
    Ok(issues)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Constructor {
    sequence: Option<String>,
    document: Option<String>,
    parent: Option<String>,
}

#[derive(Default)]
pub(super) struct WorkerLineages {
    constructors: BTreeMap<String, Constructor>,
    sessions: BTreeMap<String, String>,
}

impl WorkerLineages {
    pub(super) fn remember(&mut self, session: &str, info: &target::TargetInfo) -> Result<(), ChromiumError> {
        if self.sessions.get(session).is_some_and(|old| old != &info.target_id.0) {
            return Err(ChromiumError::Shape("worker session changed target within a generation".into()));
        }
        self.remember_target(info)?;
        self.sessions.insert(session.into(), info.target_id.0.clone());
        Ok(())
    }

    fn remember_target(&mut self, info: &target::TargetInfo) -> Result<(), ChromiumError> {
        let constructor = Constructor {
            sequence: info.frontcap_worker_sequence.clone(),
            document: info.frontcap_creator_document_id.clone(),
            parent: info.parent_id.as_ref().map(|id| id.0.clone()),
        };
        let target = &info.target_id.0;
        if self.constructors.get(target).is_some_and(|old| old != &constructor) {
            return Err(ChromiumError::Shape("worker constructor identity changed within a generation".into()));
        }
        self.constructors.insert(target.clone(), constructor);
        Ok(())
    }

    pub(super) fn for_session(&self, session: &str) -> Result<Option<WorkerLineage>, ChromiumError> {
        match self.sessions.get(session) {
            Some(target) => self.for_target(target),
            None => Ok(None),
        }
    }

    /// A browser-owned main-script Network id is its worker target token. Neither
    /// token nor script URL supplies order; only native constructor sequences do.
    pub(super) fn for_target(&self, target: &str) -> Result<Option<WorkerLineage>, ChromiumError> {
        let mut current = target;
        let mut path = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        loop {
            if !visited.insert(current) {
                return Err(ChromiumError::Shape("cyclic worker constructor ancestry".into()));
            }
            let Some(constructor) = self.constructors.get(current) else { return Ok(None) };
            let Some(sequence) = &constructor.sequence else { return Ok(None) };
            let sequence = sequence.parse::<u64>().ok().filter(|n| *n != 0)
                .ok_or_else(|| ChromiumError::Shape("invalid native worker constructor sequence".into()))?;
            path.push(sequence);
            if let Some(document) = &constructor.document {
                if document.is_empty() { return Err(ChromiumError::Shape("empty worker creator document identity".into())); }
                path.reverse();
                return Ok(Some(WorkerLineage { creator_document: document.clone(), constructors: path }));
            }
            let Some(parent) = &constructor.parent else { return Ok(None) };
            current = parent;
        }
    }
}

impl Generation {
    /// Browser hosts precede the worker inspector's attachment. Capture their
    /// constructor identity before selecting a still-held main-script response.
    pub(super) async fn refresh_worker_lineages(&self) -> Result<(), ChromiumError> {
        if !self.patched { return Ok(()) }
        let targets = self.page.client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
        let mut book = self.subs.workers.lock().unwrap();
        for target in targets.iter().filter(|t| t.r#type == "worker") {
            book.lineages.remember_target(target)?;
        }
        Ok(())
    }

    pub(super) fn worker_lineage(&self, event: &fetch::RequestPausedEvent) -> Result<Option<WorkerLineage>, ChromiumError> {
        let Some(network) = &event.network_id else { return Ok(None) };
        if let Some(owner) = self.net.worker_request_owners.get(&network.0) { return Ok(Some(owner.clone())); }
        self.announced_worker_lineage(&network.0)
    }

    /// Freeze announced ordinary worker owners while Fetch pauses may still be
    /// missing. This does not allocate an ordinal, rule or sequence position.
    pub(super) async fn freeze_worker_request_owners(&mut self) -> Result<(), ChromiumError> {
        let pending: Vec<_> = self.net.interceptions.pending_requests().map(str::to_owned).collect();
        let mut places = None;
        for network in pending {
            let Some(lineage) = self.announced_worker_lineage(&network)? else { continue };
            if let Some(previous) = self.net.worker_request_owners.get(&network) {
                if previous != &lineage { return Err(ChromiumError::Shape("worker request changed owner".into())); }
                continue;
            }
            let issued = if self.net.browser_issued.contains(&network) {
                // Browser worker loads carry wall-clock Network timestamps.
                // As in issue_time, their unselected load holds virtual time;
                // read that stopped clock without consuming the announcement.
                self.read_time().await?.max(self.now)
            } else {
                *self.net.issued.get(&network).ok_or_else(|| ChromiumError::Shape("worker announcement has no issue instant".into()))?
            };
            // The creator document's place while the request is outstanding: read once
            // per freeze, when a new owner needs it.
            let places = match &mut places {
                Some(places) => places,
                None => places.insert(self.document_places().await?),
            };
            let document = places.get(&lineage.creator_document).cloned().unwrap_or(DocumentPlace::Retired);
            self.freeze_owner(network, lineage, issued, document)?;
        }
        Ok(())
    }

    /// Freeze the owners of recovered pauses whose announcements no recorder read: the engine names
    /// the issuing worker (`frontcapWorkerId`), whose lineage the live constructor inventory gives.
    /// A worker no longer in that inventory leaves its request unresolved.
    pub(super) async fn freeze_paused_worker_owners(&mut self, pauses: &[fetch::RequestPausedEvent]) -> Result<(), ChromiumError> {
        let mut places = None;
        for ev in pauses {
            let (Some(network), Some(worker), Some(stamp)) = (&ev.network_id, &ev.frontcap_worker_id, &ev.frontcap_issue_time_us) else { continue };
            if self.net.worker_request_owners.contains_key(&network.0) { continue }
            let Some(lineage) = self.target_lineage(worker)? else { continue };
            let stamp: u64 = stamp.parse().map_err(|_| ChromiumError::Shape("invalid engine issue stamp".into()))?;
            let issued = VirtualTime(stamp.saturating_sub(self.profile.initial_date_ms.saturating_mul(1000)));
            let places = match &mut places {
                Some(places) => places,
                None => places.insert(self.document_places().await?),
            };
            let document = places.get(&lineage.creator_document).cloned().unwrap_or(DocumentPlace::Retired);
            self.freeze_owner(network.0.clone(), lineage, issued, document)?;
        }
        Ok(())
    }

    fn freeze_owner(&mut self, network: String, lineage: WorkerLineage, issued: VirtualTime, document: DocumentPlace) -> Result<(), ChromiumError> {
        // Append before publishing the in-memory result: a failed durable
        // write must not let a retry skip this request's ownership record.
        self.bindings.append(&BindingEntry::WorkerRequestOwner {
            network_id: network.clone(), lineage: lineage.clone(), issued_us: issued.0, document: Some(document.clone()),
        })?;
        retain_owner(&mut self.net.worker_request_owners, &network, &lineage)?;
        self.net.worker_request_issues.insert(network.clone(), issued);
        self.net.worker_request_places.insert(network, document);
        Ok(())
    }

    pub(super) fn target_lineage(&self, target: &str) -> Result<Option<WorkerLineage>, ChromiumError> {
        let mut books = vec![self.subs.workers.clone()];
        // Closed pages may still have queued requests. Keep their constructor
        // evidence just as we retain it when a worker's session detaches.
        books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
        for book in &books {
            if let Some(lineage) = book.lock().unwrap().lineages.for_target(target)? { return Ok(Some(lineage)); }
        }
        Ok(None)
    }

    /// A retained held request supplies the old connection's missing stream
    /// boundary only when native wait identity and durable constructor ownership agree.
    pub(super) fn recovered_worker_wait(&self, target: &str, wait: &str) -> Result<bool, ChromiumError> {
        let Some(lineage) = self.target_lineage(target)? else { return Ok(false) };
        Ok(self.net.held.values().any(|held| held.released && held.network_id.as_ref().is_some_and(|network|
            network.rsplit_once('.').map(|(_, number)| number) == Some(wait)
                && self.net.recovered_network_requests.contains(network)
                && self.net.worker_request_owners.get(network) == Some(&lineage))))
    }

    /// The issuer of the announced request `network`: a worker with a frozen owner, any other request
    /// announced on a worker's session, or the main thread.
    pub(super) fn issuer_of(&self, network: &str) -> Issuer {
        if let Some(lineage) = self.net.worker_request_owners.get(network) {
            let document = self.net.worker_request_places.get(network).cloned().unwrap_or(DocumentPlace::Retired);
            return Issuer::Worker { document, constructors: lineage.constructors.clone() };
        }
        let mut books = vec![self.subs.workers.clone()];
        books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
        let on_worker = self.net.browser_worker_scripts.contains(network)
            || books.iter().any(|book| book.lock().unwrap().requests.contains_key(network));
        if on_worker { Issuer::UnresolvedWorker } else { Issuer::MainThread }
    }

    /// Each current document's place, from every window's frame tree: the launch page's, then those
    /// of the windows it opened, in opening order.
    async fn document_places(&mut self) -> Result<BTreeMap<String, DocumentPlace>, ChromiumError> {
        let launch = self.launch_page().id.clone();
        let mut windows = vec![(0, launch)];
        windows.extend(self.contexts.lock().unwrap().popups.iter().enumerate()
            .filter(|(_, p)| !p.closed && !p.held).map(|(i, p)| (i as u64 + 1, p.session_id.clone())));
        let mut places = BTreeMap::new();
        for (window, session) in windows {
            let session = Session::sharing_clock(self.page.client.clone(), session, self.page.service_clock());
            if self.shared_arrival_service_active() {
                let snapshot = session.call(&page::FrontcapGetDocumentPlacesParams::default()).await?;
                for document in snapshot.documents {
                    let frames = document.frame_path.into_iter().map(|i| u32::try_from(i)
                        .map_err(|_| ChromiumError::Shape("invalid browser document path".into())))
                        .collect::<Result<Vec<_>, _>>()?;
                    if places.insert(document.document_id, DocumentPlace::Current { window, frames }).is_some() {
                        return Err(ChromiumError::Shape("duplicate browser document identity".into()));
                    }
                }
                continue;
            }
            let tree = match self.serviced(session.call(&page::GetFrameTreeParams::default())).await {
                Ok(reply) => reply.frame_tree,
                // A window closing meanwhile has no current document left.
                Err(ChromiumError::Cdp(cdp::CdpError::SessionDetached)) => continue,
                Err(error) => return Err(error),
            };
            let mut frames = vec![(tree, Vec::new())];
            while let Some((node, path)) = frames.pop() {
                if let Some(document) = &node.frame.frontcap_document_id {
                    places.insert(document.clone(), DocumentPlace::Current { window, frames: path.clone() });
                }
                for (i, child) in node.child_frames.into_iter().flatten().enumerate() {
                    let mut child_path = path.clone();
                    child_path.push(u32::try_from(i).map_err(|_| ChromiumError::Shape("frame tree too wide".into()))?);
                    frames.push((child, child_path));
                }
            }
        }
        Ok(places)
    }

    fn announced_worker_lineage(&self, network: &str) -> Result<Option<WorkerLineage>, ChromiumError> {
        if let Some(lineage) = self.target_lineage(network)? { return Ok(Some(lineage)); }
        let mut books = vec![self.subs.workers.clone()];
        books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
        // A nested script is announced on the creator session. Never mistake
        // that session for the script's worker if target metadata is incomplete.
        if self.net.browser_worker_scripts.contains(network) { return Ok(None) }
        let Some(session) = self.subs.sent.session(network) else { return Ok(None) };
        for book in &books {
            if let Some(lineage) = book.lock().unwrap().lineages.for_session(&session)? { return Ok(Some(lineage)); }
        }
        let target = books.iter().find_map(|book| book.lock().unwrap().lineages.sessions.get(session.as_ref()).cloned());
        if let Some(target) = target {
            for book in books {
                if let Some(lineage) = book.lock().unwrap().lineages.for_target(&target)? { return Ok(Some(lineage)); }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "worker_lineage_tests.rs"]
mod tests;
