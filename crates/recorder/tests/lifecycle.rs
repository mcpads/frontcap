mod support;

use frontcap_core::ids::RequestId;
use frontcap_core::rules::RuleSet;
use frontcap_recorder_protocol::client::ClientError;
use frontcap_recorder_protocol::{Call, ErrorCode, Op, Outcome, Reply};

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

fn is_code(r: &Result<Reply, ClientError>, code: ErrorCode) -> bool {
    matches!(r, Err(ClientError::Recorder(e)) if e.code == code)
}

#[tokio::test]
async fn launch_status_lease_and_stop() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut c) = support::spawn_recorder(&ws, support::spec(&ws, "l-1", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let Reply::Status { status } = c.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.t_us.0, 0);
    assert!(!status.lease_held);
    let Reply::Attached { lease, .. } = c.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let no_lease = c.call(Call::Mutate { lease: None, request_id: rid("m-1"), op: Op::Advance { by_us: 1000 } }).await;
    assert!(is_code(&no_lease, ErrorCode::LeaseRequired), "{no_lease:?}");
    let done = c.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-2"), op: Op::Advance { by_us: 1_000_000 } }).await.unwrap();
    assert!(matches!(done, Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. } if t_us.0 == 1_000_000), "{done:?}");
    let mut observer = support::connect(&dir).await;
    let saved = observer.call(Call::Result { request_id: rid("m-2") }).await.unwrap();
    assert_eq!(saved, Reply::Result { request_id: rid("m-2"), result: frontcap_recorder_protocol::RequestResult::Completed { answer: Box::new(frontcap_recorder_protocol::Answer::Ok(done)) } });
    assert_eq!(observer.call(Call::Result { request_id: rid("unknown") }).await.unwrap(), Reply::Result { request_id: rid("unknown"), result: frontcap_recorder_protocol::RequestResult::Unknown });
    let Reply::Status { status } = observer.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.t_us.0, 1_000_000);
    drop(observer);
    let stopped = c.call(Call::Mutate { lease: Some(lease), request_id: rid("m-3"), op: Op::Stop }).await.unwrap();
    assert!(matches!(stopped, Reply::Done { outcome: Outcome::Stopped { finalized: true }, .. }), "{stopped:?}");
    support::wait_until_exited(&dir).await;
}

#[tokio::test]
async fn second_reattach_gets_lease_held() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut a) = support::spawn_recorder(&ws, support::spec(&ws, "l-2", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let mut b = support::connect(&dir).await;
    let (x, y) = tokio::join!(a.call(Call::Reattach { request_id: rid("a-1") }), b.call(Call::Reattach { request_id: rid("b-1") }));
    let attached = [&x, &y].iter().filter(|r| matches!(r, Ok(Reply::Attached { .. }))).count();
    let held = [&x, &y].iter().filter(|r| is_code(r, ErrorCode::LeaseHeld)).count();
    assert_eq!((attached, held), (1, 1), "{x:?} {y:?}");
    drop((a, b));
    support::stop(&dir).await;
}

#[tokio::test]
async fn lease_expires_when_its_connection_closes() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut a) = support::spawn_recorder(&ws, support::spec(&ws, "l-3", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    a.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap();
    drop(a);
    let mut b = support::connect(&dir).await;
    let mut attached = false;
    for _ in 0..50 {
        if let Ok(Reply::Attached { .. }) = b.call(Call::Reattach { request_id: rid("b-1") }).await {
            attached = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(attached);
    drop(b);
    support::stop(&dir).await;
}

/// A server that answers each request only when the test releases it: a request the page makes there
/// keeps an advance running (virtual time does not flow while a passed-through request is outstanding)
/// for exactly as long as the test needs.
fn gated_backend() -> (std::net::SocketAddr, std::sync::mpsc::Sender<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let _ = released.recv_timeout(std::time::Duration::from_secs(30));
            let _ = req.respond(tiny_http::Response::from_string("late").with_header(tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap()));
        }
    });
    (addr, release)
}

/// A recorder whose page has a request outstanding at the gated backend, and a mutation that runs until
/// the backend is released: `(dir, attached client, lease, release)`.
async fn running_until_released(ws: &frontcap_recorder::launcher::Workspace, name: &str) -> (support::Runtime, frontcap_recorder_protocol::client::RecorderClient, frontcap_recorder_protocol::LeaseToken, std::sync::mpsc::Sender<()>) {
    let addr = support::serve();
    let (gate, release) = gated_backend();
    let rules = RuleSet::from_json(&format!(r#"[{{"id":"gate","match":{{"url":"http://{gate}/*"}},"action":{{"type":"passthrough"}}}}]"#)).unwrap();
    let (dir, mut a) = support::spawn_recorder(ws, support::spec(ws, name, &format!("http://{addr}/api.html?api=http://{gate}"), rules)).await;
    let Reply::Attached { lease, .. } = a.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    a.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("load"), op: Op::Advance { by_us: 50_000 } }).await.unwrap();
    a.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("call"), op: Op::Evaluate { expression: "call('/slow')".into() } }).await.unwrap();
    (dir, a, lease, release)
}

/// Waits until the recorder reports `id` running: the overlap the busy answers are tested in.
async fn until_running(b: &mut frontcap_recorder_protocol::client::RecorderClient, id: &RequestId) {
    for _ in 0..500 {
        let Reply::Status { status } = b.call(Call::Status).await.unwrap() else { panic!() };
        if status.running.as_ref() == Some(id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{id} never ran");
}

#[tokio::test]
async fn busy_while_another_mutation_runs() {
    let (_tmp, ws) = support::workspace();
    let (dir, mut a, lease, release) = running_until_released(&ws, "l-4").await;
    let mut b = support::connect(&dir).await;
    let long = a.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 30_000_000 } });
    let probe = async {
        until_running(&mut b, &rid("m-1")).await;
        let pending = b.call(Call::Result { request_id: rid("m-1") }).await.unwrap();
        assert!(matches!(pending, Reply::Result { result: frontcap_recorder_protocol::RequestResult::Pending { .. }, .. }), "{pending:?}");
        let busy = b.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-2"), op: Op::Advance { by_us: 1 } }).await;
        let Reply::Status { status } = b.call(Call::Status).await.unwrap() else { panic!() };
        release.send(()).unwrap();
        (busy, status)
    };
    let (done, (busy, status)) = tokio::join!(long, probe);
    assert!(is_code(&busy, ErrorCode::Busy), "{busy:?}");
    assert_eq!(status.running, Some(rid("m-1")));
    assert!(matches!(done, Ok(Reply::Done { .. })), "{done:?}");
    drop((a, b));
    support::stop(&dir).await;
}

#[tokio::test]
async fn long_runtime_dir_still_listens() {
    let addr = support::serve();
    let (_tmp, ws) = support::deep_workspace(140);
    let (dir, mut c) = support::spawn_recorder(&ws, support::spec(&ws, "l-5", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    assert!(matches!(c.call(Call::Status).await.unwrap(), Reply::Status { .. }));
    drop(c);
    support::stop(&dir).await;
}

#[tokio::test]
async fn retried_launch_returns_the_same_generation() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let req = support::launch_request("retry-1", &format!("http://{addr}/appear.html"));
    let first = frontcap_recorder::launcher::launch(&ws, req.clone(), &support::recorder_bin(), &[]).await.unwrap();
    let id = first.launch_id.clone();
    let _guard = support::Runtime(ws.runtime_dir(&id));
    drop(first); // the launching controller dies; its lease expires
    let mut second = None;
    for _ in 0..50 {
        match frontcap_recorder::launcher::launch(&ws, req.clone(), &support::recorder_bin(), &[]).await {
            Ok(a) => {
                second = Some(a);
                break;
            }
            Err(e) if e.0.code == ErrorCode::LeaseHeld => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            Err(e) => panic!("{e:?}"),
        }
    }
    let second = second.expect("relaunch attached");
    assert_eq!(second.launch_id, id);
    let runtimes: Vec<_> = std::fs::read_dir(ws.root.join("runtimes")).unwrap().collect();
    assert_eq!(runtimes.len(), 1, "no second generation was started");
    let mut changed = req.clone();
    changed.url.push_str("?other");
    let conflict = frontcap_recorder::launcher::launch(&ws, changed, &support::recorder_bin(), &[]).await.unwrap_err();
    assert_eq!(conflict.0.code, ErrorCode::RequestIdConflict);
    drop(second);
    support::stop(&ws.runtime_dir(&id)).await;
}

#[tokio::test]
async fn retried_launch_after_the_launcher_died_before_starting_anything() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let req = support::launch_request("retry-2", &format!("http://{addr}/appear.html"));
    // The registry entry exists, but the launching process died before it started the recorder.
    let args = frontcap_core::journal::args_sha256(&serde_json::to_value(&req).unwrap());
    frontcap_recorder::launcher::register(&ws, &req.request_id, &args).unwrap();
    let attached = frontcap_recorder::launcher::launch(&ws, req, &support::recorder_bin(), &[]).await.unwrap();
    let dir = ws.runtime_dir(&attached.launch_id);
    let _guard = support::Runtime(dir.clone());
    assert_eq!(attached.status.t_us.0, 0);
    drop(attached);
    support::stop(&dir).await;
}

#[tokio::test]
async fn a_failed_launch_leaves_no_browser_behind() {
    let (_tmp, ws) = support::workspace();
    // Nothing listens on port 1: the first navigation fails after the browser has started.
    let spec = support::spec(&ws, "l-6", "http://127.0.0.1:1/", RuleSet::default());
    let dir = ws.runtime_dir(&spec.launch_id);
    let _guard = support::Runtime(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();
    frontcap_recorder::runtime::save_spec(&dir, &spec).unwrap();
    let status = std::process::Command::new(support::recorder_bin()).args(["launch", "--runtime-dir"]).arg(&dir).stderr(std::process::Stdio::null()).status().unwrap();
    assert!(!status.success());
    let record = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    assert!(matches!(record.state, frontcap_recorder::runtime::RuntimeState::Failed { .. }), "{record:?}");
    let browser = record.browser.expect("the browser's identity was recorded as soon as it started");
    assert!(!browser.is_alive(), "the browser of a failed launch is terminated");
}

#[tokio::test]
async fn effects_and_history_are_observable() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut c) = support::spawn_recorder(&ws, support::spec(&ws, "l-7", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let Reply::Attached { lease, .. } = c.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let late = frontcap_core::ids::WatchName::new("late").unwrap();
    c.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("w-1"), op: Op::WatchAdd {
        name: late.clone(),
        predicate: frontcap_core::predicate::Predicate::Attached { target: frontcap_core::predicate::Locator::Css { selector: ".late".into() } }, context: None } }).await.unwrap();
    let Reply::Status { status } = c.call(Call::Status).await.unwrap() else { panic!() };
    let before = status.observation_seq;
    c.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 2_000_000 } }).await.unwrap();
    let Reply::Status { status } = c.call(Call::Status).await.unwrap() else { panic!() };
    assert!(status.dom_mutations >= 1);
    assert!(status.observation_seq > before);
    let Reply::Effects { effects } = c.call(Call::Effects { after_seq: before, before_seq: None }).await.unwrap() else { panic!() };
    assert!(effects.requests.is_empty() && effects.unmatched_requests.is_empty());
    let Reply::History { samples } = c.call(Call::WatchHistory { name: late }).await.unwrap() else { panic!() };
    assert!(samples.iter().any(|s| s.value == frontcap_core::event::SampleValue::True && s.t_us.0 == 1_234_000));
    drop(c);
    support::stop(&dir).await;
}

#[tokio::test]
async fn a_request_on_its_own_connection_is_not_detached_while_the_lease_lives() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut keep) = support::spawn_recorder(&ws, support::spec(&ws, "l-8", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let Reply::Attached { lease, .. } = keep.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let mut per_call = support::connect(&dir).await;
    per_call.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 1000 } }).await.unwrap();
    drop(per_call);
    drop(keep); // the lease ends now
    let mut b = support::connect(&dir).await;
    let mut attached = None;
    for _ in 0..100 {
        if let Ok(Reply::Attached { completed_while_detached, .. }) = b.call(Call::Reattach { request_id: rid("b-1") }).await {
            attached = Some(completed_while_detached);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(attached.unwrap(), Vec::<RequestId>::new(), "m-1 completed while its lease lived");
    drop(b);
    support::stop(&dir).await;
}

mod observe_calls {
    use super::*;
    use frontcap_core::predicate::Locator;
    use frontcap_recorder_protocol::{ConsoleFilter, Observation, ObserveOp};

    async fn attached(ws: &frontcap_recorder::launcher::Workspace, run: &str, page: &str) -> (support::Runtime, frontcap_recorder_protocol::client::RecorderClient, frontcap_recorder_protocol::LeaseToken) {
        let addr = support::serve();
        let (dir, mut c) = support::spawn_recorder(ws, support::spec(ws, run, &format!("http://{addr}/{page}"), RuleSet::default())).await;
        let Reply::Attached { lease, .. } = c.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
        (dir, c, lease)
    }

    #[tokio::test]
    async fn observe_snapshot_query_and_console() {
        let (_tmp, ws) = support::workspace();
        let (dir, mut c, lease) = attached(&ws, "o-1", "observe.html").await;
        c.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 200_000 } }).await.unwrap();
        let Reply::Observed { result: Observation::Snapshot(s) } = c.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await.unwrap() else { panic!() };
        let save = s.nodes.iter().find(|n| n.role == "button" && n.name == "Save").expect("the button").r#ref.clone();
        c.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("c-1"), op: Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Ref { id: save.clone() } }, context: None } }).await.unwrap();
        let Reply::Observed { result: Observation::Query(q) } = c
            .call(Call::Observe { op: ObserveOp::Query { target: Locator::Css { selector: "#count".into() }, props: vec!["text".into()] }, context: None })
            .await
            .unwrap() else { panic!() };
        assert_eq!(q.values["text"], "1");
        let Reply::Observed { result: Observation::Console { entries } } = c
            .call(Call::Observe { op: ObserveOp::Console { filter: ConsoleFilter { level: Some("error".into()), ..Default::default() } }, context: None })
            .await
            .unwrap() else { panic!() };
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!((entries[0].t_us.0, entries[0].text.as_str()), (150_000, "broken"));
        let evidence = std::fs::read_to_string(s_bundle(&mut c).await.join("evidence.jsonl")).unwrap();
        assert!(evidence.lines().filter(|l| l.contains("\"observation\"")).count() >= 3, "{evidence}");
        drop(c);
        support::stop(&dir).await;
    }

    async fn s_bundle(c: &mut frontcap_recorder_protocol::client::RecorderClient) -> std::path::PathBuf {
        let Reply::Status { status } = c.call(Call::Status).await.unwrap() else { panic!() };
        status.bundle
    }

    #[tokio::test]
    async fn observations_answer_busy_while_running() {
        let (_tmp, ws) = support::workspace();
        let (dir, mut a, lease, release) = super::running_until_released(&ws, "o-2").await;
        let mut b = support::connect(&dir).await;
        let long = a.call(Call::Mutate { lease: Some(lease), request_id: rid("m-1"), op: Op::Advance { by_us: 30_000_000 } });
        let probe = async {
            super::until_running(&mut b, &rid("m-1")).await;
            let busy = b.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await;
            release.send(()).unwrap();
            busy
        };
        let (_, busy) = tokio::join!(long, probe);
        assert!(is_code(&busy, ErrorCode::Busy), "{busy:?}");
        drop((a, b));
        support::stop(&dir).await;
    }

    #[tokio::test]
    async fn evaluate_is_tainted_and_answered_once() {
        let (_tmp, ws) = support::workspace();
        let (dir, mut c, lease) = attached(&ws, "o-3", "observe.html").await;
        let call = || Call::Mutate { lease: Some(lease.clone()), request_id: rid("e-1"), op: Op::Evaluate { expression: "window.__n = (window.__n || 0) + 1".into() } };
        let first = c.call(call()).await.unwrap();
        assert!(matches!(&first, Reply::Done { outcome: Outcome::Evaluated { value }, .. } if *value == serde_json::json!(1)), "{first:?}");
        assert_eq!(c.call(call()).await.unwrap(), first, "answered from the journal, not run twice");
        let trace = std::fs::read_to_string(s_bundle(&mut c).await.join("trace.jsonl")).unwrap();
        assert!(trace.lines().any(|l| l.contains("arbitrary_evaluate") && l.contains("window.__n")), "{trace}");
        drop(c);
        support::stop(&dir).await;
    }

    #[tokio::test]
    async fn evaluate_reports_exceptions() {
        let (_tmp, ws) = support::workspace();
        let (dir, mut c, lease) = attached(&ws, "o-4", "observe.html").await;
        let e = c.call(Call::Mutate { lease: Some(lease), request_id: rid("e-1"), op: Op::Evaluate { expression: "throw new Error('nope')".into() } }).await;
        assert!(matches!(&e, Err(ClientError::Recorder(r)) if r.code == ErrorCode::ScriptFailed && r.message.contains("nope")), "{e:?}");
        drop(c);
        support::stop(&dir).await;
    }
}

/// A role locator resolves after a snapshot: both read the same accessibility tree (through the recorder,
/// `Accessibility.queryAXTree` right after a snapshot never answered).
#[tokio::test]
async fn a_role_locator_resolves_after_a_snapshot() {
    use frontcap_core::action::Action;
    use frontcap_core::predicate::Locator;
    use frontcap_recorder_protocol::ObserveOp;
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut c) = support::spawn_recorder(&ws, support::spec(&ws, "l-r", &format!("http://{addr}/profile.html"), RuleSet::default())).await;
    let Reply::Attached { lease, .. } = c.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let m = |id: &str, op| Call::Mutate { lease: Some(lease.clone()), request_id: rid(id), op };
    c.call(m("s-1", Op::Settle { idle_us: 250_000, max_us: 10_000_000 })).await.unwrap();
    c.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await.unwrap();
    let wall = std::time::Instant::now();
    let save = Locator::Role { role: "button".into(), name: Some("Save".into()) };
    let clicked = c.call(m("c-1", Op::Act { action: Action::Click { target: save }, then: None, context: None })).await;
    assert!(matches!(clicked, Ok(Reply::Done { outcome: Outcome::Acted { .. }, .. })), "{clicked:?}");
    assert!(wall.elapsed() < std::time::Duration::from_secs(2), "{:?}", wall.elapsed());
    c.call(m("stop", Op::Stop)).await.unwrap();
    support::wait_until_exited(&dir).await;
}
