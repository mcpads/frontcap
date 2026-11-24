//! Same-process UIKit host transitions, retaining process evidence separately from app receipts.
use frontcap_core::{
    ids::RequestId,
    native::{
        app_state::State,
        ios_lifecycle::Transition,
        ios_request::{Completion, Operation, Owner},
    },
    process::ProcessIdentity,
    state::{load, save_atomic},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { transition: Box<Transition> },
    Failed { error: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Before {
    owner: Owner,
    operation: Operation,
    process: ProcessIdentity,
}
fn attempt(root: &Path, id: &RequestId) -> PathBuf {
    root.join("app-state").join(id.as_str())
}
pub(crate) fn prepare(root: &Path, id: &RequestId, owner: &Owner, state: State) -> io::Result<()> {
    let operation = Operation::SetAppState {
        generation: owner.app.generation.clone(),
        state,
    };
    crate::ios_store::RequestStore::validate_operation(
        &root.join("requests"),
        owner,
        id,
        &operation,
    )
    .map_err(io::Error::other)?;
    let process = ProcessIdentity::of(owner.app.pid)?
        .filter(|p| p.is_alive())
        .ok_or_else(|| io::Error::other("UIKit process absent before host transition"))?;
    let path = attempt(root, id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::create_dir(&path)?;
    save_evidence(
        &path.join("before.json"),
        &Before {
            owner: owner.clone(),
            operation,
            process,
        },
    )
}
pub(crate) fn complete(
    root: &Path,
    id: &RequestId,
    owner: &Owner,
    state: State,
) -> io::Result<Transition> {
    let after = ProcessIdentity::of(owner.app.pid)?
        .filter(|p| p.is_alive())
        .ok_or_else(|| io::Error::other("UIKit process absent after host transition"))?;
    save_evidence(&attempt(root, id).join("after.json"), &after)?;
    let result = read_completion(root, id, owner, state)?
        .ok_or_else(|| io::Error::other("UIKit host transition evidence incomplete"))?;
    save_evidence(&attempt(root, id).join("receipt.json"), &result)?;
    Ok(result)
}
/// Original evidence only: no process inspection, native call, journal repair or process adoption.
pub fn read_completion(
    root: &Path,
    id: &RequestId,
    owner: &Owner,
    state: State,
) -> io::Result<Option<Transition>> {
    let path = attempt(root, id);
    let Some(before): Option<Before> = load(&path.join("before.json"))? else {
        return Ok(None);
    };
    let operation = Operation::SetAppState {
        generation: owner.app.generation.clone(),
        state,
    };
    if before.owner != *owner || before.operation != operation {
        return Err(io::Error::other("UIKit host transition binding differs"));
    }
    let Some(process_after) = load(&path.join("after.json"))? else {
        return Ok(None);
    };
    let Some(Completion::Ui(completion)) =
        crate::ios_execution::Executor::read_completion(root, owner, id, &operation)
            .map_err(io::Error::other)?
    else {
        return Ok(None);
    };
    let transition = Transition {
        owner: owner.clone(),
        process_before: before.process,
        process_after,
        completion: *completion,
    };
    transition.validate().map_err(io::Error::other)?;
    if let Some(saved) = load::<Transition>(&path.join("receipt.json"))?
        && saved != transition {
        return Err(io::Error::other(
            "UIKit original host transition evidence changed",
        ));
    }
    Ok(Some(transition))
}

fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    match WriteBudget::from_environment()? {
        Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
        None => save_atomic(path, value),
    }
}
