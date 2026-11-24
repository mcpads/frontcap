//! Construction of a child generation from a saved basis. The child is launched under
//! the branch request id. A recipe child has the recipe's browser-owned state installed before its
//! first script, and each later phase runs under an id derived from the branch's; a replay child
//! receives the recorded commands under their recorded ids and stays at the end of the prefix; a
//! checkpoint child is restored from the basis's process image at launch and is ready at the image's
//! boundary. Every way a retried branch finds the same child, and the recorder answers every completed
//! step from its journal instead of applying it twice.
use crate::basis::{self, BasisError};
use crate::launcher::{self, Attached, LaunchRequest, Workspace};
use frontcap_core::basis::{Barrier, BasisKind, BranchOrigin, Recipe, validate_changes};
use frontcap_core::event::{ClockOutcome, SampleValue};
use frontcap_core::ids::{LaunchId, RequestId, SavedId, WatchName};
use frontcap_core::intervention::{AppliedState, StateChange};
use frontcap_core::observation::ObservationBoundary;
use frontcap_core::profile::DeterminismProfile;
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use frontcap_recorder_protocol::{Answer, Call, ErrorCode, ErrorReply, LeaseToken, Observation, ObserveOp, Op, Outcome, Reply, RequestResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub struct BranchRequest {
    pub request_id: RequestId,
    pub saved_id: SavedId,
    /// The requested restoration method; it must be the basis's own.
    pub kind: BasisKind,
    /// Typed changes of this branch, applied after the recipe's.
    pub changes: Vec<StateChange>,
    pub progress_ms: Option<u64>,
    pub host_class: String,
    pub watchdog_ms: u64,
    pub repo_root: PathBuf,
}

/// A child that reached `ready`, with what each phase produced.
pub struct Constructed {
    pub attached: Attached,
    /// The caller already held the child's lease: `attached.client` is a plain connection, and the
    /// caller's own lease connection must be kept.
    pub reused: bool,
    pub origin: BranchOrigin,
    pub barrier: Option<Outcome>,
    pub changes: Option<AppliedState>,
    pub branch_changes: Option<AppliedState>,
    pub verification: Vec<(WatchName, SampleValue)>,
    /// A replay child: the recorded commands it repeated, and how many the prefix has.
    pub replayed: Option<(usize, usize)>,
    pub boundary: ObservationBoundary,
}

/// What the phases of one construction produced.
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Phases {
    barrier: Option<Outcome>,
    changes: Option<AppliedState>,
    branch_changes: Option<AppliedState>,
    verification: Vec<(WatchName, SampleValue)>,
    replayed: Option<(usize, usize)>,
    boundary: Option<ObservationBoundary>,
}

/// How a basis is restored once its child is launched.
enum Plan<'a> {
    Recipe(&'a Recipe),
    Replay { prepared: Box<crate::replay::Prepared>, source: PathBuf },
    /// The child is restored from the basis's process image at launch: it is ready at the image's boundary.
    Checkpoint,
}

#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error(transparent)]
    Basis(#[from] BasisError),
    /// The requested method is not the basis's; nothing was created.
    #[error("{0}")]
    MethodMismatch(String),
    #[error("{0}")]
    Invalid(String),
    /// Launching or reaching the child failed; a retry with the same request id resumes it.
    #[error("{}", .0.message)]
    Launch(ErrorReply),
    /// A phase failed: the child is stopped with its evidence and never published as ready.
    #[error("construction of {launch_id} failed at {phase}: {detail}")]
    Construction { launch_id: LaunchId, phase: String, detail: String, bundle: PathBuf },
}

/// The terminal outcome of a construction, kept in the child's runtime directory. A retried branch
/// reports it again instead of running the phases on a child that may since have moved on.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum Construction {
    Ready(Box<Phases>),
    Failed { phase: String, detail: String },
}

const CONSTRUCTION: &str = "construction.json";

enum PhaseError {
    /// The phase ran and did not establish its condition.
    Failed(String, String),
    /// The recorder or its connection failed; the outcome is not known.
    Fault(ErrorReply),
}

/// Constructs a child from a saved basis by its own method, or finds the child an earlier attempt of
/// the same request constructed. `held` returns the lease the caller already holds for a generation.
pub async fn construct(
    ws: &Workspace,
    req: BranchRequest,
    recorder_bin: &Path,
    env: &[(String, String)],
    held: &(dyn Fn(&LaunchId) -> Option<LeaseToken> + Sync),
) -> Result<Constructed, BranchError> {
    let _held = basis::hold(ws, &req.saved_id)?;
    let basis = basis::load(ws, &req.saved_id)?;
    let kind = basis.manifest.kind;
    if req.kind != kind {
        return Err(BranchError::MethodMismatch(format!(
            "{} is a {} basis and cannot be restored as {}", basis.id, kind.as_str(), req.kind.as_str()
        )));
    }
    validate_changes(&req.changes, "changes").map_err(|e| BranchError::Invalid(e.to_string()))?;
    if req.progress_ms.is_some_and(|ms| ms > 10_000) {
        return Err(BranchError::Invalid("progress_ms is at most 10000".into()));
    }
    if kind != BasisKind::Recipe && req.progress_ms.is_some() {
        return Err(BranchError::Invalid("progress_ms applies only to a recipe branch's changes; apply changes to the ready child".into()));
    }
    let corrupt = |reason: String| BranchError::Basis(BasisError::Corrupt { id: basis.id.clone(), reason });
    let origin = BranchOrigin { saved_id: basis.id.clone(), sha256: basis.sha256.clone(), kind };
    let (launch, plan) = match (&basis.manifest.recipe, &basis.manifest.replay, basis.checkpoint_image()) {
        (Some(recipe), None, None) => {
            let mut rules = recipe.rules.clone();
            for r in rules.rules.iter_mut() {
                if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut r.action {
                    *cassette = basis.file(cassette).display().to_string();
                }
            }
            let launch = LaunchRequest {
                request_id: req.request_id.clone(),
                url: recipe.url.clone(),
                profile: DeterminismProfile { host_class: req.host_class.clone(), ..recipe.profile.clone() },
                rules,
                passthrough_origins: recipe.passthrough_origins.clone(),
                watchdog_ms: req.watchdog_ms,
                repo_root: req.repo_root.clone(),
                origin: Some(origin.clone()),
                restore: None,
            };
            (launch, Plan::Recipe(recipe))
        }
        (None, Some(replay), None) => {
            if !req.changes.is_empty() {
                return Err(BranchError::Invalid("a replay branch repeats its recording only; apply changes to the ready child".into()));
            }
            let source = basis.replay_source(ws).expect("a replay basis has a source");
            let manifest = std::fs::read(source.join("manifest.json")).map_err(|e| corrupt(format!("its source run {} is unavailable: {e}", replay.run)))?;
            if frontcap_core::canonical::sha256_bytes_hex(&manifest) != replay.manifest_sha256 {
                return Err(corrupt(format!("its source run {} changed", replay.run)));
            }
            let options = crate::replay::ReplayOptions {
                url: None, watchdog_ms: req.watchdog_ms, repo_root: req.repo_root.clone(),
                recorder_bin: recorder_bin.to_path_buf(), env: env.to_vec(),
            };
            let prepared = crate::replay::prepare(ws, &source, &options).await.map_err(|e| corrupt(format!("its source run {}: {e}", replay.run)))?;
            if prepared.commands.len() != replay.commands {
                return Err(corrupt(format!("its source run {} has {} commands, not {}", replay.run, prepared.commands.len(), replay.commands)));
            }
            let launch = LaunchRequest {
                request_id: req.request_id.clone(),
                url: prepared.launch.url.clone(),
                profile: prepared.launch.profile.clone(),
                rules: prepared.launch.rules.clone(),
                passthrough_origins: prepared.launch.passthrough_origins.clone(),
                watchdog_ms: req.watchdog_ms,
                repo_root: req.repo_root.clone(),
                origin: Some(origin.clone()),
                restore: None,
            };
            (launch, Plan::Replay { prepared: Box::new(prepared), source })
        }
        (None, None, Some(image)) => {
            if !req.changes.is_empty() {
                return Err(BranchError::Invalid("a checkpoint branch restores its image only; apply changes to the ready child".into()));
            }
            // The source's launch inputs: the child continues its execution, and its own evidence names them.
            let dir = crate::image::image_dir(ws, image).map_err(corrupt)?;
            let source: crate::runtime::LaunchSpec = std::fs::read(dir.join("spec.json")).ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .ok_or_else(|| corrupt("its image has no launch spec".into()))?;
            let launch = LaunchRequest {
                request_id: req.request_id.clone(),
                url: source.url,
                profile: DeterminismProfile { host_class: req.host_class.clone(), ..source.profile },
                rules: source.rules,
                passthrough_origins: source.passthrough_origins,
                watchdog_ms: req.watchdog_ms,
                repo_root: req.repo_root.clone(),
                origin: Some(origin.clone()),
                restore: Some(image.to_string()),
            };
            (launch, Plan::Checkpoint)
        }
        _ => return Err(corrupt("its content does not match its kind".into())),
    };
    // The whole branch (basis, method and changes) owns the request id; the launch owns its own entry.
    let mut branch_identity = serde_json::json!({
        "sha256": basis.sha256, "kind": kind, "changes": req.changes,
    });
    if let Some(ms) = req.progress_ms { branch_identity["progress_ms"] = ms.into(); }
    let branch_args = frontcap_core::journal::args_sha256(&branch_identity);
    launcher::register(ws, &req.request_id.derived("branch"), &branch_args).map_err(|e| BranchError::Launch(e.0))?;
    let launch_args = frontcap_core::journal::args_sha256(&serde_json::to_value(&launch).expect("launch requests serialize"));
    let earlier = launcher::registered(ws, &req.request_id, &launch_args);
    let concluded = earlier.as_ref().and_then(|id| frontcap_core::state::load::<Construction>(&ws.runtime_dir(id).join(CONSTRUCTION)).ok().flatten());
    if let (Some(id), Some(Construction::Failed { phase, detail })) = (&earlier, &concluded) {
        return Err(BranchError::Construction { bundle: ws.runs().join(launcher::run_id_of(id).as_str()), launch_id: id.clone(), phase: phase.clone(), detail: detail.clone() });
    }
    let reused = earlier.as_ref().and_then(|id| held(id).map(|lease| (id.clone(), lease)));
    let orphaned = earlier.as_ref().filter(|id| reused.is_none() && !ended(ws, id));
    let mut attached = match (reused.clone(), orphaned) {
        // The caller's lease connection stays its own; the phases use another connection with its token.
        (Some((id, lease)), _) => connect(ws, &id, lease).await.map_err(BranchError::Launch)?,
        (None, Some(id)) => take_back(ws, id, &req.request_id, recorder_bin, env).await?,
        (None, None) => launcher::launch(ws, launch, recorder_bin, env).await.map_err(|e| BranchError::Launch(e.0))?,
    };
    let result = match (concluded, plan) {
        // A child that already became ready may have moved on since: report what it was when ready.
        (Some(Construction::Ready(p)), _) => Ok(*p),
        (_, Plan::Recipe(recipe)) => phases(&mut attached, recipe, &req).await,
        (_, Plan::Replay { mut prepared, source }) => match replay_phases(ws, &mut attached, &mut prepared, &source).await {
            Ok(p) => verify(&mut attached, &req, &basis.manifest.verify).await.map(|verification| Phases { verification, ..p }),
            Err(e) => Err(e),
        },
        (_, Plan::Checkpoint) => match verify(&mut attached, &req, &basis.manifest.verify).await {
            Ok(verification) => current_boundary(&mut attached).await.map(|b| Phases { verification, boundary: Some(b), ..Phases::default() }),
            Err(e) => Err(e),
        },
    };
    let dir = ws.runtime_dir(&attached.launch_id);
    let keep = |c: &Construction| frontcap_core::state::save_atomic(&dir.join(CONSTRUCTION), c)
        .map_err(|e| BranchError::Launch(ErrorReply::new(ErrorCode::Internal, e.to_string())));
    match result {
        Ok(p) => {
            keep(&Construction::Ready(Box::new(p.clone())))?;
            Ok(Constructed {
                attached, reused: reused.is_some(), origin, barrier: p.barrier, changes: p.changes, branch_changes: p.branch_changes,
                verification: p.verification, replayed: p.replayed, boundary: p.boundary.expect("a ready child has a boundary"),
            })
        }
        Err(PhaseError::Fault(e)) => Err(BranchError::Launch(e)),
        Err(PhaseError::Failed(phase, detail)) => {
            let id = attached.launch_id.clone();
            keep(&Construction::Failed { phase: phase.clone(), detail: detail.clone() })?;
            // Stopping finalizes the child's evidence; it is never published as ready.
            let stop = attached.client.call(Call::Mutate { lease: Some(attached.lease.clone()), request_id: req.request_id.derived("stop"), op: Op::Stop }).await;
            if let Err(e) = stop {
                return Err(BranchError::Launch(ErrorReply::new(ErrorCode::RecorderFault, format!("construction of {id} failed at {phase} ({detail}), and stopping it failed: {e}"))));
            }
            drop(attached);
            wait_stopped(&dir).await;
            Err(BranchError::Construction { bundle: ws.runs().join(launcher::run_id_of(&id).as_str()), launch_id: id, phase, detail })
        }
    }
}

/// Repeats the recorded commands, each at its recorded instant with its recorded outcome, then checks
/// that the child's behavior so far is the recording's beginning. The child stays at the end of the
/// prefix.
async fn replay_phases(ws: &Workspace, a: &mut Attached, p: &mut crate::replay::Prepared, source: &Path) -> Result<Phases, PhaseError> {
    let (replayed, diverged) = crate::replay::execute(p, a, true).await;
    if let Some(d) = diverged {
        let why = d.message.map(|m| format!(" ({m})")).unwrap_or_default();
        return Err(PhaseError::Failed("replay".into(), format!("{} answered {} instead of {}{why}", d.request_id, d.actual, d.expected)));
    }
    let recorded = crate::replay::recorded_trace(p, source).await
        .map_err(|e| PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, e.to_string())))?;
    let trace = std::fs::read_to_string(ws.runs().join(launcher::run_id_of(&a.launch_id).as_str()).join("trace.jsonl"))
        .map_err(|e| PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, e.to_string())))?;
    if let Err(d) = frontcap_core::projection::compare_replay_prefix(&recorded, &trace, &[], &p.recorded) {
        let bounded = |line: Option<String>| line.map_or_else(|| "nothing".to_string(), |l| l.chars().take(300).collect());
        return Err(PhaseError::Failed("replay".into(), format!(
            "the behavior log differs from the recording at line {}: recorded {}, replayed {}", d.index, bounded(d.left), bounded(d.right),
        )));
    }
    // The prefix is over: the child names its nodes itself from here on, and the recording's references
    // (the parent's) are refused like any other generation's.
    match a.client.call(Call::RetireBoundRefs { lease: a.lease.clone() }).await.map_err(fault)? {
        Reply::Bound => {}
        other => return Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected reply {other:?}")))),
    }
    let boundary = current_boundary(a).await?;
    Ok(Phases { replayed: Some((replayed, p.commands.len())), boundary: Some(boundary), ..Phases::default() })
}

/// A connection to a generation whose lease the caller already holds.
async fn connect(ws: &Workspace, id: &LaunchId, lease: LeaseToken) -> Result<Attached, ErrorReply> {
    let record = crate::runtime::load_runtime(&ws.runtime_dir(id))
        .map_err(|e| ErrorReply::new(ErrorCode::Internal, e.to_string()))?
        .ok_or_else(|| ErrorReply::new(ErrorCode::NotFound, format!("no runtime {id}")))?;
    let lost = |e: ClientError| match e {
        ClientError::Recorder(reply) => reply,
        other => ErrorReply::new(ErrorCode::RecorderFault, other.to_string()),
    };
    let (mut client, _) = RecorderClient::connect(&record.socket).await.map_err(lost)?;
    let status = match client.call(Call::Status).await.map_err(lost)? {
        Reply::Status { status } => *status,
        other => return Err(ErrorReply::new(ErrorCode::Internal, format!("unexpected reply {other:?}"))),
    };
    Ok(Attached { launch_id: id.clone(), client, lease, status, completed_while_detached: Vec::new() })
}

/// The recorder records `stopped` once its last connection is gone; the failed child is reported only
/// when its runtime says so.
async fn wait_stopped(dir: &Path) {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(10) {
        if crate::runtime::load_runtime(dir).ok().flatten().is_some_and(|r| r.state == crate::runtime::RuntimeState::Stopped || !r.recorder.is_alive()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Whether the child's run is over (stopped or failed): such a child is not taken back.
fn ended(ws: &Workspace, id: &LaunchId) -> bool {
    !matches!(crate::runtime::load_runtime(&ws.runtime_dir(id)), Ok(Some(r)) if r.state == crate::runtime::RuntimeState::Ready)
}

/// The child of an earlier branch under this request id that the caller does not hold (the caller
/// restarted, or the child's recorder died): its recorder is relaunched if it died, then its lease is
/// taken. A child another controller holds stays its (`lease_held`).
async fn take_back(ws: &Workspace, id: &LaunchId, request_id: &RequestId, recorder_bin: &Path, env: &[(String, String)]) -> Result<Attached, BranchError> {
    let dead = crate::runtime::load_runtime(&ws.runtime_dir(id)).ok().flatten().is_some_and(|r| !r.recorder.is_alive());
    if dead {
        launcher::recover(ws, id, recorder_bin, env).await.map_err(|e| BranchError::Launch(e.0))?;
    }
    launcher::reattach(ws, id, request_id.derived("reattach")).await.map_err(|e| BranchError::Launch(e.0))
}

async fn phases(a: &mut Attached, recipe: &Recipe, req: &BranchRequest) -> Result<Phases, PhaseError> {
    let id = |role: &str| req.request_id.derived(role);
    // At the start boundary, before any script the barrier lets run.
    if !recipe.dialog_rules.0.is_empty() {
        mutate(a, id("dialog-rules"), Op::DialogRules { rules: recipe.dialog_rules.clone() }, "dialog rules").await?;
    }
    let barrier = match &recipe.barrier {
        Barrier::None => None,
        Barrier::Settle { idle_window_ms, max_ms } => {
            let op = Op::Settle { idle_us: idle_window_ms * 1000, max_us: max_ms * 1000 };
            Some(mutate(a, id("barrier"), op, "barrier").await?)
        }
        Barrier::Until { predicate, max_ms } => {
            let outcome = mutate(a, id("barrier"), Op::UntilPredicate { predicate: predicate.clone(), max_us: max_ms * 1000, context: None }, "barrier").await?;
            if !matches!(outcome, Outcome::Clock { result: ClockOutcome::ConditionMet { .. }, .. }) {
                return Err(PhaseError::Failed("barrier".into(), format!("the predicate did not hold within {max_ms} ms")));
            }
            Some(outcome)
        }
    };
    let changes = apply(a, id("changes"), &recipe.changes, recipe.progress_ms, "changes").await?;
    let branch_changes = apply(a, id("branch-changes"), &req.changes, req.progress_ms, "branch changes").await?;
    let verification = verify(a, req, &recipe.verify).await?;
    let boundary = current_boundary(a).await?;
    Ok(Phases { barrier, changes, branch_changes, verification, replayed: None, boundary: Some(boundary) })
}

/// Each named predicate's first sample in the ready child, kept there as a watch; one that is not true
/// fails the branch.
async fn verify(a: &mut Attached, req: &BranchRequest, verify: &[frontcap_core::basis::Verification]) -> Result<Vec<(WatchName, SampleValue)>, PhaseError> {
    let mut verification = Vec::new();
    for v in verify {
        let op = Op::WatchAdd { name: v.name.clone(), predicate: v.predicate.clone(), context: None };
        match mutate(a, req.request_id.derived(&format!("verify-{}", v.name)), op, "verify").await? {
            Outcome::Sample { value } => verification.push((v.name.clone(), value)),
            other => return Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected verification outcome {other:?}")))),
        }
    }
    let failed: Vec<String> = verification.iter().filter(|(_, v)| *v != SampleValue::True)
        .map(|(name, v)| format!("{name} is {}", serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()))
        .collect();
    if !failed.is_empty() {
        return Err(PhaseError::Failed("verify".into(), failed.join(", ")));
    }
    Ok(verification)
}

/// Applies one batch of typed changes at the current boundary. A batch an earlier attempt completed is
/// answered from the journal: its recorded boundary, not today's, belongs to its arguments.
async fn apply(a: &mut Attached, request_id: RequestId, changes: &[StateChange], progress_ms: Option<u64>, phase: &str) -> Result<Option<AppliedState>, PhaseError> {
    if changes.is_empty() {
        return Ok(None);
    }
    let outcome = match completed(a, &request_id, phase).await? {
        Some(outcome) => outcome,
        None => {
            let boundary = current_boundary(a).await?;
            mutate(a, request_id, Op::StateApply { boundary, changes: changes.to_vec(), context: None, progress_us: progress_ms.map(|ms| ms * 1000) }, phase).await?
        }
    };
    match outcome {
        Outcome::StateApplied(applied) if applied.completed => Ok(Some(applied)),
        Outcome::StateApplied(applied) => {
            let detail = applied.receipts.iter().find(|r| r.error.is_some() || !r.verified)
                .map(|r| format!("item {}: {}", r.index, r.error.clone().unwrap_or_else(|| "readback did not verify".into())))
                .unwrap_or_else(|| "the batch did not complete".into());
            Err(PhaseError::Failed(phase.into(), detail))
        }
        other => Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected {phase} outcome {other:?}")))),
    }
}

fn fault(e: ClientError) -> PhaseError {
    match e {
        ClientError::Recorder(reply) => PhaseError::Fault(reply),
        other => PhaseError::Fault(ErrorReply::new(ErrorCode::RecorderFault, other.to_string())),
    }
}

/// The saved outcome of a request an earlier attempt completed.
async fn completed(a: &mut Attached, request_id: &RequestId, phase: &str) -> Result<Option<Outcome>, PhaseError> {
    match a.client.call(Call::Result { request_id: request_id.clone() }).await.map_err(fault)? {
        Reply::Result { result: RequestResult::Completed { answer }, .. } => match *answer {
            Answer::Ok(Reply::Done { outcome, .. }) => Ok(Some(outcome)),
            Answer::Ok(other) => Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected saved answer {other:?}")))),
            Answer::Err(e) => Err(PhaseError::Failed(phase.into(), e.message)),
        },
        _ => Ok(None),
    }
}

async fn current_boundary(a: &mut Attached) -> Result<ObservationBoundary, PhaseError> {
    match a.client.call(Call::Observe { op: ObserveOp::InputState, context: None }).await.map_err(fault)? {
        Reply::Observed { result: Observation::Input(state) } => Ok(state.boundary),
        other => Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected reply {other:?}")))),
    }
}

/// Runs one journaled phase. A recorder refusal of the phase's own arguments (an ambiguous or missing
/// target, an invalid predicate) fails construction; a lost recorder is a fault to resume.
async fn mutate(a: &mut Attached, request_id: RequestId, op: Op, phase: &str) -> Result<Outcome, PhaseError> {
    let reply = a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id, op }).await;
    match reply {
        Ok(Reply::Done { outcome: Outcome::NotApplied, .. }) => {
            Err(PhaseError::Fault(ErrorReply::new(ErrorCode::RecorderFault, format!("the {phase} phase was interrupted before it reached the page"))))
        }
        Ok(Reply::Done { outcome, .. }) => Ok(outcome),
        Ok(Reply::Suspended { operation }) => Err(PhaseError::Failed(phase.into(), match (&operation.dialog, &operation.request) {
            (Some(dialog), _) => format!("a dialog suspended it: {}", dialog.message),
            (_, Some(request)) => format!("the page waits for a held request: {} {}", request.method, request.url),
            _ => "it was suspended".into(),
        })),
        Ok(other) => Err(PhaseError::Fault(ErrorReply::new(ErrorCode::Internal, format!("unexpected reply {other:?}")))),
        Err(ClientError::Recorder(e)) if !matches!(e.code, ErrorCode::RecorderFault | ErrorCode::Busy | ErrorCode::Internal | ErrorCode::LeaseRequired | ErrorCode::LeaseHeld) => {
            Err(PhaseError::Failed(phase.into(), e.message))
        }
        Err(e) => Err(fault(e)),
    }
}
