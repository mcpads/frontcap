//! Read-only workspace inventory. Files and process probes are hints, never recovery or lease authority.
use crate::tools::ToolError;
use frontcap_core::ids::LaunchId;
use frontcap_recorder::{launcher::Workspace, runtime};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct Inventory {
    generations: Vec<Generation>,
    next_after: Option<LaunchId>,
    next_action: &'static str,
}

#[derive(Serialize)]
struct Generation {
    launch_id: LaunchId,
    state: &'static str,
    recorder_alive: Option<bool>,
    browser_alive: Option<bool>,
    /// The saved starting point a branch child was built from.
    #[serde(skip_serializing_if = "Option::is_none")]
    basis: Option<frontcap_core::ids::SavedId>,
    next_action: &'static str,
}

pub(crate) fn list(ws: &Workspace, after: Option<LaunchId>) -> Result<Inventory, ToolError> {
    let error = |e: std::io::Error| ToolError::new("inventory_unavailable", e.to_string(), "check workspace access and retry session list");
    let entries = match std::fs::read_dir(ws.root.join("runtimes")) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(error(e)),
    };
    let mut ids = std::collections::BTreeSet::new();
    if let Some(entries) = entries {
        for entry in entries {
            let entry = entry.map_err(error)?;
            if !entry.file_type().map_err(error)?.is_dir() { continue; }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
            let Ok(id) = LaunchId::new(name) else { continue; };
            if after.as_ref().is_none_or(|after| &id > after) {
                ids.insert(id);
                // Keep only the page and a lookahead, even for a large workspace.
                if ids.len() > 21 { ids.pop_last(); }
            }
        }
    }
    let more = ids.len() > 20;
    let generations: Vec<_> = ids.into_iter().take(20).map(|id| {
        let mut row = Generation {
            launch_id: id.clone(), state: "unavailable", recorder_alive: None, browser_alive: None,
            basis: runtime::load_spec(&ws.runtime_dir(&id)).ok().flatten().and_then(|spec| spec.origin).map(|origin| origin.saved_id),
            next_action: "inspect retained evidence; do not assume this target stopped or relaunch over it",
        };
        if let Ok(Some(record)) = runtime::load_runtime(&ws.runtime_dir(&id))
            && record.launch_id == id
        {
            let alive = record.recorder.has_exited().ok().map(|exited| !exited);
            row.recorder_alive = alive;
            row.browser_alive = record.browser.as_ref().and_then(|p| p.has_exited().ok().map(|exited| !exited));
            (row.state, row.next_action) = match record.state {
                runtime::RuntimeState::Stopped => ("stopped", "debug bundle; launch a new generation to execute"),
                runtime::RuntimeState::Failed { .. } => ("failed", "inspect retained evidence; stop then debug finalize if needed"),
                runtime::RuntimeState::Starting => ("starting", "retry session list; startup is not proven ready"),
                runtime::RuntimeState::Ready if alive == Some(true) => ("ready", "status for live ownership and execution; reattach only if available"),
                runtime::RuntimeState::Ready if alive == Some(false) && row.browser_alive == Some(false) =>
                    ("lost", "its recorder and browser ended: debug finalize seals the evidence; launch a new generation to go on"),
                runtime::RuntimeState::Ready if alive == Some(false) => ("recorder_unavailable", "session reattach to attempt recovery; do not assume the browser died"),
                runtime::RuntimeState::Ready => ("probe_unavailable", "retry process inspection; liveness and serviceability are unknown"),
            };
        }
        row
    }).collect();
    let next_after = more.then(|| generations.last().expect("full page").launch_id.clone());
    Ok(Inventory { generations, next_after, next_action: "status checks live ownership; session reattach acquires control explicitly" })
}
