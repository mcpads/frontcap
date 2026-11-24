//! Detached observation readback; derived exports never change original execution evidence.
use super::*;
use frontcap_core::native::{ios_request::Completion as IosCompletion, ios_ui::UiOutcome, observation::Artifact};
use std::{io::Read, os::unix::fs::OpenOptionsExt, process::Command, sync::atomic::AtomicBool};

#[derive(Serialize)]
struct Export<'a> {
    image: PathBuf,
    image_identity: &'a frontcap_native::storage_volume::RetainedImage,
    artifact: &'a Artifact,
    directory: PathBuf,
    uikit_session: &'a LaunchId,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exported { passed: bool, copy_removed: bool, artifact_exported: bool }

#[derive(Serialize)]
enum Readback {
    Stop(Completion),
    OwnerLoss { proof: Box<owner_loss::Proof>, volume: frontcap_native::storage_volume::RetirementEvidence },
}
impl Readback {
    fn volume(&self) -> &frontcap_native::storage_volume::RetirementEvidence {
        match self { Self::Stop(record) => &record.volume, Self::OwnerLoss { volume, .. } => volume }
    }
}

/// Return None for an ordinary/live session. A completed volume receipt instead requires verified
/// offline readback; a failure cannot silently fall back to an unrelated live path.
pub fn retained_ios_png(runtime: &Path, launch: &LaunchId, request: &RequestId, outcome: &UiOutcome,
    exports: &Path, reader: Option<&Path>) -> io::Result<Option<Vec<u8>>> {
    let stop: Option<Completion> = load(&runtime.join("native-session-volume-retirement.json"))?;
    let loss: Option<owner_loss::Proof> = load(&runtime.join("native-session-volume/worker/owner-loss-writers-retired.json"))?;
    if stop.is_none() && loss.is_none() { return Ok(None); }
    let spec: LaunchSpec = load(&super::super::spec_path(runtime))?
        .ok_or_else(|| io::Error::other("retained observation launch spec missing"))?;
    let config = spec.session_volume.as_ref().ok_or_else(|| io::Error::other("retained observation volume configuration missing"))?;
    let plan: Config = load(&runtime.join("native-session-volume-plan.json"))?
        .ok_or_else(|| io::Error::other("retained observation volume plan missing"))?;
    if &spec.launch_id != launch || &plan != config {
        return Err(io::Error::other("retained observation retirement differs from launch"));
    }
    let record = match (stop, loss) {
        (Some(record), None) if record.launch_id == spec.launch_id && record.retirement.passed => Readback::Stop(record),
        (None, Some(proof)) => {
            let volume = owner_loss::verify(runtime, launch, config, &proof)?;
            Readback::OwnerLoss { proof: Box::new(proof), volume }
        }
        _ => return Err(io::Error::other("retained observation has conflicting or failed retirement receipts")),
    };
    let Some((_, RequestState::Completed(answer))) = Journal::read_record(&runtime::journal_path(runtime), request).map_err(io::Error::other)? else {
        return Err(io::Error::other("retained PNG requires its completed original observation"));
    };
    validate_answer(answer, request, outcome)?;
    let (artifacts, capture) = match outcome {

        UiOutcome::ObservedTargets { artifacts, catalog, .. } => (artifacts, &catalog.observation),
        UiOutcome::SystemAlertsObserved { artifacts, catalog } => (artifacts, &catalog.observation),
        _ => return Err(io::Error::other("retained observation has no PNG")),
    };
    if !frontcap_core::native::ios_ui::valid_observation_artifacts(artifacts)
        || artifacts[0].path != format!("observations/{capture}/screen.png") || artifacts[0].bytes > 16 * 1024 * 1024 {
        return Err(io::Error::other("retained observation artifact inventory differs"));
    }
    let artifact = &artifacts[0];
    let image = record.volume().retained_image.as_ref().ok_or_else(|| io::Error::other("retained image proof absent"))?;
    let key = frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(
        &(&spec.launch_id, request, &record, artifact)));
    fs::create_dir_all(exports)?;
    let directory = exports.join(key);
    match fs::create_dir(&directory) { Ok(()) => (), Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (), Err(e) => return Err(e) }
    if !fs::symlink_metadata(&directory)?.is_dir() { return Err(io::Error::other("linked export directory")); }
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(directory.join("lock"))?;
    lock.lock()?;
    let output = directory.join("export");
    if output.exists() {
        if !fs::symlink_metadata(&output)?.is_dir() { return Err(io::Error::other("linked export output")); }
        let complete: Exported = load(&output.join("result.json"))?
            .ok_or_else(|| io::Error::other(format!("retained export incomplete; inspect {}", directory.display())))?;
        if !complete.passed || !complete.copy_removed || !complete.artifact_exported {
            return Err(io::Error::other(format!("retained export lacks cleanup proof; inspect {}", directory.display())));
        }
        return read_png(&output.join("artifact"), artifact).map(Some);
    }
    if directory.join("exchange").exists() { return Err(io::Error::other(format!("retained export attempt unresolved; inspect {}", directory.display()))); }
    frontcap_native::storage_volume::inspect_retained(&runtime.join("native-session-volume"), config, record.volume())?;
    let image_path = runtime.join("native-session-volume/worker/volume").join(if image.gzip.is_some() { "bounded.dmg.gz" } else { "bounded.dmg" });
    let helper = reader.map(Path::to_path_buf).unwrap_or_else(|| config.worker.with_file_name("native_retained_artifact.py"));
    if !helper.is_absolute() || !helper.is_file() { return Err(io::Error::other("retained artifact reader must be an installed absolute file")); }
    let mut command = Command::new(&config.python);
    command.arg(&helper).arg("--bridge").env("PYTHONDONTWRITEBYTECODE", "1");
    let exchange: frontcap_native::process::Exchange<Exported> = frontcap_native::process::exchange_typed_recorded(
        &mut command, &Export { image: image_path, image_identity: image, artifact, directory: output.clone(), uikit_session: &spec.launch_id },
        Instant::now() + Duration::from_secs(60), &AtomicBool::new(false), 64 * 1024, &directory.join("exchange"))?;
    if exchange.failure.is_some() || exchange.cleanup_failure.is_some() || exchange.evidence_failure.is_some()
        || exchange.exit_code != Some(0) || !exchange.reply.is_some_and(|r| r.passed && r.copy_removed && r.artifact_exported) {
        return Err(io::Error::other(format!("retained artifact export failed; inspect {}", directory.display())));
    }
    read_png(&output.join("artifact"), artifact).map(Some)
}

fn read_png(path: &Path, artifact: &Artifact) -> io::Result<Vec<u8>> {
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != artifact.bytes { return Err(io::Error::other("retained PNG size/type differs")); }
    let mut bytes = Vec::new(); file.take(artifact.bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != artifact.bytes || frontcap_core::canonical::sha256_bytes_hex(&bytes) != artifact.sha256
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(io::Error::other("retained PNG differs from original observation")); }
    Ok(bytes)
}

fn validate_answer(answer: serde_json::Value, request: &RequestId, outcome: &UiOutcome) -> io::Result<()> {
    let Answer::Ok(Reply::Native { body: NativeReply::Done { request_id, outcome: saved } }) = serde_json::from_value(answer)? else {
        return Err(io::Error::other("retained observation answer differs"));
    };
    if &request_id != request || !matches!(saved.as_ref(), Outcome::IosCompleted { completion: IosCompletion::Ui(c) } if &c.outcome == outcome) {
        return Err(io::Error::other("retained observation differs from original request"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_volume_artifact_tests.rs"]
mod tests;
