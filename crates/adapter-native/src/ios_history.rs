//! UIKit request deltas reuse the immutable-event/atomic-head layout used by Flutter.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Head {
    pub owner: Owner,
    pub first_ui: u64,
    pub event_count: u64,
    pub budget_sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored { Events(Head), Snapshot(State) }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Event {
    next_clock: u64,
    next_ui: u64,
    invalidated_through_ui: u64,
    binding: Option<(RequestId, Binding)>,
}
impl Event {
    pub fn between(old: &State, new: &State) -> Result<Self, StoreError> {
        if old.owner != new.owner || old.first_ui != new.first_ui { return Err(StoreError::IdentityChanged); }
        if old.bindings.keys().any(|id| !new.bindings.contains_key(id)) { return Err(StoreError::StorageUncertain); }
        let changed: Vec<_> = new.bindings.iter().filter(|(id,b)| old.bindings.get(*id) != Some(b)).collect();
        if changed.len() > 1 { return Err(StoreError::StorageUncertain); }
        Ok(Self { next_clock: new.next_clock, next_ui: new.next_ui,
            invalidated_through_ui: new.invalidated_through_ui,
            binding: changed.first().map(|(id,b)| ((*id).clone(), (*b).clone())) })
    }
    fn apply(self, state: &mut State, pending: &mut Option<RequestId>, finished: &mut bool) -> Result<(), StoreError> {
        let (mut clock, mut ui) = (state.next_clock, state.next_ui);
        if let Some((id, binding)) = self.binding {
            match state.bindings.get(&id) {
                None if binding.progress == Progress::Prepared && pending.is_none() && !*finished => {
                    let number = if binding.request.is_clock() { clock } else { ui };
                    if binding.request != Request::build_pair(&state.owner, &binding.operation, number, clock)? {
                        return Err(StoreError::StorageUncertain);
                    }
                    *pending = Some(id.clone());
                },
                Some(old) if old.operation == binding.operation && old.request == binding.request
                    && pending.as_ref() == Some(&id) && matches!((&old.progress, &binding.progress),
                        (Progress::Prepared, Progress::Dispatched) | (Progress::Dispatched, Progress::Completed { .. })) => (),
                _ => return Err(StoreError::StorageUncertain),
            }
            if self.invalidated_through_ui != state.invalidated_through_ui { return Err(StoreError::StorageUncertain); }
            if let Progress::Completed { receipt } = &binding.progress {
                receipt.validate(&state.owner, &binding.request)?;
                if binding.request.is_clock() { clock += 1; } else {
                    ui += 1;
                    if matches!(receipt, Completion::Press { clock: frontcap_core::native::ios_request::PressClock::Delivered { .. }, .. }) { clock += 1; }
                }
                *pending = None;
                *finished = binding.operation == Operation::Finish;
            }
            state.bindings.insert(id, binding);
        } else if pending.is_some() || *finished || self.invalidated_through_ui != state.next_ui - 1 {
            return Err(StoreError::StorageUncertain);
        }
        if self.next_clock != clock || self.next_ui != ui
            || self.invalidated_through_ui < state.invalidated_through_ui { return Err(StoreError::StorageUncertain); }
        state.next_clock = self.next_clock;
        state.next_ui = self.next_ui;
        state.invalidated_through_ui = self.invalidated_through_ui;
        Ok(())
    }
}
pub(super) fn read(directory: &Path, owner: &Owner) -> Result<(State, Option<Head>), StoreError> {
    let saved: Stored = load(&directory.join("requests.json"))?.ok_or_else(||
        std::io::Error::new(std::io::ErrorKind::NotFound, "missing iOS request state"))?;
    let (state, head) = match saved {
        Stored::Snapshot(state) => (state, None),
        Stored::Events(head) => {
            if &head.owner != owner { return Err(StoreError::IdentityChanged); }
            let mut state = State { owner: head.owner.clone(), next_clock: 1, next_ui: head.first_ui,
                first_ui: head.first_ui, invalidated_through_ui: 0, bindings: BTreeMap::new() };
            state.validate()?;
            let mut pending = None;
            let mut finished = false;
            for index in 0..head.event_count {
                let event: Event = load(&directory.join("events").join(format!("{index}.json")))?
                    .ok_or(StoreError::StorageUncertain)?;
                event.apply(&mut state, &mut pending, &mut finished)?;
            }
            (state, Some(head))
        }
    };
    if &state.owner != owner { return Err(StoreError::IdentityChanged); }
    state.validate()?;
    Ok((state, head))
}
