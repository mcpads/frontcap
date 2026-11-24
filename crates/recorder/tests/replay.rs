//! Re-execution of a recorded run against the same build and against a changed one.
mod support;

use frontcap_core::action::Action;
use frontcap_core::ids::{RequestId, WatchName};
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_recorder::launcher::{self, Attached, Workspace};
use frontcap_recorder::replay::{ReplayOptions, replay};
use frontcap_recorder_protocol::{Call, ObserveOp, Observation, Op, Reply, SettleSpec};
use std::path::PathBuf;

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

async fn launch(ws: &Workspace, url: String, rules: RuleSet) -> Attached {
    let req = launcher::LaunchRequest { rules, ..support::launch_request(&RequestId::generate().to_string(), &url) };
    launcher::launch(ws, req, &support::recorder_bin(), &[]).await.unwrap()
}

async fn mutate(a: &mut Attached, id: &str, op: Op) -> Reply {
    a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: rid(id), op }).await.unwrap()
}

async fn stop(ws: &Workspace, mut a: Attached) -> PathBuf {
    mutate(&mut a, "stop", Op::Stop).await;
    support::wait_until_exited(&ws.runtime_dir(&a.launch_id)).await;
    ws.runs().join(launcher::run_id_of(&a.launch_id).as_str())
}

fn options(url: Option<String>) -> ReplayOptions {
    ReplayOptions { url, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![] }
}

/// Delayed and agent-held script responses keep the page inspectable, execute at their answer's
/// instant and retain those facts when the command stream is replayed.
#[tokio::test]
async fn delayed_and_held_scripts_replay_their_response_instants() {
    use frontcap_recorder_protocol::Outcome;
    use serde_json::json;
    let addr = support::serve_dir("../adapter-chromium/tests/fixtures");
    for kind in ["classic", "async", "module", "inserted"] {
        for held in [false, true] {
            let (_tmp, ws) = support::workspace();
            let response = MockSpec {
                status: 200,
                headers: [("content-type".into(), "text/javascript".into()), ("access-control-allow-origin".into(), "*".into())].into(),
                body: "events.push(['script', performance.now()]); document.querySelector('#status').textContent = 'ready';".into(),
                body_base64: None, latency_ms: if held { 0 } else { 300 },
            };
            let action = if held { json!({"type": "hold"}) } else {
                let mut action = serde_json::to_value(&response).unwrap();
                action["type"] = json!("mock"); action
            };
            let rules = RuleSet::from_json(&json!([{"id":"script", "match":{"url":"https://scripts.test/*"}, "action":action}]).to_string()).unwrap();
            let mut a = launch(&ws, format!("http://{addr}/delayed_scripts.html?kind={kind}"), rules).await;
            let _guard = support::Runtime(ws.runtime_dir(&a.launch_id));
            mutate(&mut a, "before", Op::Advance { by_us: 100_000 }).await;
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = mutate(&mut a, "waiting", Op::Evaluate { expression: "events".into() }).await else { panic!() };
            assert_eq!(value, json!([["timer",50]]), "{kind}, held={held}");
            if held {
                let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!() };
                assert_eq!(status.held.len(), 1);
                mutate(&mut a, "answer", Op::Respond { ordinal: status.held[0].clone(), response }).await;
            }
            mutate(&mut a, "after", Op::Advance { by_us: 250_000 }).await;
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = mutate(&mut a, "executed", Op::Evaluate { expression: "events".into() }).await else { panic!() };
            assert_eq!(value, json!([["timer",50],["script",if held {100} else {300}]]), "{kind}, held={held}");
            let source = stop(&ws, a).await;
            let report = replay(&ws, &source, options(None)).await.unwrap();
            assert!(report.equal, "{kind}, held={held}: {report:#?}");
        }
    }
}

/// observe.html: a snapshot, a click by its reference, and waits; replayed against the same page.
#[tokio::test]
async fn a_replayed_run_has_the_same_behavior() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{addr}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 50_000 }).await;
    let Reply::Observed { result: Observation::Snapshot(s) } =
        a.client.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await.unwrap()
    else {
        panic!()
    };
    let save = s.nodes.iter().find(|n| n.role == "button" && n.name == "Save").unwrap().r#ref.clone();
    let click = Op::Act { action: Action::Click { target: Locator::Ref { id: save.clone() } }, then: Some(SettleSpec { idle_us: 250_000, max_us: 2_000_000 }), context: None };
    mutate(&mut a, "c-2", click.clone()).await;
    mutate(&mut a, "c-3", Op::WatchAdd { name: WatchName::new("one").unwrap(), predicate: Predicate::TextEquals { target: Locator::Css { selector: "#count".into() }, text: "1".into() }, context: None }).await;
    mutate(&mut a, "c-4", Op::Advance { by_us: 200_000 }).await;
    let source = stop(&ws, a).await;

    let commands = frontcap_core::inputs::read_commands(&source).unwrap();
    let ids: Vec<&str> = commands.iter().map(|c| c.request_id.as_str()).collect();
    assert_eq!(ids, ["c-1", "c-2", "c-3", "c-4"], "every request but stop");
    assert_eq!(commands[1].op, serde_json::to_value(&click).unwrap());
    assert_eq!(commands[1].refs[&save].name.as_deref(), Some("Save"));
    let manifest: frontcap_core::bundle::Manifest = serde_json::from_str(&std::fs::read_to_string(source.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest.replayable, "{:?}", manifest.missing_inputs);

    let report = replay(&ws, &source, options(None)).await.unwrap();
    assert!(report.equal, "{report:#?}");
    assert_eq!((report.replayed, report.commands), (4, 4));
}

/// appear.html: the element appears 500 ms later in the changed build; the replay says where.
#[tokio::test]
async fn a_changed_build_differs_where_the_behavior_changed() {
    let (recorded_at, changed_at) = (support::serve(), support::serve_dir("tests/fixtures/variant"));
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{recorded_at}/appear.html"), RuleSet::default()).await;
    mutate(&mut a, "w-1", Op::WatchAdd { name: WatchName::new("late").unwrap(), predicate: Predicate::Visible { target: Locator::Css { selector: ".late".into() } }, context: None }).await;
    mutate(&mut a, "u-1", Op::Until { watch: WatchName::new("late").unwrap(), max_us: 5_000_000 }).await;
    let source = stop(&ws, a).await;

    let same = replay(&ws, &source, options(None)).await.unwrap();
    assert!(same.equal, "{same:#?}");
    let changed = replay(&ws, &source, options(Some(format!("http://{changed_at}/appear.html")))).await.unwrap();
    assert!(!changed.equal);
    assert_eq!(changed.diverged, None, "the until still ends with the condition met");
    let d = changed.difference.expect("a difference");
    // The element appears at 1.234 s in the recorded build and 500 ms later in the changed one. Idle
    // frame ticks take no sample, so the changed build's first is the one that sees it.
    let (recorded, replayed) = (d.recorded.unwrap(), d.replayed.unwrap());
    assert!(recorded.contains("\"value\":\"true\"") && recorded.contains("\"t_us\":1234000"), "{recorded}");
    assert!(replayed.contains("\"value\":\"true\"") && replayed.contains("\"t_us\":1734000"), "{replayed}");
}

/// hold.html: the recorded answer to a held request has no request to answer in a build that never asks.
#[tokio::test]
async fn a_command_the_changed_build_cannot_take_diverges() {
    let (recorded_at, changed_at) = (support::serve(), support::serve_dir("tests/fixtures/variant"));
    let (_tmp, ws) = support::workspace();
    let rules = RuleSet::from_json(r#"[{"id":"hold","match":{"url":"https://api.example.test/hold"},"action":{"type":"hold"}}]"#).unwrap();
    let mut a = launch(&ws, format!("http://{recorded_at}/hold.html"), rules).await;
    mutate(&mut a, "a-1", Op::Advance { by_us: 100_000 }).await;
    let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!() };
    let ordinal = status.held.first().cloned().expect("held");
    let response = MockSpec { status: 200, headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(), body: "{}".into(), body_base64: None, latency_ms: 0 };
    mutate(&mut a, "r-1", Op::Respond { ordinal, response }).await;
    mutate(&mut a, "a-2", Op::Advance { by_us: 100_000 }).await;
    let source = stop(&ws, a).await;

    let report = replay(&ws, &source, options(Some(format!("http://{changed_at}/hold.html")))).await.unwrap();
    assert!(!report.equal);
    let d = report.diverged.expect("diverged");
    assert_eq!((d.request_id.as_str(), d.expected.as_str(), d.actual.as_str()), ("r-1", "ack", "error:no_such_hold"));
    assert_eq!(report.replayed, 2);
    assert!(report.difference.is_some(), "the trace differs too");
}

/// A bundle still open, or one whose inputs are incomplete, is refused before anything launches.
#[tokio::test]
async fn an_open_or_incomplete_bundle_is_refused() {
    use frontcap_recorder::replay::ReplayError;
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{addr}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 10_000 }).await;
    let open = ws.runs().join(launcher::run_id_of(&a.launch_id).as_str());
    assert!(matches!(replay(&ws, &open, options(None)).await, Err(ReplayError::NotFinalized(_))));
    let source = stop(&ws, a).await;
    let path = source.join("manifest.json");
    let mut manifest: frontcap_core::bundle::Manifest = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest.replayable = false;
    manifest.missing_inputs = vec!["recorder_fault".into()];
    std::fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
    match replay(&ws, &source, options(None)).await {
        Err(ReplayError::NotReplayable { missing, .. }) => assert_eq!(missing, ["recorder_fault"]),
        other => panic!("{other:?}"),
    }
}

async fn snapshot_ref(a: &mut Attached, role: &str, name: &str) -> String {
    let Reply::Observed { result: Observation::Snapshot(s) } =
        a.client.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await.unwrap()
    else {
        panic!()
    };
    s.nodes.iter().find(|n| n.role == role && n.name == name).unwrap().r#ref.clone()
}

/// A reference whose recorded element is another one in the changed build is stale; it never acts on
/// whatever now sits at the recorded path.
#[tokio::test]
async fn a_reference_to_a_moved_element_diverges_as_stale() {
    let (recorded_at, changed_at) = (support::serve(), support::serve_dir("tests/fixtures/variant"));
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{recorded_at}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 50_000 }).await;
    let save = snapshot_ref(&mut a, "button", "Save").await;
    mutate(&mut a, "c-2", Op::Act { action: Action::Click { target: Locator::Ref { id: save } }, then: None, context: None }).await;
    let source = stop(&ws, a).await;
    let report = replay(&ws, &source, options(Some(format!("http://{changed_at}/observe.html")))).await.unwrap();
    let d = report.diverged.expect("diverged");
    assert_eq!((d.request_id.as_str(), d.expected.as_str(), d.actual.as_str()), ("c-2", "acted", "error:stale_ref"), "{d:?}");
}

/// What `evaluate` answered is part of its outcome, though the trace records only the expression.
#[tokio::test]
async fn an_evaluated_value_that_changed_diverges() {
    let (recorded_at, changed_at) = (support::serve(), support::serve_dir("tests/fixtures/variant"));
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{recorded_at}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 50_000 }).await;
    mutate(&mut a, "e-1", Op::Evaluate { expression: "document.title".into() }).await;
    let source = stop(&ws, a).await;
    assert!(replay(&ws, &source, options(None)).await.unwrap().equal);
    let report = replay(&ws, &source, options(Some(format!("http://{changed_at}/observe.html")))).await.unwrap();
    let d = report.diverged.expect("diverged");
    assert_eq!(d.request_id.as_str(), "e-1");
    assert!(d.expected.starts_with("evaluated:") && d.actual.starts_with("evaluated:") && d.expected != d.actual, "{d:?}");
}

/// A recorded request that failed replays as the same failure.
#[tokio::test]
async fn a_recorded_failure_replays_as_the_same_outcome() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{addr}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 50_000 }).await;
    let missing = a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: rid("c-2"), op: Op::Act { action: Action::Click { target: Locator::Css { selector: "#absent".into() } }, then: None, context: None } }).await;
    assert!(missing.is_err(), "{missing:?}");
    mutate(&mut a, "c-3", Op::Advance { by_us: 50_000 }).await;
    let source = stop(&ws, a).await;
    let commands = frontcap_core::inputs::read_commands(&source).unwrap();
    assert_eq!(commands[1].outcome, "error:not_found");
    let report = replay(&ws, &source, options(None)).await.unwrap();
    assert!(report.equal, "{report:#?}");
    assert_eq!(report.replayed, 3);
}

/// A bundle whose members changed after finalization is not evidence.
#[tokio::test]
async fn an_altered_bundle_is_refused() {
    use frontcap_recorder::replay::ReplayError;
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{addr}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "c-1", Op::Advance { by_us: 10_000 }).await;
    let source = stop(&ws, a).await;
    std::fs::write(source.join("inputs/commands.jsonl"), "").unwrap();
    assert!(matches!(replay(&ws, &source, options(None)).await, Err(ReplayError::Corrupt(_))));
    assert!(matches!(replay(&ws, &source.join("nothing"), options(None)).await, Err(ReplayError::NotFound(_))));
}

#[tokio::test]
async fn dependent_dialog_replay_matches_sources_and_cleans_up_a_divergent_opening() {
    let addr = support::serve();
    let moved = support::serve();
    let changed = support::serve_dir("tests/fixtures/variant");
    let (_temp, ws) = support::workspace();
    let source = record_dialog_chain(&ws, addr, false).await;
    for url in [None, Some(format!("http://{moved}/dialog-replay.html"))] {
        let report = replay(&ws, &source, options(url)).await.unwrap();
        assert!(report.equal, "{report:#?}");
        support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
        let inputs = frontcap_core::inputs::read_commands(&ws.runs().join(report.run.as_str())).unwrap();
        let parent = inputs.iter().find(|c| c.request_id == rid("parent")).unwrap();
        assert_eq!(parent.dialogs.iter().map(|d| d.request_id.clone()).collect::<Vec<_>>(), [rid("z-first"), rid("a-second")]);
    }
    let report = replay(&ws, &source, options(Some(format!("http://{changed}/dialog-replay.html")))).await.unwrap();
    assert_eq!(report.diverged.as_ref().unwrap().actual, "dialog_mismatch", "{report:#?}");
    support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
    let events = frontcap_core::log::BehaviorLog::read(&ws.runs().join(report.run.as_str()).join("trace.jsonl")).unwrap();
    let dialogs: Vec<_> = events.iter().filter_map(|e| match &e.kind { frontcap_core::event::EventKind::Dialog(d) => Some(d), _ => None }).collect();
    assert!(!dialogs.is_empty());
    assert!(dialogs.iter().all(|d| d.answer == "false" || d.answer == "null"), "divergent dialog received the recorded approval: {dialogs:?}");
    for (directory, expected) in [("tests/fixtures/variant-no-dialog", "dialog_missing"), ("tests/fixtures/variant-extra-dialog", "dialog_unexpected")] {
        let addr = support::serve_dir(directory);
        let report = replay(&ws, &source, options(Some(format!("http://{addr}/dialog-replay.html")))).await.unwrap();
        assert_eq!(report.diverged.as_ref().unwrap().actual, expected, "{report:#?}");
        support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
    }
}

async fn record_dialog_chain(ws: &Workspace, addr: std::net::SocketAddr, cleanup: bool) -> PathBuf {
    use frontcap_recorder_protocol::DialogResponse;
    let mut a = launch(ws, format!("http://{addr}/dialog-replay.html"), RuleSet::default()).await;
    mutate(&mut a, "ready", Op::Advance { by_us: 1_000 }).await;
    mutate(&mut a, "policy", Op::DialogRules { rules: serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap() }).await;
    mutate(&mut a, "start", Op::Act { action: Action::Click { target: Locator::Css { selector: "#start".into() } }, then: None, context: None }).await;
    let Reply::Suspended { mut operation } = mutate(&mut a, "parent", Op::Advance { by_us: 50_000 }).await else { panic!("expected suspension") };
    if cleanup {
        a.client.call(Call::Release { lease: a.lease.clone(), request_id: rid("release") }).await.unwrap();
        return stop(ws, a).await;
    }
    for (request, prompt) in [("z-first", None), ("a-second", Some("한글 🍎".to_string()))] {
        a.client.call(Call::AnswerDialog { lease: a.lease.clone(), request_id: rid(request), response: DialogResponse {
            parent_request_id: rid("parent"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: true, prompt_text: prompt,
        }}).await.unwrap();
        if request == "z-first" {
            operation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!() };
                    if let Some(next) = status.suspension && next.dialog.as_ref().unwrap().id != operation.dialog.as_ref().unwrap().id { break next }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
        }
    }
    assert!(matches!(finish_parent(&mut a, "parent").await,
        frontcap_recorder_protocol::Answer::Ok(Reply::Done { outcome: frontcap_recorder_protocol::Outcome::Clock { t_us, .. }, .. }) if t_us.0 == 51_000));
    stop(ws, a).await
}

#[tokio::test]
async fn child_dialog_sources_replay_after_origin_rewriting() {
    let addr = support::serve_dir("tests/fixtures/variant");
    let moved = support::serve_dir("tests/fixtures/variant");
    let (_temp, ws) = support::workspace();
    let source = record_dialog_chain(&ws, addr, false).await;
    let inputs = frontcap_core::inputs::read_commands(&source).unwrap();
    let parent = inputs.iter().find(|c| c.request_id == rid("parent")).unwrap();
    assert!(parent.dialogs.iter().all(|d| d.attempt.as_ref().unwrap().dialog.source.frame_path == [0]));
    let report = replay(&ws, &source, options(Some(format!("http://{moved}/dialog-replay.html")))).await.unwrap();
    assert!(report.equal, "{report:#?}");
    support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
}

async fn finish_parent(a: &mut Attached, parent: &str) -> frontcap_recorder_protocol::Answer {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Reply::Result { result: frontcap_recorder_protocol::RequestResult::Completed { answer }, .. } = a.client.call(Call::Result { request_id: rid(parent) }).await.unwrap()
                && matches!(a.client.call(Call::Status).await.unwrap(), Reply::Status { status } if status.running.is_none()) { break *answer }
            tokio::task::yield_now().await;
        }
    }).await.unwrap()
}

#[tokio::test]
async fn a_dismissed_beforeunload_replays_as_navigation_cancelled() {
    let addr = support::serve();
    let (_temp, ws) = support::workspace();
    let mut a = launch(&ws, format!("http://{addr}/observe.html"), RuleSet::default()).await;
    mutate(&mut a, "ready", Op::Advance { by_us: 1_000 }).await;
    mutate(&mut a, "setup", Op::Evaluate { expression: "document.body.innerHTML='<button id=arm>Arm</button>';addEventListener('beforeunload',e=>{e.preventDefault();e.returnValue='stay'});true".into() }).await;
    mutate(&mut a, "activate", Op::Act { action: Action::Click { target: Locator::Css { selector: "#arm".into() } }, then: None, context: None }).await;
    mutate(&mut a, "policy", Op::DialogRules { rules: serde_json::from_str(r#"[{"kind":"beforeunload","answer":"hold"}]"#).unwrap() }).await;
    let Reply::Suspended { operation } = mutate(&mut a, "navigate", Op::Act { action: Action::Navigate { url: format!("http://{addr}/observe.html?next") }, then: None, context: None }).await else { panic!("beforeunload did not suspend") };
    a.client.call(Call::AnswerDialog { lease: a.lease.clone(), request_id: rid("stay"), response: frontcap_recorder_protocol::DialogResponse {
        parent_request_id: rid("navigate"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: false, prompt_text: None,
    }}).await.unwrap();
    assert!(matches!(finish_parent(&mut a, "navigate").await, frontcap_recorder_protocol::Answer::Err(error) if error.code == frontcap_recorder_protocol::ErrorCode::NavigationCancelled));
    let source = stop(&ws, a).await;
    let report = replay(&ws, &source, options(None)).await.unwrap();
    assert!(report.equal, "{report:#?}");
    support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
}

#[tokio::test]
async fn recorded_lease_cleanup_dismissals_replay_under_the_new_lease() {
    let addr = support::serve();
    let (_temp, ws) = support::workspace();
    let source = record_dialog_chain(&ws, addr, true).await;
    let inputs = frontcap_core::inputs::read_commands(&source).unwrap();
    let parent = inputs.iter().find(|c| c.request_id == rid("parent")).unwrap();
    assert_eq!(parent.dialogs.len(), 2);
    assert!(parent.dialogs.iter().all(|d| d.attempt.as_ref().unwrap().origin == frontcap_core::inputs::DialogAnswerOrigin::LeaseCleanup));
    let report = replay(&ws, &source, options(None)).await.unwrap();
    assert!(report.equal, "{report:#?}");
    support::wait_until_exited(&ws.runtime_dir(&report.launch_id)).await;
}
