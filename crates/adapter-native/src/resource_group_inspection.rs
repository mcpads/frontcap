//! Read original group retirement without acquiring, signalling or recreating target resources.
use super::*;

pub(crate) enum Inspection { Active, Finished(Box<Retirement>), Unresolved }

pub(crate) fn inspect(directory: &Path, target: &Path, ports: &[u16], volume: Option<&crate::storage_volume::Config>) -> io::Result<Inspection> {
    inspect_with_volume_root(directory, target, ports, volume, None)
}

pub(crate) fn inspect_with_volume_root(directory: &Path, target: &Path, ports: &[u16],
    volume: Option<&crate::storage_volume::Config>, volume_root: Option<&Path>) -> io::Result<Inspection> {
    let Some(config) = load::<Config>(&directory.join("config.json"))? else { return Ok(Inspection::Unresolved); };
    // Match the worker's frozen target and configured ports before consulting any group's outcome.
    if config.target != target || config.ports != ports || config.scratch_volume.as_ref() != volume || config.scratch_volume_root.as_deref() != volume_root {
        return Err(io::Error::other("resource group differs from original native target"));
    }
    let _lock = match lock(directory) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(Inspection::Active),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Inspection::Unresolved),
        Err(error) => return Err(error),
    };
    let Some(result) = load::<Retirement>(&directory.join("retirement.json"))? else { return Ok(Inspection::Unresolved); };
    let ready: Ready = load(&directory.join("ready.json"))?
        .ok_or_else(|| io::Error::other("resource group readiness binding absent"))?;
    if ready.target != target || !ready.target.starts_with(&ready.evidence_root)
        || ready.evidence_root != config.evidence_root.clone().unwrap_or_else(|| target.to_owned()) {
        return Err(io::Error::other("resource group retirement target differs"));
    }
    if result.passed {
        if let Some(server) = &result.adb_server {
            let original: AdbServer = load(&directory.join("adb-server.json"))?
                .ok_or_else(|| io::Error::other("original ADB server binding missing"))?;
            if &original != server || server.port == 0 || server.port == 5037
                || !result.resources.contains_key(server.id.as_str()) {
                return Err(io::Error::other("ADB server retirement binding differs"));
            }
        } else if directory.join("adb-server.json").exists() {
            return Err(io::Error::other("ADB server retirement binding omitted"));
        }
        match (volume, &result.scratch_volume) {
            (Some(plan), Some(evidence)) => crate::storage_volume::inspect(&config.scratch_volume_directory(directory), plan, evidence)?,
            (None, None) => (),
            _ => return Err(io::Error::other("original scratch-volume retirement binding missing or unexpected")),
        }
        let cleanup: Retirement = load(&directory.join("cleanup.json"))?
            .ok_or_else(|| io::Error::other("original resource cleanup proof missing"))?;
        if !result.errors.is_empty() || serde_json::to_value(&cleanup)? != serde_json::to_value(&result)? {
            return Err(io::Error::other("resource retirement differs from original cleanup proof"));
        }
        if let Some(simulator) = &result.simulator {
            crate::simulator::data::inspect_retirement(&target.join("simulator"))?;
            let original: frontcap_core::native::ownership::Retirement = load(&target.join("simulator/retirement.json"))?
                .ok_or_else(|| io::Error::other("original Simulator retirement proof missing"))?;
            if !simulator.passed || !simulator.errors.is_empty() || serde_json::to_value(simulator)? != serde_json::to_value(original)? {
                return Err(io::Error::other("resource retirement differs from original Simulator proof"));
            }
        }
        for (id, resource) in &result.resources {
            let id = RequestId::new(id).map_err(io::Error::other)?;
            let original: ResultRecord = load(&directory.join("resources").join(id.as_str()).join("result.json"))?
                .ok_or_else(|| io::Error::other("original resource retirement proof missing"))?;
            if resource.retirement_error.is_some() || serde_json::to_value(resource)? != serde_json::to_value(original)? {
                return Err(io::Error::other("resource retirement differs from original child proof"));
            }
        }
        for proof in result.helper_evidence.values() {
            if !proof.path.is_absolute() || !proof.path.canonicalize()?.starts_with(&ready.evidence_root)
                || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&proof.path)?) != proof.sha256 {
                return Err(io::Error::other("original helper retirement evidence changed"));
            }
            crate::process::validate_exchange_files(&proof.path)?;
        }
        for (id, forward) in &result.forwarding {
            let id = RequestId::new(id).map_err(io::Error::other)?;
            let original: ForwardRetirement = load(&directory.join("forwarding").join(id.as_str()).join("retirement.json"))?
                .ok_or_else(|| io::Error::other("original forward retirement proof missing"))?;
            if matches!(forward.disposition, ForwardDisposition::OwnedServerRetired) {
                let intent: ForwardRegistration = load(&directory.join("forwarding").join(id.as_str()).join("intent.json"))?
                    .ok_or_else(|| io::Error::other("original forward intent missing"))?;
                let server = result.adb_server.as_ref()
                    .ok_or_else(|| io::Error::other("forward has no owned server retirement"))?;
                if intent.port != forward.port || intent.server_port != Some(server.port)
                    || intent.write_root.as_ref().is_some_and(|root| root != &ready.temporary_disks)
                    || result.resources.get(server.id.as_str()).is_none_or(|proof| proof.process.is_none()) {
                    return Err(io::Error::other("forward differs from retired owned server"));
                }
            }
            if !forward.passed || !forward.errors.is_empty() || serde_json::to_value(forward)? != serde_json::to_value(original)? {
                return Err(io::Error::other("resource retirement differs from original forward proof"));
            }
        }
    }
    // Do not probe/release the host lease or ports: a later owned session may legitimately use them.
    Ok(Inspection::Finished(Box::new(result)))
}

// The inspection owns only this existing lock, never target resource ownership.
pub(super) fn lock(directory: &Path) -> io::Result<Option<crate::store::RequestLease>> {
    let lock = File::options().read(true).write(true).open(directory.join("group.lock"))?;
    match crate::store::RequestLease::try_acquire(lock) {
        Ok(lease) => Ok(Some(lease)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(io::Error::other(error)),
    }
}
