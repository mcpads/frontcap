//! Offline sealing of failed evidence. No engine connection or execution recovery is allowed here.
use crate::{control::ControlState, runtime::{self, RuntimeState}};
use frontcap_core::{bundle::{Bundle, Determinism, ExecutionMetadata, Finalization, Level, Manifest}, event::{EventKind, TaintRecord, TaintSource}, ids::LaunchId, log::BehaviorLog};
use std::path::{Path, PathBuf};

pub(crate) fn finalize(dir: &Path, id: &LaunchId) -> Result<Manifest, String> {
    let _lock = crate::take_lock(dir).map_err(|e| e.to_string())?.ok_or("the recorder still owns this execution")?;
    let result = finalize_owned(dir, id);
    let state = match &result {
        Ok(_) => Finalization::Finalized,
        Err(reason) => Finalization::Failed { reason: reason.clone() },
    };
    if let Err(error) = frontcap_core::state::save_atomic(&dir.join("finalization.json"), &state) {
        return Err(format!("{}; could not persist finalization state: {error}", result.as_ref().err().map(String::as_str).unwrap_or("manifest was finalized")));
    }
    crate::fault::point(id.as_str(), "finalize_after_status");
    result
}

fn finalize_owned(dir: &Path, id: &LaunchId) -> Result<Manifest, String> {
    let mut record = runtime::load_runtime(dir).map_err(|e| e.to_string())?.ok_or("missing runtime")?;
    let spec = runtime::load_spec(dir).map_err(|e| e.to_string())?.ok_or("missing launch spec")?;
    if &record.launch_id != id || &spec.launch_id != id { return Err("runtime and launch identities disagree".into()); }
    let browser_ended = record.browser.as_ref().ok_or("owned browser identity is unavailable")?.has_exited().map_err(|e| e.to_string())?;
    // The lock is free, so no recorder serves a ready execution: it ended without recording a stop or a
    // failure. With its browser gone too nothing can recover it, and it is recorded as failed.
    if record.state == RuntimeState::Ready {
        if !browser_ended { return Err("its recorder ended but its browser is alive; session reattach recovers it".into()); }
        record.state = RuntimeState::Failed { reason: "its recorder and browser ended without a stop".into(), error: None };
        runtime::save_runtime(dir, &record).map_err(|e| e.to_string())?;
    }
    let reason = match &record.state {
        RuntimeState::Failed { reason, .. } => reason.clone(),
        RuntimeState::Stopped => "offline finalization after incomplete stop".into(),
        _ => return Err("only a failed or stopped execution can be finalized offline".into()),
    };
    if !browser_ended {
        return Err("the owned browser is still alive; stop it before finalization".into());
    }
    let bundle = Bundle::open(&spec.runs_root, &spec.run_id).map_err(|e| e.to_string())?;
    let previous: Option<Manifest> = frontcap_core::state::load(&bundle.manifest_path()).map_err(|e| e.to_string())?;
    if let Some(m) = previous.filter(|m| m.finalization == Finalization::Finalized) {
        if m.run != spec.run_id || m.launch != *id { return Err("manifest identity mismatch".into()); }
        let inspection = bundle.inspect().map_err(|e| e.to_string())?;
        if !inspection.missing.is_empty() || !inspection.mismatched.is_empty() || !inspection.unexpected.is_empty() {
            return Err("finalized source evidence failed integrity validation".into());
        }
        return Ok(m);
    }
    let metadata: ExecutionMetadata = frontcap_core::state::load(&bundle.root().join("execution.json")).map_err(|e| e.to_string())?
        .ok_or("missing execution.json; cannot reconstruct the original execution identity")?;
    if metadata.run != spec.run_id || metadata.launch != *id || metadata.profile != spec.profile {
        return Err("execution metadata does not match launch spec".into());
    }
    let control: ControlState = frontcap_core::state::load(&runtime::control_path(dir)).map_err(|e| e.to_string())?.ok_or("missing control state")?;
    let events = BehaviorLog::read(&bundle.trace()).map_err(|e| e.to_string())?;
    let mut taints: Vec<_> = events.into_iter().filter_map(|e| match e.kind { EventKind::Taint(t) => Some(t), _ => None }).collect();
    taints.push(TaintRecord { source: TaintSource::RecorderFault, detail: reason });
    let mut missing = control.missing_inputs;
    missing.push("recorder_fault".into());
    missing.push("offline finalization: final renderer samples and buffered events are unavailable".into());
    missing.extend(control.pending_inputs.keys().map(|id| format!("inputs of {id}")));
    for (name, path) in [("trace.jsonl", bundle.trace()), ("journal.jsonl", runtime::journal_path(dir))] {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") { missing.push(format!("incomplete final line in {name}")); }
    }
    missing.sort(); missing.dedup();
    // Copy only retained control evidence, never proxy credentials or browser profiles. Byte-for-byte
    // copies keep incomplete journal tails; finalization must not repair or infer their outcomes.
    let sources: std::collections::BTreeMap<_, _> = ["runtime.json", "control.json", "journal.jsonl"].into_iter()
        .map(|name| std::fs::read(dir.join(name)).map(|bytes| (name, bytes))).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    // Hash the named content digests, so a changed retry retains a new coherent snapshot without
    // overwriting evidence from an earlier attempt. Partial copies resume in their original group.
    let digests: std::collections::BTreeMap<_, _> = sources.iter()
        .map(|(name, bytes)| (*name, frontcap_core::canonical::sha256_bytes_hex(bytes))).collect();
    let snapshot = format!("recovery/{}", frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(&digests)));
    let recovery = bundle.root().join(&snapshot);
    std::fs::create_dir_all(&recovery).map_err(|e| e.to_string())?;
    for (name, bytes) in sources {
        let path = recovery.join(name);
        match std::fs::read(&path) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => return Err(format!("retained recovery evidence changed: {name}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                frontcap_core::state::save_atomic_bytes(&path, &bytes).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
        crate::fault::point(id.as_str(), &format!("finalize_after_{name}"));
    }
    missing.extend(sync_sources(bundle.root())?);
    missing.sort(); missing.dedup();
    crate::fault::point(id.as_str(), "finalize_before_manifest");
    let manifest = bundle.finalize(Manifest {
        run: metadata.run, launch: metadata.launch, engine: metadata.engine, engine_revision: metadata.engine_revision,
        host_class: metadata.profile.host_class.clone(), profile: metadata.profile,
        determinism: Determinism { level: Level::None, taints }, replayable: false, missing_inputs: missing,
        finalization: Finalization::Open, recovery_snapshot: Some(snapshot), members: Default::default(),
    }).map_err(|e| e.to_string())?;
    crate::fault::point(id.as_str(), "finalize_after_manifest");
    Ok(manifest)
}

fn sync_sources(root: &Path) -> Result<Vec<String>, String> {
    let mut incomplete = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    let mut directories: Vec<PathBuf> = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() { return Err(format!("source evidence contains a symbolic link: {}", entry.path().display())); }
            if kind.is_dir() { pending.push(entry.path()); }
            else if kind.is_file() {
                if frontcap_core::state::is_atomic_temporary(&entry.path()) {
                    incomplete.push(format!("unpublished atomic write: {}", entry.path().strip_prefix(root).expect("under bundle").display()));
                }
                std::fs::File::open(entry.path()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            }
            else { return Err("source evidence contains a non-regular file".into()); }
        }
        directories.push(dir);
    }
    for dir in directories.into_iter().rev() { std::fs::File::open(dir).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?; }
    Ok(incomplete)
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod tests;
