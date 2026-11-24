//! Re-execution of a recorded run: a new
//! generation from the run's launch inputs receives every recorded command with its request id, each
//! recorded reference bound from its resolution record, and the two behavior logs are compared under the
//! `replay` projection.
mod dialogs;
mod storage;

use crate::launcher::{self, Attached, LaunchRequest, Workspace};
use crate::server::outcome_tag;
use frontcap_core::bundle::{Bundle, Finalization, Manifest};
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::inputs::{self, CommandLine};
use frontcap_core::projection::{Difference, compare_replay};
use frontcap_recorder_protocol::client::ClientError;
use frontcap_recorder_protocol::{Answer, Call, ErrorCode, ErrorReply, Op, Reply};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub struct ReplayOptions {
    /// Launch at this URL instead of the recorded one; the recorded origin is read as this one's
    /// everywhere (rules, commands, and the comparison).
    pub url: Option<String>,
    pub watchdog_ms: u64,
    pub repo_root: PathBuf,
    pub recorder_bin: PathBuf,
    pub env: Vec<(String, String)>,
}

/// The first recorded command whose replay answered differently.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Divergence {
    pub request_id: RequestId,
    pub expected: String,
    pub actual: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplayReport {
    pub source: RunId,
    pub run: RunId,
    pub launch_id: LaunchId,
    /// Commands sent before the replay ended (all of them unless it diverged).
    pub replayed: usize,
    pub commands: usize,
    pub diverged: Option<Divergence>,
    /// Whether the behavior is the same: every command answered as recorded and the traces compare equal.
    pub equal: bool,
    /// The first line where the traces differ under the `replay` projection.
    pub difference: Option<DifferenceReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DifferenceReport {
    pub index: usize,
    pub recorded: Option<String>,
    pub replayed: Option<String>,
}

impl From<Difference> for DifferenceReport {
    fn from(d: Difference) -> DifferenceReport {
        DifferenceReport { index: d.index, recorded: d.left, replayed: d.right }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("{0} is not a bundle")]
    NotFound(String),
    #[error("{0} is not a finalized bundle; stop its generation first")]
    NotFinalized(String),
    #[error("the bundle is not intact: {0}")]
    Corrupt(String),
    #[error("{run} is not replayable: missing {missing:?}")]
    NotReplayable { run: String, missing: Vec<String> },
    #[error(transparent)]
    Inputs(#[from] inputs::InputsError),
    #[error("{}", .0.message)]
    Launch(ErrorReply),
    #[error("{0}")]
    Io(String),
}

fn origin_of(url: &str) -> Option<String> {
    frontcap_chromium::encode::origin_of(url)
}

use frontcap_core::projection::rewrite;

fn answer_of(r: Result<Reply, ClientError>) -> Answer {
    match r {
        Ok(reply) => Answer::Ok(reply),
        Err(ClientError::Recorder(e)) => Answer::Err(e),
        Err(other) => Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, other.to_string())),
    }
}

fn message_of(a: &Answer) -> Option<String> {
    match a {
        Answer::Err(e) => Some(e.message.clone()),
        Answer::Ok(_) => None,
    }
}

/// The origin rewrites among `rewrites` (those that are not hashes).
fn with_rule_hashes_origins(rewrites: &[(String, String)]) -> Vec<(String, String)> {
    rewrites.iter().filter(|(from, _)| from.contains("://")).cloned().collect()
}

/// `rewrites`, and for each installed rule set the hash it has once rewritten: a rule set is recorded
/// by its hash, so reading the recorded origin as another one changes it.
fn with_rule_hashes(source: &Path, rewrites: Vec<(String, String)>) -> Result<Vec<(String, String)>, ReplayError> {
    if rewrites.is_empty() {
        return Ok(rewrites);
    }
    let dir = source.join("rules");
    let mut out = rewrites.clone();
    let entries = std::fs::read_dir(&dir).map_err(|e| ReplayError::Io(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let path = entry.map_err(|e| ReplayError::Io(e.to_string()))?.path();
        let text = std::fs::read_to_string(&path).map_err(|e| ReplayError::Io(format!("{}: {e}", path.display())))?;
        let (from, to) = (frontcap_core::canonical::sha256_hex(&text), frontcap_core::canonical::sha256_hex(&rewrite(&text, &rewrites)));
        if from != to {
            out.push((from, to));
        }
    }
    Ok(out)
}

/// The re-execution inputs of a finalized bundle, checked and prepared for a new generation.
pub(crate) struct Prepared {
    pub(crate) manifest: Manifest,
    pub(crate) launch: inputs::LaunchInputs,
    pub(crate) commands: Vec<CommandLine>,
    ops: Vec<Op>,
    rule_rewrites: Vec<(String, String)>,
    pub(crate) recorded: Vec<(u64, String)>,
    installation_hashes: Vec<(u64, String, String)>,
    storage_relocation: Option<storage::StorageRelocation>,
    source_events: Vec<frontcap_core::event::BehaviorEvent>,
    watchdog_ms: u64,
    /// Rewritten cassette copies live as long as the preparation.
    _removed: Removed,
}

/// A directory removed when dropped.
pub(super) struct Removed(pub(super) PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Checks the finalized bundle at `source` and prepares its launch inputs and commands, read at the
/// URL `o.url` names when given.
pub(crate) async fn prepare(ws: &Workspace, source: &Path, o: &ReplayOptions) -> Result<Prepared, ReplayError> {
    let read = |p: PathBuf| std::fs::read_to_string(&p).map_err(|e| ReplayError::Io(format!("{}: {e}", p.display())));
    let manifest_path = source.join("manifest.json");
    if !source.is_dir() || !manifest_path.exists() {
        return Err(if source.join("trace.jsonl").exists() { ReplayError::NotFinalized(source.display().to_string()) } else { ReplayError::NotFound(source.display().to_string()) });
    }
    let manifest: Manifest = serde_json::from_str(&read(manifest_path)?).map_err(|e| ReplayError::Corrupt(format!("manifest.json: {e}")))?;
    if manifest.finalization != Finalization::Finalized {
        return Err(ReplayError::NotFinalized(source.display().to_string()));
    }
    if !manifest.replayable {
        return Err(ReplayError::NotReplayable { run: manifest.run.to_string(), missing: manifest.missing_inputs });
    }
    check_integrity(source, &manifest)?;
    let recorded = inputs::read_launch(source)?;
    // The origin moves, and its WebSocket form with it (`ws://` for `http://`, `wss://` for `https://`).
    let rewrites: Vec<(String, String)> = match &o.url {
        Some(url) => match (origin_of(&recorded.url), origin_of(url)) {
            (Some(from), Some(to)) if from != to => {
                let ws = |o: &str| o.strip_prefix("https://").map(|r| format!("wss://{r}")).or_else(|| o.strip_prefix("http://").map(|r| format!("ws://{r}")));
                let mut r = vec![(from.clone(), to.clone())];
                if let (Some(f), Some(t)) = (ws(&from), ws(&to)) {
                    r.push((f, t));
                }
                r
            }
            _ => vec![],
        },
        None => vec![],
    };
    let mut launch: inputs::LaunchInputs =
        serde_json::from_str(&rewrite(&serde_json::to_string(&recorded).expect("launch inputs serialize"), &rewrites)).map_err(|e| ReplayError::Corrupt(e.to_string()))?;
    if let Some(url) = &o.url {
        launch.url = url.clone();
    }
    let storage_relocation = match (origin_of(&recorded.url), origin_of(&launch.url)) {
        (Some(from), Some(to)) => storage::StorageRelocation::prepare(
            ws, &o.repo_root, &recorded.profile, &mut launch.profile, &from, &to,
        ).await?,
        _ => None,
    };
    // A replayed cassette is the recorded bundle's copy; what the recorded run recorded is answered from
    // its recording, not from the backend again. At another origin a cassette is matched and answered as
    // its requests are made now: a copy outside the finalized bundle with the origin rewritten in its
    // request keys and response headers (`Location`, CORS); response bodies are kept as recorded.
    let moved_dir = ws.root.join(format!("replay-cassettes-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
    // The copies serve this replay only.
    let removed = Removed(moved_dir.clone());
    let moved = |path: PathBuf| -> Result<PathBuf, ReplayError> {
        if rewrites.is_empty() || !path.exists() {
            return Ok(path);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ReplayError::Io(format!("{}: {e}", path.display())))?;
        let copy = moved_dir.join(path.file_name().expect("a cassette file"));
        std::fs::create_dir_all(&moved_dir).and_then(|_| std::fs::write(&copy, rewrite(&text, &rewrites))).map_err(|e| ReplayError::Io(e.to_string()))?;
        Ok(copy)
    };
    let mut recorded = Vec::new();
    let mut installation_hashes = Vec::new();
    let origins = launch.passthrough_origins.clone().unwrap_or_else(|| origin_of(&launch.url).into_iter().collect());
    let mut prepare_rules = |rules: &mut frontcap_core::rules::RuleSet, revision: u64| -> Result<(), ReplayError> {
        for r in &mut rules.rules {
            match &mut r.action {
                frontcap_core::rules::Action::Replay { cassette, .. } if Path::new(cassette.as_str()).is_relative() => {
                    *cassette = moved(source.join(&*cassette))?.display().to_string();
                }
                frontcap_core::rules::Action::Record { cassette } => {
                    let name = frontcap_core::cassette::recording_name(revision, cassette);
                    let mut kept = source.join(format!("cassettes/{name}.jsonl"));
                    if !kept.exists() {
                        kept = moved_dir.join(format!("empty-cassette-{revision}-{}.jsonl", r.id));
                        std::fs::create_dir_all(&moved_dir).and_then(|_| std::fs::write(&kept, "")).map_err(|e| ReplayError::Io(e.to_string()))?;
                    }
                    recorded.push((revision, r.id.to_string()));
                    r.action = frontcap_core::rules::Action::Replay {
                        cassette: moved(kept)?.display().to_string(), latency_ms: None,
                        after_last: frontcap_core::rules::AfterLast::Fail,
                    };
                }
                _ => {}
            }
        }
        // Derive the expected installed hash from the transformed inputs, never from the replay's
        // output. Otherwise an incorrect installation could be normalized into apparent equality.
        let mut normalized = rules.clone();
        for r in &mut normalized.rules {
            if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut r.action {
                let text = read(PathBuf::from(&*cassette))?;
                *cassette = format!("cassettes/{}.jsonl", frontcap_core::canonical::sha256_hex(&text));
            }
        }
        let expected = frontcap_core::rules::RuleSet::effective(normalized, &origins)
            .map_err(|e| ReplayError::Corrupt(e.to_string()))?.sha256();
        let original = read(source.join(format!("rules/{revision:04}.json")))?;
        let from = frontcap_core::canonical::sha256_hex(&rewrite(&original, &rewrites));
        if from != expected { installation_hashes.push((revision, from, expected)); }
        Ok(())
    };
    prepare_rules(&mut launch.rules, 0)?;
    // The replay runs on this host, whatever host recorded the run.
    launch.profile.host_class = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let commands: Vec<CommandLine> = inputs::read_commands(source)?;
    dialogs::validate(&commands)?;
    let ops = commands
        .iter()
        .map(|cmd| {
            let mut op = serde_json::from_str::<Op>(&rewrite(&cmd.op.to_string(), &rewrites)).map_err(|e| ReplayError::Corrupt(format!("{}: {e}", cmd.request_id)))?;
            // Uploaded files are read from the recorded bundle's copies.
            if let Op::Act { action: frontcap_core::action::Action::Upload { files, .. }, .. } = &mut op {
                for f in files.iter_mut() {
                    *f = source.join(&*f).display().to_string();
                }
            }
            // Successful installations captured bundle-relative copies, just like launch rules.
            // Refused commands retain their validation shape and must reproduce their refusal.
            if let Op::SetRules { rules, expected_revision, .. } = &mut op
                && !cmd.outcome.starts_with("error:")
            {
                let revision = expected_revision.checked_add(1).ok_or_else(|| ReplayError::Corrupt("rule revision overflow".into()))?;
                prepare_rules(rules, revision)?;
            } else if let Op::SetRules { rules, .. } = &mut op {
                // A refused attempt may have completed cassette preparation before its boundary
                // changed. Resolve its retained assets, including intentionally unreadable paths.
                for rule in &mut rules.rules {
                    if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut rule.action
                        && Path::new(cassette.as_str()).is_relative()
                    { *cassette = moved(source.join(&*cassette))?.display().to_string(); }
                }
            }
            if let Op::EditRules { edit: frontcap_core::rules::RuleEdit::Add { rule, .. }, .. } = &mut op
                && let frontcap_core::rules::Action::Replay { cassette, .. } = &mut rule.action
                && Path::new(cassette.as_str()).is_relative() {
                *cassette = moved(source.join(&*cassette))?.display().to_string();
            }
            Ok(op)
        })
        .collect::<Result<Vec<Op>, ReplayError>>()?;
    let rule_rewrites = with_rule_hashes(source, rewrites)?;
    let source_events = frontcap_core::log::BehaviorLog::read(&source.join("trace.jsonl")).map_err(|e| ReplayError::Corrupt(e.to_string()))?;
    Ok(Prepared {
        manifest, launch, commands, ops, rule_rewrites, recorded, installation_hashes, storage_relocation, source_events,
        watchdog_ms: o.watchdog_ms, _removed: removed,
    })
}

/// Sends the prepared commands to `attached` in order, each starting at its recorded virtual time and
/// answering its recorded outcome; the first that does not is the divergence. With `resume`, a command
/// the generation already completed is checked from its saved answer instead of being sent again.
pub(crate) async fn execute(p: &Prepared, attached: &mut Attached, resume: bool) -> (usize, Option<Divergence>) {
    let lease = attached.lease.clone();
    let mut replayed = 0;
    let mut diverged = None;
    for (cmd, mut op) in p.commands.iter().zip(p.ops.iter().cloned()) {
        let diverge = |actual: String, message: Option<String>| Divergence { request_id: cmd.request_id.clone(), expected: cmd.outcome.clone(), actual, message };
        // A resumed generation already completed this command: its saved answer is the one to check.
        if resume
            && let Ok(Reply::Result { result: frontcap_recorder_protocol::RequestResult::Completed { answer }, .. }) =
                attached.client.call(Call::Result { request_id: cmd.request_id.clone() }).await
        {
            replayed += 1;
            let actual = outcome_tag(&answer);
            if actual != cmd.outcome {
                diverged = Some(diverge(actual, message_of(&answer)));
                break;
            }
            continue;
        }
        // Each command starts where it started in the recorded run, or the replay has already diverged.
        match answer_of(attached.client.call(Call::Status).await) {
            Answer::Ok(Reply::Status { status }) if status.t_us == cmd.t_us => {}
            Answer::Ok(Reply::Status { status }) => {
                let d = Divergence { request_id: cmd.request_id.clone(), expected: format!("t_us:{}", cmd.t_us.0), actual: format!("t_us:{}", status.t_us.0), message: Some("the command starts at another virtual time".into()) };
                diverged = Some(d);
                break;
            }
            other => {
                diverged = Some(diverge(outcome_tag(&other), message_of(&other)));
                break;
            }
        }
        if !cmd.refs.is_empty() {
            let bound = answer_of(attached.client.call(Call::BindRefs { lease: lease.clone(), refs: cmd.refs.clone() }).await);
            if let Answer::Err(_) = &bound {
                diverged = Some(diverge(outcome_tag(&bound), message_of(&bound)));
                break;
            }
        }
        // State revisions include DOM mutation counters, so retain the original revision and
        // subtract only recording taints excluded before this command's durable effect.
        let removed_taints = removed_recording_taints(&p.source_events, &p.recorded, &cmd.request_id);
        // The context the command addressed: the one it named, or the one its recorded action ran in
        // (input continuing a pointer, keys or a composition held in a popup names none).
        let addressed = match &op { Op::Act { context, .. } | Op::StateApply { context, .. } => context.clone(), _ => None }
            .or_else(|| p.source_events.iter().find_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Action(a) if a.request_id == cmd.request_id => a.context.clone(),
                _ => None,
            }));
        // Rebind execution identity and the explained recording delta only. Time, document and
        // other state changes remain guarded; a different page state still rejects.
        if let Op::Act { action: frontcap_core::action::Action::ClickAt { boundary, .. } | frontcap_core::action::Action::HoverAt { boundary, .. } | frontcap_core::action::Action::PointerBegin { boundary, .. } | frontcap_core::action::Action::PointerMove { boundary, .. } | frontcap_core::action::Action::PointerPress { boundary, .. } | frontcap_core::action::Action::TouchAdd { boundary, .. } | frontcap_core::action::Action::TouchMove { boundary, .. } | frontcap_core::action::Action::Wheel { boundary, .. }, .. } | Op::StateApply { boundary, .. } | Op::SetRules { boundary, .. } | Op::EditRules { boundary, .. } = &mut op
            && cmd.outcome != "error:observation_changed"
            && launcher::run_id_of(&boundary.launch_id) == p.manifest.run
        {
            let observed = answer_of(attached.client.call(Call::Observe {
                op: frontcap_recorder_protocol::ObserveOp::Scene { scope: None, depth: Some(0), max_nodes: Some(1), image: false, unfold: false },
                context: addressed,
            }).await);
            match observed {
                Answer::Ok(Reply::Observed { result: frontcap_recorder_protocol::Observation::Scene(scene) }) => {
                    boundary.launch_id = scene.boundary.launch_id;
                    boundary.context_id = scene.boundary.context_id;
                    boundary.state_revision = boundary.state_revision.saturating_sub(removed_taints);
                }
                other => {
                    diverged = Some(diverge(outcome_tag(&other), message_of(&other)));
                    break;
                }
            }
        }
        if let Op::Act { action: frontcap_core::action::Action::PointerMove { handle, .. } | frontcap_core::action::Action::PointerPress { handle, .. } | frontcap_core::action::Action::PointerRelease { handle, .. } | frontcap_core::action::Action::TouchAdd { handle, .. } | frontcap_core::action::Action::TouchMove { handle, .. } | frontcap_core::action::Action::TouchEnd { handle, .. } | frontcap_core::action::Action::PointerEnd { handle } | frontcap_core::action::Action::PointerCancel { handle }, .. } = &mut op
            && launcher::run_id_of(&handle.launch_id) == p.manifest.run
        {
            handle.launch_id = attached.launch_id.clone();
        }
        // A request the controller cancelled has its waits cancelled at the instant they stopped.
        if let Some(at) = cmd.cancelled_at_us {
            let armed = answer_of(attached.client.call(Call::Cancel { lease: Some(lease.clone()), request_id: cmd.request_id.clone(), at_us: Some(at) }).await);
            if let Answer::Err(_) = &armed {
                diverged = Some(diverge(outcome_tag(&armed), message_of(&armed)));
                break;
            }
        }
        let answer = answer_of(attached.client.call(Call::Mutate { lease: Some(lease.clone()), request_id: cmd.request_id.clone(), op }).await);
        replayed += 1;
        let answer = match dialogs::complete(&mut attached.client, &lease, cmd, &p.rule_rewrites, answer, p.watchdog_ms).await {
            Ok(answer) => answer,
            Err((actual, message)) => {
                diverged = Some(diverge(actual, Some(message)));
                break;
            }
        };
        let actual = outcome_tag(&answer);
        if actual != cmd.outcome {
            diverged = Some(diverge(actual, message_of(&answer)));
            break;
        }
    }
    (replayed, diverged)
}

/// The recorded behavior log as the replay is expected to reproduce it: origins and rule hashes
/// rewritten, relocated storage identities mapped.
pub(crate) async fn recorded_trace(p: &mut Prepared, source: &Path) -> Result<String, ReplayError> {
    let recorded_trace = std::fs::read_to_string(source.join("trace.jsonl")).map_err(|e| ReplayError::Io(format!("trace.jsonl: {e}")))?;
    let mut normalized_trace = String::new();
    for (source_line, line) in recorded_trace.lines().zip(rewrite(&recorded_trace, &p.rule_rewrites).lines()) {
        let mut event: frontcap_core::event::BehaviorEvent = serde_json::from_str(line).map_err(|e| ReplayError::Corrupt(e.to_string()))?;
        if let Some(relocation) = p.storage_relocation.as_mut() {
            let source_event: frontcap_core::event::BehaviorEvent = serde_json::from_str(source_line)
                .map_err(|e| ReplayError::Corrupt(e.to_string()))?;
            relocation.event(&source_event.kind, &mut event.kind).await?;
        }
        if let frontcap_core::event::EventKind::RulesInstalled(installed) = &mut event.kind
            && let Some((_, _, expected)) = p.installation_hashes.iter().find(|(revision, from, _)| *revision == installed.revision && from == &installed.sha256)
        { installed.sha256 = expected.clone(); }
        normalized_trace.push_str(&serde_json::to_string(&event).expect("event serializes"));
        normalized_trace.push('\n');
    }
    Ok(normalized_trace)
}

/// Replays the finalized bundle at `source` in a new generation of `ws`, which is stopped at the end —
/// also when the replay fails part way.
pub async fn replay(ws: &Workspace, source: &Path, o: ReplayOptions) -> Result<ReplayReport, ReplayError> {
    let mut p = prepare(ws, source, &o).await?;
    let req = LaunchRequest {
        request_id: RequestId::generate(),
        url: p.launch.url.clone(),
        profile: p.launch.profile.clone(),
        rules: p.launch.rules.clone(),
        passthrough_origins: p.launch.passthrough_origins.clone(),
        watchdog_ms: o.watchdog_ms,
        repo_root: o.repo_root.clone(),
        origin: None,
        restore: None,
    };
    let mut attached = launcher::launch(ws, req, &o.recorder_bin, &o.env).await.map_err(|e| ReplayError::Launch(e.0))?;
    let lease = attached.lease.clone();
    let (replayed, diverged) = execute(&p, &mut attached, false).await;
    // A divergent opening may still suspend its parent. Release dismisses it and completes bounded
    // input cleanup before stop; never send the recorded approval to an unmatched dialog.
    // A deadline can cancel a framed call after sending it. Keep the lease-owning connection alive
    // but use a fresh stream for cleanup, so a late answer cannot desynchronize release or stop.
    let mut cleanup = if diverged.is_some() {
        let socket = crate::runtime::socket_path(&ws.runtime_dir(&attached.launch_id));
        let (client, hello) = frontcap_recorder_protocol::client::RecorderClient::connect(&socket).await
            .map_err(|e| ReplayError::Io(format!("connect replay cleanup: {e}")))?;
        if !matches!(hello, Reply::Hello { launch_id } if launch_id == attached.launch_id) {
            return Err(ReplayError::Io("replay cleanup connected to another generation".into()));
        }
        Some(client)
    } else { None };
    let finalizer = cleanup.as_mut().unwrap_or(&mut attached.client);
    let stop_lease = if diverged.is_some() {
        match answer_of(finalizer.call(Call::Release { lease: lease.clone(), request_id: RequestId::generate() }).await) {
            Answer::Ok(Reply::Released) => None,
            Answer::Err(e) => return Err(ReplayError::Launch(e)),
            other => return Err(ReplayError::Io(format!("unexpected replay cleanup reply: {other:?}"))),
        }
    } else { Some(lease) };
    let stopped = answer_of(finalizer.call(Call::Mutate { lease: stop_lease, request_id: RequestId::generate(), op: Op::Stop }).await);
    if let Answer::Err(e) = stopped {
        return Err(ReplayError::Launch(e));
    }
    let run = launcher::run_id_of(&attached.launch_id);
    let replay_trace = std::fs::read_to_string(ws.runs().join(run.as_str()).join("trace.jsonl")).map_err(|e| ReplayError::Io(format!("replay trace: {e}")))?;
    let normalized_trace = recorded_trace(&mut p, source).await?;
    let difference = compare_replay(&normalized_trace, &replay_trace, &[], &p.recorded).err().map(DifferenceReport::from);
    Ok(ReplayReport {
        source: p.manifest.run.clone(),
        run,
        launch_id: attached.launch_id,
        replayed,
        commands: p.commands.len(),
        equal: diverged.is_none() && difference.is_none(),
        diverged,
        difference,
    })
}

/// Count replay-excluded evidence before the original command, never later events at the same time.
/// No durable effect means there is no proven boundary mapping.
fn removed_recording_taints(events: &[frontcap_core::event::BehaviorEvent], recorded: &[(u64, String)], request: &RequestId) -> u64 {
    use frontcap_core::event::{EventKind, TaintSource};
    let mut requests = std::collections::BTreeSet::new();
    let mut count = 0;
    for event in events {
        let owner = match &event.kind {
            EventKind::RulesInstalled(r) => r.request_id.as_ref(),
            EventKind::Action(a) => Some(&a.request_id),
            EventKind::Intervention(i) => Some(&i.request_id),
            _ => None,
        };
        if owner == Some(request) { return count; }
        if let EventKind::Request(r) = &event.kind
            && r.rule.as_ref().is_some_and(|id| recorded.iter().any(|(rev, rule)| *rev == r.rules_revision && rule == id.as_str()))
        { requests.insert(r.id.clone()); }
        if matches!(&event.kind, EventKind::Taint(t) if t.source == TaintSource::PassthroughContent && requests.contains(&t.detail)) { count += 1; }
    }
    0
}

/// A bundle is evidence only as finalized: every member present with its hash, nothing added, and the
/// re-execution inputs among the members.
fn check_integrity(source: &Path, manifest: &Manifest) -> Result<(), ReplayError> {
    let corrupt = |what: String| ReplayError::Corrupt(what);
    let (Some(runs_root), Some(name)) = (source.parent(), source.file_name().and_then(|n| n.to_str())) else {
        return Err(ReplayError::NotFound(source.display().to_string()));
    };
    let run = RunId::new(name).map_err(|e| corrupt(e.to_string()))?;
    let bundle = Bundle::open(runs_root, &run).map_err(|e| corrupt(e.to_string()))?;
    let inspection = bundle.inspect().map_err(|e| corrupt(e.to_string()))?;
    if !inspection.mismatched.is_empty() || !inspection.missing.is_empty() || !inspection.unexpected.is_empty() {
        return Err(corrupt(format!("mismatched {:?}, missing {:?}, unexpected {:?}", inspection.mismatched, inspection.missing, inspection.unexpected)));
    }
    for member in ["inputs/launch.json", "inputs/commands.jsonl", "trace.jsonl"] {
        if !manifest.members.contains_key(member) {
            return Err(corrupt(format!("{member} is not a member")));
        }
    }
    Ok(())
}
