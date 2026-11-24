//! Idle supervision uses the same journalled Stop for storage pressure and worker loss.
use super::*;
use frontcap_recorder_protocol::native::Ready;

#[derive(Serialize)]
struct RetirementRequest<'a> {
    reason: &'a str,
    retirement_request: &'a RequestId,
}

/// A departed worker can retire its registered roots independently of recorder Stop.
/// Its identity is evidence of lost authority, never permission to signal or adopt a target.
pub(super) fn worker_exited(rec: &Recorder) -> io::Result<bool> {
    let ready = rec.state.lock().unwrap().status.ready.clone();
    let directory = session_directory(&rec.dir)?;
    let identity = if matches!(ready, Ready::Flutter { .. } | Ready::FlutterEntering { .. }) {
        directory.join("session/worker-process.json")
    } else {
        directory.join("worker.json")
    };
    match load::<ProcessIdentity>(&identity)? {
        Some(worker) => worker.has_exited(),
        None => Ok(false),
    }
}

pub(super) fn retire_worker_loss(
    rec: &Arc<Recorder>, reason: &str,
) -> io::Result<Option<tokio::task::JoinHandle<io::Result<()>>>> {
    retire_with_evidence(rec, reason, "native-worker-loss.json", "native-worker-retirement.json")
}

pub(super) fn retire(
    rec: &Arc<Recorder>,
    reason: &str,
) -> io::Result<Option<tokio::task::JoinHandle<io::Result<()>>>> {
    retire_with_evidence(rec, reason, "native-storage-pressure.json", "native-storage-retirement.json")
}

fn retire_with_evidence(
    rec: &Arc<Recorder>, reason: &str, trigger: &'static str, result: &'static str,
) -> io::Result<Option<tokio::task::JoinHandle<io::Result<()>>>> {
    let mut state = rec.state.lock().unwrap();
    if state.status.stopped || state.retiring {
        return Ok(None);
    }
    state.status.failure.get_or_insert_with(|| reason.to_owned());
    // A failed publication does not unfence this owner. The normal Stop journal below still
    // determines whether retirement was admitted; actual full-disk durability is not promised.
    let _ = rec.save(Class::Retirement, &rec.dir.join("native-status.json"), &Recorder::snapshot(&state));
    // Never race Stop against a dispatched operation or overwrite its original outcome.
    // The failure above fences new ordinary work while that operation settles.
    if state.status.running.is_some() {
        return Ok(None);
    }
    let id = RequestId::generate();
    let budget_ms = 60_000_u64;
    let hash = args_sha256(&serde_json::to_value((&Op::Stop, budget_ms)).unwrap());
    rec.charge_admission(&id, &hash, &Op::Stop, budget_ms)?;
    rec.save(Class::Retirement, &rec.dir.join(trigger), &RetirementRequest {
        reason, retirement_request: &id,
    })?;
    if !matches!(state.journal.begin(&id, &hash).map_err(io::Error::other)?, Begin::New) {
        return Err(io::Error::other("storage retirement identity already exists"));
    }
    recovery::record(&rec.dir, &id, &Op::Stop, budget_ms)?;
    state.journal.dispatched(&id).map_err(io::Error::other)?;
    state.status.running = Some(id.clone());
    state.retiring = true;
    let lease = state.lease.as_ref().map(|(lease, _)| lease.clone())
        .unwrap_or_else(LeaseToken::generate);
    drop(state);
    let rec = rec.clone();
    Ok(Some(tokio::task::spawn_blocking(move || {
        let answer = rec.execute(id, lease, Op::Stop, Instant::now() + Duration::from_millis(budget_ms));
        rec.save(Class::Retirement, &rec.dir.join(result), &answer)
    })))
}
