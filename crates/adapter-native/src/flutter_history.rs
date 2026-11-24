//! Immutable per-transition payloads, committed by a small atomic head.
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Head {
    pub owner: Owner,
    pub event_count: u64,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored { Events(Head), Snapshot(State) }

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Event {
    Request { index: usize, entry: Entry },
    External { entry: ExternalEntry },
    System { entry: ExternalEntry },
}
impl Event {
    pub fn between(old: &State, new: &State) -> Result<Self, StoreError> {
        if old.owner != new.owner { return Err(StoreError::IdentityChanged); }
        if new.external_entries.len() == old.external_entries.len() + 1
            && new.external_entries[..old.external_entries.len()] == old.external_entries
            && new.system_entries == old.system_entries && new.entries == old.entries {
            return Ok(Self::External { entry: new.external_entries.last().unwrap().clone() });
        }
        if new.system_entries.len() == old.system_entries.len() + 1
            && new.system_entries[..old.system_entries.len()] == old.system_entries
            && new.external_entries == old.external_entries && new.entries == old.entries {
            return Ok(Self::System { entry: new.system_entries.last().unwrap().clone() });
        }
        if old.external_entries != new.external_entries || old.system_entries != new.system_entries {
            return Err(StoreError::StorageUncertain);
        }
        let changed: Vec<_> = new.entries.iter().enumerate().filter(|(i,e)| old.entries.get(*i) != Some(e)).collect();
        if changed.len() != 1 || new.entries.len() < old.entries.len() || new.entries.len() > old.entries.len() + 1 {
            return Err(StoreError::StorageUncertain);
        }
        let (index, entry) = changed[0];
        Ok(Self::Request { index, entry: entry.clone() })
    }
    fn apply(self, state: &mut State) -> Result<(), StoreError> {
        match self {
            Self::Request { index, entry } => {
                if index == state.entries.len() && matches!(entry.progress, Progress::Prepared {}) {
                    state.entries.push(entry);
                } else {
                    let old = state.entries.get_mut(index).ok_or(StoreError::StorageUncertain)?;
                    let valid = matches!((&old.progress, &entry.progress),
                        (Progress::Prepared {}, Progress::Dispatched {}) |
                        (Progress::Dispatched {}, Progress::Completed { .. }));
                    if old.id != entry.id || old.request != entry.request || !valid {
                        return Err(StoreError::StorageUncertain);
                    }
                    *old = entry;
                }
            }
            Self::External { entry } => state.external_entries.push(entry),
            Self::System { entry } => state.system_entries.push(entry),
        }
        state.validate()?;
        Ok(())
    }
}
pub(super) fn read(directory: &Path, owner: &Owner) -> Result<(State, Option<u64>), StoreError> {
    let saved: Stored = load(&directory.join("requests.json"))?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "missing Flutter request state"))?;
    let (state, count) = match saved {
        Stored::Snapshot(state) => (state, None),
        Stored::Events(head) => {
            if &head.owner != owner { return Err(StoreError::IdentityChanged); }
            let mut state = State { owner: head.owner, entries: vec![], external_entries: vec![], system_entries: vec![] };
            state.validate()?;
            for index in 0..head.event_count {
                let event: Event = load(&directory.join("events").join(format!("{index}.json")))?
                    .ok_or(StoreError::StorageUncertain)?;
                event.apply(&mut state)?;
            }
            (state, Some(head.event_count))
        }
    };
    if &state.owner != owner { return Err(StoreError::IdentityChanged); }
    state.validate()?;
    Ok((state, count))
}
