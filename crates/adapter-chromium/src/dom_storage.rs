//! Native DOM storage for attached targets. The browser reads the storage service
//! once every renderer of the partition, a retired one included, has confirmed its
//! writes; mapping targets to logical contexts remains the caller's.
use crate::{ChromiumError, session::Session, encode::{base64, unbase64, canonical_origin_host}};
use cdp::protocol::{page, storage};
use frontcap_core::{dom_storage::{DomStorageArea, DomStorageEntry, DomStorageEncoding, DomStorageState, SessionStorageState}, storage::StoragePartition};
use std::collections::{BTreeMap, BTreeSet};

pub struct TargetDomStorage {
    pub local: Vec<DomStorageArea>,
    pub session: SessionStorageState,
    pub has_session_namespace: bool,
    pub frame_id: String,
    pub loader_id: String,
    pub window_name: String,
    pub url: String,
    /// Current native opener, which may differ from creation provenance.
    pub opener_frame_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageWindow {
    pub context: String,
    pub frame_id: String,
    pub loader_id: String,
    pub name: String,
    pub url: String,
    pub opener_frame_id: Option<String>,
}

#[derive(Debug)]
pub struct LiveDomStorage {
    pub storage: DomStorageState,
    pub windows: Vec<StorageWindow>,
}

/// Construction inputs for current live top-level windows and their DOM storage.
/// This does not include document, history, or retired-writer state.
#[derive(Debug)]
pub struct ContextStorageInputs {
    pub url: String,
    pub window_name: String,
    pub contexts: Vec<frontcap_core::profile::InitialContext>,
    pub storage: DomStorageState,
}

impl TargetDomStorage {
    fn window(&self) -> StorageWindow {
        StorageWindow {
            context: self.session.context.clone(), frame_id: self.frame_id.clone(),
            loader_id: self.loader_id.clone(), name: self.window_name.clone(),
            url: self.url.clone(), opener_frame_id: self.opener_frame_id.clone(),
        }
    }
}

fn invalid(message: &str) -> ChromiumError {
    ChromiumError::InvalidArgument(message.into())
}

/// Validate construction inputs before starting a browser. Popup reconstruction
/// needs an explicit topology; accepting its session maps here would lose owners.
pub fn validate_initial_dom_storage(profile: &frontcap_core::profile::DeterminismProfile) -> Result<(), ChromiumError> {
    let mut owners = BTreeSet::from([crate::generation::PAGE.to_string()]);
    if !profile.initial_contexts.is_empty() && profile.popups != frontcap_core::profile::PopupPolicy::Controlled {
        return Err(invalid("initial contexts require controlled popups"));
    }
    let mut closed = BTreeSet::new();
    for (index, context) in profile.initial_contexts.iter().enumerate() {
        if !owners.contains(&context.opener) {
            return Err(invalid("initial context opener must name page or an earlier context"));
        }
        if context.closed {
            // Navigating it would run the application in a window the source no longer had.
            if context.url != "about:blank" {
                return Err(invalid("a closed initial context stays at about:blank"));
            }
            closed.insert(format!("popup-{}", index + 1));
        } else if context.url != "about:blank" {
            let origin = crate::encode::origin_of(&context.url)
                .ok_or_else(|| invalid("initial context URL must be HTTP, HTTPS or about:blank"))?;
            canonical_origin_host(&origin)?;
        }
        owners.insert(format!("popup-{}", index + 1));
    }
    let Some(state) = &profile.initial_dom_storage else { return Ok(()); };
    if profile.initial_storage.is_some() {
        return Err(invalid("initial_dom_storage and initial_storage cannot both be supplied"));
    }
    state.validate().map_err(|e| invalid(&e.to_string()))?;
    if state.sessions.iter().any(|session| !owners.contains(&session.context)) {
        return Err(ChromiumError::Unsupported("initial DOM storage popup owners require context topology restoration".into()));
    }
    if state.sessions.iter().any(|session| closed.contains(&session.context)) {
        return Err(invalid("a closed initial context has no session storage"));
    }
    for area in state.local.iter().chain(state.sessions.iter().flat_map(|session| &session.areas)) {
        canonical_origin_host(&area.origin)?;
        if let Some(partition) = &area.partition {
            canonical_origin_host(&partition.top_level_site)?;
        }
    }
    Ok(())
}

fn decode_bytes(value: &cdp::Binary) -> Result<Vec<u8>, ChromiumError> {
    let bytes = unbase64(&value.0).ok_or_else(|| invalid("invalid native DOM storage base64"))?;
    if base64(&bytes) != value.0 {
        return Err(invalid("noncanonical native DOM storage base64"));
    }
    Ok(bytes)
}

fn decode_area(native: storage::FrontcapDOMStorageArea) -> Result<DomStorageArea, ChromiumError> {
    if native.nonce_partition {
        return Err(ChromiumError::Unsupported("nonce DOM storage identity cannot be reconstructed".into()));
    }
    canonical_origin_host(&native.origin)?;
    canonical_origin_host(&native.top_level_site)?;
    if native.first_party && native.has_cross_site_ancestor {
        return Err(invalid("contradictory native DOM storage ancestry"));
    }
    let mut keys = BTreeSet::new();
    let mut entries = Vec::with_capacity(native.entries.len());
    for entry in native.entries {
        let key = decode_bytes(&entry.key)?;
        let value = decode_bytes(&entry.value)?;
        if !keys.insert(key.clone()) {
            return Err(invalid("duplicate raw key in native DOM storage snapshot"));
        }
        entries.push(DomStorageEntry { key, value });
    }
    Ok(DomStorageArea {
        origin: native.origin,
        partition: Some(StoragePartition { top_level_site: native.top_level_site,
            has_cross_site_ancestor: Some(native.has_cross_site_ancestor) }),
        entries,
    })
}

async fn resolve_area(session: &Session, area: &DomStorageArea) -> Result<storage::SerializedStorageKey, ChromiumError> {
    session.call(&storage::FrontcapResolveStorageKeyParams {
        origin: area.origin.clone(),
        top_level_site: area.partition.as_ref().map(|p| p.top_level_site.clone()),
        has_cross_site_ancestor: area.partition.as_ref().and_then(|p| p.has_cross_site_ancestor),
    }).await.map(|r| r.storage_key)
}

async fn decode_areas(session: &Session, native: Vec<storage::FrontcapDOMStorageArea>) -> Result<Vec<DomStorageArea>, ChromiumError> {
    let mut seen = BTreeSet::new();
    let mut areas = Vec::with_capacity(native.len());
    for native in native {
        let native_key = native.storage_key.clone();
        let area = decode_area(native)?;
        let canonical = resolve_area(session, &area).await?;
        if canonical != native_key || !seen.insert(canonical.0) {
            return Err(invalid("native DOM storage scope identity mismatch or duplicate"));
        }
        areas.push(area);
    }
    Ok(areas)
}

/// Reads the native partition's local storage and this target's session namespace
/// once every renderer's writes have reached the storage service. The caller holds
/// the stopped boundary and accounts for the other contexts' session namespaces.
pub async fn read_target(session: &Session, context: &str) -> Result<TargetDomStorage, ChromiumError> {
    if context.is_empty() { return Err(invalid("DOM storage context name is empty")); }
    if cfg!(target_endian = "big") {
        return Err(ChromiumError::Unsupported("native DOM storage byte order is not supported".into()));
    }
    let before = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
    let native = session.call(&storage::FrontcapReadDomStorageParams::default()).await?;
    if native.session_namespace.is_none() && !native.session_storage.is_empty() {
        return Err(invalid("native session storage has values without a namespace"));
    }
    if native.frame_id != before.id {
        return Err(invalid("native DOM storage metadata names a different frame"));
    }
    let result = TargetDomStorage {
        local: decode_areas(session, native.local_storage).await?,
        session: SessionStorageState { context: context.into(),
            areas: decode_areas(session, native.session_storage).await? },
        has_session_namespace: native.session_namespace.is_some(),
        frame_id: native.frame_id.0,
        loader_id: before.loader_id.0.clone(),
        window_name: native.window_name,
        url: native.url,
        opener_frame_id: native.opener_frame_id.map(|id| id.0),
    };
    let after = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
    if before.id != after.id || before.loader_id != after.loader_id {
        return Err(invalid("document changed while reading DOM storage"));
    }
    Ok(result)
}

/// Combines an explicitly enrolled set of live targets from one generation.
/// The caller owns the stopped boundary; this does not infer that the supplied
/// target set is complete.
pub async fn read_live_targets(targets: &[(&str, &Session)]) -> Result<LiveDomStorage, ChromiumError> {
    if targets.is_empty() { return Err(invalid("DOM storage capture needs an enrolled target")); }
    let mut contexts = BTreeSet::new();
    let mut frame_ids = BTreeSet::new();
    let mut frames = Vec::with_capacity(targets.len());
    for (context, session) in targets {
        if context.is_empty() || !contexts.insert(*context) {
            return Err(invalid("DOM storage capture needs unique nonempty context names"));
        }
        let frame = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
        if !frame_ids.insert(frame.id.0.clone()) {
            return Err(invalid("multiple DOM storage owners name the same target"));
        }
        frames.push(frame);
    }
    let mut local = None;
    let mut local_identity = None;
    let mut sessions = Vec::with_capacity(targets.len());
    let mut windows = Vec::with_capacity(targets.len());
    for (context, session) in targets {
        let captured = read_target(session, context).await?;
        windows.push(captured.window());
        let identity = area_map(&captured.local);
        if let Some(expected) = &local_identity {
            if expected != &identity {
                return Err(invalid("shared local storage changed across enrolled targets"));
            }
        } else {
            local_identity = Some(identity);
            local = Some(captured.local);
        }
        sessions.push(captured.session);
    }
    for (index, ((context, session), before)) in targets.iter().zip(frames).enumerate() {
        let after = read_target(session, context).await?;
        if before.id.0 != after.frame_id || before.loader_id.0 != after.loader_id ||
            windows[index] != after.window() {
            return Err(invalid("enrolled window changed during DOM storage capture"));
        }
        if Some(area_map(&after.local)) != local_identity ||
            area_map(&after.session.areas) != area_map(&sessions[index].areas) {
            return Err(invalid("enrolled storage changed during DOM storage capture"));
        }
    }
    let state = DomStorageState { encoding: DomStorageEncoding::ChromiumLittleEndian,
        local: local.unwrap_or_default(), sessions };
    state.validate().map_err(|e| invalid(&e.to_string()))?;
    Ok(LiveDomStorage { storage: state, windows })
}

type AreaIdentity = (String, Option<StoragePartition>);
type RawEntries = BTreeMap<Vec<u8>, Vec<u8>>;
fn area_map(areas: &[DomStorageArea]) -> BTreeMap<AreaIdentity, RawEntries> {
    areas.iter().map(|area| ((area.origin.clone(), area.partition.clone()), entry_map(&area.entries))).collect()
}

/// Restores one construction-owned target. The caller must map the logical
/// session owner to this target and discard the child on any error; this is not
/// an atomic transaction across areas. Every scope is resolved before any write.
pub async fn restore_target(
    session: &Session, encoding: DomStorageEncoding,
    local: &[DomStorageArea], session_state: &SessionStorageState,
) -> Result<TargetDomStorage, ChromiumError> {
    if cfg!(target_endian = "big") {
        return Err(ChromiumError::Unsupported("native DOM storage byte order is not supported".into()));
    }
    match encoding { DomStorageEncoding::ChromiumLittleEndian => {} }
    DomStorageState { encoding, local: local.to_vec(), sessions: vec![session_state.clone()] }
        .validate().map_err(|e| invalid(&e.to_string()))?;
    let before = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
    let mut planned = Vec::new();
    let mut identities = BTreeSet::new();
    for (is_local, areas) in [(true, local), (false, session_state.areas.as_slice())] {
        for area in areas {
            canonical_origin_host(&area.origin)?;
            if let Some(partition) = &area.partition { canonical_origin_host(&partition.top_level_site)?; }
            let key = resolve_area(session, area).await?;
            if !identities.insert((is_local, key.0.clone())) {
                return Err(invalid("DOM storage inputs resolve to the same native area"));
            }
            planned.push((is_local, key, area));
        }
    }
    for (is_local, key, area) in &planned {
        let current = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
        if current.id != before.id || current.loader_id != before.loader_id {
            return Err(invalid("document changed before DOM storage restoration"));
        }
        let entries = area.entries.iter().map(|e| storage::FrontcapDOMStorageEntry {
            key: cdp::Binary(base64(&e.key)), value: cdp::Binary(base64(&e.value)),
        }).collect();
        session.call(&storage::FrontcapRestoreDomStorageAreaParams::new(
            key.clone(), *is_local, entries,
        )).await?;
    }
    let actual = read_target(session, &session_state.context).await?;
    let mut readback = BTreeMap::new();
    for (is_local, areas) in [(true, actual.local.as_slice()), (false, actual.session.areas.as_slice())] {
        for area in areas {
            let key = resolve_area(session, area).await?;
            readback.insert((is_local, key.0), entry_map(&area.entries));
        }
    }
    for (is_local, key, area) in planned {
        if readback.get(&(is_local, key.0)) != Some(&entry_map(&area.entries)) {
            return Err(invalid("DOM storage restoration readback differs from requested bytes"));
        }
    }
    let after = session.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
    if before.id != after.id || before.loader_id != after.loader_id {
        return Err(invalid("document changed during DOM storage restoration"));
    }
    Ok(actual)
}

fn entry_map(entries: &[DomStorageEntry]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    entries.iter().map(|entry| (entry.key.clone(), entry.value.clone())).collect()
}

#[cfg(test)]
#[path = "dom_storage_tests.rs"]
mod tests;
