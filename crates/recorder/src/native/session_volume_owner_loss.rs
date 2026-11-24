//! Read-only binding of an original volume owner's loss receipt. Never supplies a Stop answer.
use super::*;
use frontcap_core::native::ownership::Retirement;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Registration {
    operation: String,
    session: PathBuf,
    worker: ProcessIdentity,
    ready_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retirement_reader: Option<PathBuf>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    registration: Registration,
    diagnostic: PathBuf,
    diagnostic_process: ProcessIdentity,
    diagnostic_config_sha256: String,
    target_pids: [u32; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retirement_reader_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_sha256: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
    binding: Binding,
    pids_absent: Vec<u32>,
    session_result_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    independent_retirement_sha256: Option<String>,
    diagnostic_result_sha256: String,
    retirement: Retirement,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acknowledged { request_sha256: String }

fn digest(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

pub(super) fn verify(runtime: &Path, launch: &LaunchId, config: &Config, proof: &Proof)
    -> io::Result<frontcap_native::storage_volume::RetirementEvidence> {
    let directory = runtime.join("native-session-volume"); let worker = directory.join("worker");
    let binding: Binding = load(&worker.join("watch-uikit.json"))?
        .ok_or_else(|| io::Error::other("original UIKit watch registration missing"))?;
    let ack: Acknowledged = load(&worker.join("watch-ready.json"))?
        .ok_or_else(|| io::Error::other("original UIKit watch acknowledgement missing"))?;
    validate(&worker, launch, proof, &binding, &ack)?;
    // This derives content/readback evidence from the original volume worker's receipt.
    // It grants no live device, process, volume or ordinary Stop authority.
    frontcap_native::storage_volume::read_retained_receipt(&directory, config)
}

fn validate(worker: &Path, launch: &LaunchId, proof: &Proof, binding: &Binding, ack: &Acknowledged) -> io::Result<()> {
    let registration = &binding.registration; let session = worker.join("volume/mount").join(launch.as_str());
    let mut bytes = serde_json::to_vec(registration)?;bytes.push(b'\n');
    let pids = [registration.worker.pid, binding.diagnostic_process.pid, binding.target_pids[0], binding.target_pids[1]];
    let reader_valid = match (&registration.retirement_reader, &binding.retirement_reader_sha256, &binding.launch_sha256) {
        (None, None, None) => proof.independent_retirement_sha256.is_none(),
        (Some(path), Some(reader), Some(launch)) => path.is_absolute() && digest(reader) && digest(launch),
        _ => false,
    };
    let source_valid = proof.session_result_sha256.as_ref().is_none_or(|value| digest(value))
        && proof.independent_retirement_sha256.as_ref().is_none_or(|value| digest(value))
        && (proof.session_result_sha256.is_some() || proof.independent_retirement_sha256.is_some());
    if !reader_valid || !source_valid || proof.binding != *binding || registration.operation != "watch_uikit" || registration.session != session
        || binding.diagnostic.parent() != Some(session.join("worker.log.diagnostics").as_path())
        || binding.diagnostic.file_name().is_none() || pids.contains(&0)
        || registration.worker.started.is_empty() || binding.diagnostic_process.started.is_empty()
        || proof.pids_absent != pids || !proof.retirement.passed || !proof.retirement.errors.is_empty()
        || ![&registration.ready_sha256, &binding.diagnostic_config_sha256,
            &proof.diagnostic_result_sha256].into_iter().all(|value| digest(value))
        || ack.request_sha256 != frontcap_core::canonical::sha256_bytes_hex(&bytes) {
        return Err(io::Error::other("owner-loss readback differs from original UIKit registration/retirement"));
    }
    Ok(())
}

// Acquisition registration precedes target readiness, so it has no ready hash or frozen app PIDs.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AcquisitionRegistration {
    operation: String,
    session: PathBuf,
    worker: ProcessIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retirement_reader: Option<PathBuf>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AcquisitionBinding {
    registration: AcquisitionRegistration,
    diagnostic: PathBuf,
    diagnostic_process: ProcessIdentity,
    diagnostic_config_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retirement_reader_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionProof {
    binding: AcquisitionBinding,
    pids_absent: Vec<u32>,
    session_result_sha256: Option<String>,
    diagnostic_result_sha256: String,
    result: Option<frontcap_native::ios_session::SessionResult>,
    #[serde(default)]
    independent_retirement_sha256: Option<String>,
    #[serde(default)]
    retirement: Option<Retirement>,
}

/// Historical target quiescence only. Does not depend on a mounted session or retained image bytes,
/// prove volume/service disposal, supply a Stop answer, or permit recreating any live owner.
pub(super) fn acquisition_target(runtime: &Path, spec: &LaunchSpec) -> io::Result<Option<Retirement>> {
    let worker = runtime.join("native-session-volume/worker");
    let Some(proof): Option<AcquisitionProof> = load(&worker.join("owner-loss-acquisition-retired.json"))? else { return Ok(None); };
    let target::Launch::Ios(target) = &spec.target else { return Err(io::Error::other("UIKit acquisition proof has a different platform")); };
    let config = spec.session_volume.as_ref().ok_or_else(|| io::Error::other("acquisition volume configuration missing"))?;
    let plan: Config = load(&runtime.join("native-session-volume-plan.json"))?
        .ok_or_else(|| io::Error::other("acquisition volume plan missing"))?;
    if &plan != config { return Err(io::Error::other("acquisition volume differs from original launch")); }
    let binding: AcquisitionBinding = load(&worker.join("watch-acquisition.json"))?
        .ok_or_else(|| io::Error::other("original acquisition watch missing"))?;
    let ack: Acknowledged = load(&worker.join("watch-acquisition-ready.json"))?
        .ok_or_else(|| io::Error::other("original acquisition watch acknowledgement missing"))?;
    let registration = &binding.registration;
    let session = worker.join("volume/mount").join(spec.launch_id.as_str());
    let mut bytes = serde_json::to_vec(registration)?; bytes.push(b'\n');
    let reader = match (&registration.retirement_reader, &binding.retirement_reader_sha256, &binding.launch_sha256) {
        (None, None, None) => proof.independent_retirement_sha256.is_none(),
        (Some(path), Some(binary), Some(launch)) => path.is_absolute() && digest(binary) && digest(launch)
            && target.resource_lifetime_ms.is_some_and(|ms| (1..=900_000).contains(&ms)),
        _ => false,
    };
    if proof.binding != binding || registration.operation != "watch_uikit_acquisition" || registration.session != session
        || binding.diagnostic.parent() != Some(session.join("worker.log.diagnostics").as_path())
        || binding.diagnostic.file_name().is_none() || registration.worker.started.is_empty() || binding.diagnostic_process.started.is_empty()
        || !matches!(proof.pids_absent.len(), 2 | 4) || proof.pids_absent.contains(&0)
        || !proof.pids_absent.starts_with(&[registration.worker.pid, binding.diagnostic_process.pid])
        || !reader || !digest(&binding.diagnostic_config_sha256) || !digest(&proof.diagnostic_result_sha256)
        || proof.session_result_sha256.as_ref().is_some_and(|value| !digest(value))
        || proof.independent_retirement_sha256.as_ref().is_some_and(|value| !digest(value))
        || proof.result.is_some() != proof.session_result_sha256.is_some()
        || ack.request_sha256 != frontcap_core::canonical::sha256_bytes_hex(&bytes) {
        return Err(io::Error::other("acquisition readback differs from original registration/retirement"));
    }
    let retirement = match (proof.result, proof.retirement, proof.independent_retirement_sha256) {
        (Some(result), None, None) if !result.acquisition_not_started => result.retirement,
        // A no-acquisition outcome is not a target-retirement result.
        (Some(result), None, None) if result.acquisition_not_started && result.retirement.is_none() && result.error.is_some() => return Ok(None),
        (result, Some(retirement), Some(_)) if result.as_ref().is_none_or(|r| !r.acquisition_not_started) => Some(retirement),
        _ => None,
    };
    retirement.filter(|r| r.passed && r.errors.is_empty()).map(Some)
        .ok_or_else(|| io::Error::other("acquisition target retirement is unproven"))
}

#[cfg(test)]
#[path = "session_volume_owner_loss_tests.rs"]
mod tests;
