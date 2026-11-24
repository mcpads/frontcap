//! Live recorder parent/dependent execution with recorded hold rules; MCP wiring remains separate.
mod support;

use frontcap_chromium::generation::{Generation, LaunchOptions};
use frontcap_core::{ids::RequestId, journal::Journal, rules::RuleSet};
use frontcap_recorder::{control::ControlState, runtime, server::{self, Recorder}};
use frontcap_recorder_protocol::{Answer, Call, DialogResponse, ErrorCode, LeaseToken, Op, Outcome, Reply, RequestResult};
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use std::time::Duration;

fn id(text: &str) -> RequestId { RequestId::new(text).unwrap() }

async fn call(client: &mut RecorderClient, request: Call) -> Result<Reply, ClientError> {
    tokio::time::timeout(Duration::from_secs(5), client.call(request)).await.expect("recorder call stalled")
}

type LiveRecorder = (tempfile::TempDir, std::path::PathBuf, std::sync::Arc<Recorder>, tokio::task::JoinHandle<std::io::Result<()>>, RecorderClient, LeaseToken, frontcap_core::process::ProcessIdentity);

async fn fixture() -> LiveRecorder {
    let addr = support::serve();
    let (temp, ws) = support::workspace();
    let spec = support::spec(&ws, "dialogs", &format!("http://{addr}/observe.html"), RuleSet::default());
    let g = Generation::launch(LaunchOptions {
        repo_root: spec.repo_root.clone(), runs_root: spec.runs_root.clone(), run: spec.run_id.clone(),
        url: spec.url.clone(), profile: spec.profile.clone(), rules: spec.rules.clone(), passthrough_origins: None,
        watchdog: Duration::from_secs(2), browser_profile: None, launch_id: Some(spec.launch_id.clone()),
        on_browser: None, proxy: None, upstream: None, cell: None,
    }).await.unwrap();
    let dir = temp.path().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    let path = runtime::journal_path(&dir);
    let journal = Journal::open(&path).unwrap();
    let browser = frontcap_core::process::ProcessIdentity::of(g.endpoint().browser_pid).unwrap().unwrap();
    let rec = Recorder::new(dir.clone(), spec, g, journal, ControlState::default());
    let socket = dir.join("recorder.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serving = tokio::spawn(server::serve(rec.clone(), listener));
    let (mut client, _) = RecorderClient::connect(&socket).await.unwrap();
    let Reply::Attached { lease, .. } = call(&mut client, Call::Reattach { request_id: id("attach") }).await.unwrap() else { panic!() };
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("policy"), op: Op::DialogRules {
        rules: serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap(),
    }}).await.unwrap();
    (temp, dir, rec, serving, client, lease, browser)
}

#[tokio::test]
async fn a_suspended_parent_accepts_correlated_answers_and_publishes_one_terminal_result() {
    let (_temp, dir, _rec, serving, mut client, lease, _browser) = fixture().await;
    let path = runtime::journal_path(&dir);
    let setup = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("setup"), op: Op::Evaluate {
        expression: "window.answers=[]; setTimeout(()=>{ answers.push(confirm('first')); answers.push(prompt('second','default')); },10); true".into()
    }}).await.unwrap();
    assert!(matches!(setup, Reply::Done { .. }));
    let parent = id("advance");
    let Reply::Suspended { operation } = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: parent.clone(), op: Op::Advance { by_us: 50_000 } }).await.unwrap() else { panic!("expected a live suspended operation") };
    assert_eq!(operation.request_id, parent);
    assert_eq!(operation.dialog.as_ref().unwrap().message, "first");
    assert_eq!(operation.dialog.as_ref().unwrap().t_us.0, 10_000);
    tokio::time::sleep(Duration::from_secs(3)).await; // longer than the parent's active-service watchdog
    assert_eq!(call(&mut client, Call::Result { request_id: parent.clone() }).await.unwrap(), Reply::Result { request_id: parent.clone(), result: RequestResult::Pending { dispatched: true } });
    let busy = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("unrelated"), op: Op::Advance { by_us: 1 } }).await;
    assert!(matches!(busy, Err(ClientError::Recorder(e)) if e.code == ErrorCode::Busy));
    let response = DialogResponse { parent_request_id: parent.clone(), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: true, prompt_text: None };
    let denied = call(&mut client, Call::AnswerDialog { lease: LeaseToken::generate(), request_id: id("denied"), response: response.clone() }).await;
    assert!(matches!(denied, Err(ClientError::Recorder(e)) if e.code == ErrorCode::LeaseRequired));
    let stale = DialogResponse { dialog_id: "dialog-stale".into(), ..response.clone() };
    assert!(matches!(call(&mut client, Call::AnswerDialog { lease: lease.clone(), request_id: id("stale"), response: stale }).await,
        Err(ClientError::Recorder(e)) if e.code == ErrorCode::NoSuchHold));
    assert_eq!(Journal::read_state(&path, &id("stale")).unwrap(), None);
    let wrong = DialogResponse { parent_request_id: id("wrong"), ..response.clone() };
    assert!(call(&mut client, Call::AnswerDialog { lease: lease.clone(), request_id: id("wrong-parent"), response: wrong }).await.is_err());
    assert_eq!(Journal::read_state(&path, &id("denied")).unwrap(), None);
    assert_eq!(Journal::read_state(&path, &id("wrong-parent")).unwrap(), None);
    let invalid_prompt = DialogResponse { prompt_text: Some("invalid on confirm".into()), ..response.clone() };
    assert!(matches!(call(&mut client, Call::AnswerDialog { lease: lease.clone(), request_id: id("invalid-prompt"), response: invalid_prompt }).await,
        Err(ClientError::Recorder(e)) if e.code == ErrorCode::InvalidArguments));
    assert_eq!(Journal::read_state(&path, &id("invalid-prompt")).unwrap(), None);
    let first = Call::AnswerDialog { lease: lease.clone(), request_id: id("z-answer-first"), response };
    let first_answer = call(&mut client, first.clone()).await.unwrap();
    assert!(matches!(&first_answer, Reply::DialogAnswered { receipt } if receipt.accept && Some(&receipt.dialog) == operation.dialog.as_ref()));
    assert_eq!(call(&mut client, Call::Result { request_id: id("z-answer-first") }).await.unwrap(), Reply::Result {
        request_id: id("z-answer-first"), result: RequestResult::Completed { answer: Box::new(Answer::Ok(first_answer.clone())) },
    });
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Reply::Status { status } = call(&mut client, Call::Status).await.unwrap() else { panic!() };
            assert_eq!(status.running.as_ref(), Some(&parent));
            if let Some(s) = status.suspension && s.dialog.as_ref().unwrap().id != operation.dialog.as_ref().unwrap().id { break s }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(second.dialog.as_ref().unwrap().message, "second");
    assert_eq!(second.dialog.as_ref().unwrap().default_prompt, "default");
    assert_eq!(call(&mut client, first.clone()).await.unwrap(), first_answer, "retry answered the second dialog");
    let mut conflicting = first.clone();
    if let Call::AnswerDialog { response, .. } = &mut conflicting { response.accept = false; }
    assert!(matches!(call(&mut client, conflicting).await, Err(ClientError::Recorder(e)) if e.code == ErrorCode::RequestIdConflict));
    call(&mut client, Call::AnswerDialog { lease: lease.clone(), request_id: id("a-answer-second"), response: DialogResponse {
        parent_request_id: parent.clone(), dialog_id: second.dialog.as_ref().unwrap().id.clone(), accept: true, prompt_text: Some("한글 🍎".into()),
    }}).await.unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Reply::Result { result, .. } = call(&mut client, Call::Result { request_id: parent.clone() }).await.unwrap() else { panic!() };
            if let RequestResult::Completed { answer } = result { break *answer }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(matches!(completed, Answer::Ok(Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. }) if t_us.0 == 50_000));
    assert_eq!(call(&mut client, first).await.unwrap(), first_answer, "completed parent invalidated a saved child answer");
    let value = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("read"), op: Op::Evaluate { expression: "answers".into() } }).await.unwrap();
    assert!(matches!(value, Reply::Done { outcome: Outcome::Evaluated { value }, .. } if value == serde_json::json!([true,"한글 🍎"])));
    let Reply::Status { status } = call(&mut client, Call::Status).await.unwrap() else { panic!() };
    call(&mut client, Call::Mutate { lease: Some(lease), request_id: id("stop"), op: Op::Stop }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving).await.unwrap().unwrap().unwrap();
    let commands = frontcap_core::inputs::read_commands(&status.bundle).unwrap();
    let parents: Vec<_> = commands.iter().filter(|c| c.request_id == parent).collect();
    assert_eq!(parents.len(), 1, "parent was published more than once");
    let inputs = &parents[0].dialogs;
    assert_eq!(inputs.iter().map(|d| d.request_id.clone()).collect::<Vec<_>>(), [id("z-answer-first"), id("a-answer-second")], "archive must retain admission order");
    for (input, message) in inputs.iter().zip(["first", "second"]) {
        let attempt = input.attempt.as_ref().unwrap();
        assert_eq!(attempt.origin, frontcap_core::inputs::DialogAnswerOrigin::Agent);
        assert_eq!(attempt.dialog.message, message);
        assert!(attempt.accept);
        let frontcap_core::inputs::DialogInputOutcome::Answered { receipt } = &input.result else { panic!("answer lost from archive") };
        assert_eq!(receipt.dialog, attempt.dialog);
        assert_eq!(receipt.request_id, input.request_id);
    }
    assert_eq!(inputs[1].attempt.as_ref().unwrap().prompt_text.as_deref(), Some("한글 🍎"));
    assert!(!commands.iter().any(|c| inputs.iter().any(|d| d.request_id == c.request_id)), "dependent must not become a root command");
}


async fn cleanup_chain(explicit_release: bool, predicate_wait: bool) {
    let (_temp, dir, _rec, serving, mut client, lease, _browser) = fixture().await;
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("setup"), op: Op::Evaluate {
        expression: "window.answers=[]; setTimeout(()=>{answers.push(confirm('first'));answers.push(prompt('second','default'));document.body.dataset.answered='yes';},10);true".into(),
    }}).await.unwrap();
    let op = if predicate_wait {
        Op::UntilPredicate { predicate: frontcap_core::predicate::Predicate::Attached {
            target: frontcap_core::predicate::Locator::Css { selector: "body[data-answered=yes]".into() },
        }, max_us: 50_000, context: None }
    } else { Op::Advance { by_us: 50_000 } };
    let Reply::Suspended { .. } = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("parent"), op }).await.unwrap() else { panic!() };
    if predicate_wait {
        let state: ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
        assert!(state.predicate_watches.contains_key(&id("parent")), "suspension must retain cleanup ownership");
    }
    let (mut observer, _) = RecorderClient::connect(&dir.join("recorder.sock")).await.unwrap();
    if explicit_release {
        assert!(matches!(call(&mut client, Call::Release { lease, request_id: id("release") }).await.unwrap(), Reply::Released));
    }
    drop(client);
    let new_lease = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match call(&mut observer, Call::Reattach { request_id: id("replacement") }).await {
                Ok(Reply::Attached { lease, status, .. }) => {
                    assert!(!status.input_cleanup_pending);
                    assert!(status.running.is_none());
                    assert!(status.suspension.is_none());
                    break lease;
                }
                Err(ClientError::Recorder(e)) if matches!(e.code, ErrorCode::Busy | ErrorCode::LeaseHeld) => tokio::task::yield_now().await,
                other => panic!("unexpected reattach: {other:?}"),
            }
        }
    }).await.unwrap();
    let Reply::Result { result: RequestResult::Completed { answer }, .. } = call(&mut observer, Call::Result { request_id: id("parent") }).await.unwrap() else { panic!() };
    assert!(matches!(*answer, Answer::Ok(Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. }) if t_us.0 == if predicate_wait { 10_000 } else { 50_000 }));
    let state: ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
    assert!(state.predicate_watches.is_empty());
    assert!(state.watches.list().is_empty());
    assert_eq!(state.dialog_answers.len(), 2);
    for (request, plan) in state.dialog_answers {
        assert_eq!(plan.origin, frontcap_recorder::control::DialogAnswerOrigin::LeaseCleanup);
        assert!(!plan.response.accept);
        assert_eq!(plan.response.parent_request_id, id("parent"));
        assert!(matches!(Journal::read_state(&runtime::journal_path(&dir), &request).unwrap(), Some(frontcap_core::journal::RequestState::Completed(_))));
    }
    let value = call(&mut observer, Call::Mutate { lease: Some(new_lease.clone()), request_id: id("read"), op: Op::Evaluate { expression: "answers".into() } }).await.unwrap();
    assert!(matches!(value, Reply::Done { outcome: Outcome::Evaluated { value }, .. } if value == serde_json::json!([false,null])));
    let Reply::Status { status } = call(&mut observer, Call::Status).await.unwrap() else { panic!() };
    call(&mut observer, Call::Mutate { lease: Some(new_lease), request_id: id("stop"), op: Op::Stop }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving).await.unwrap().unwrap().unwrap();
    let commands = frontcap_core::inputs::read_commands(&status.bundle).unwrap();
    let parent = commands.iter().find(|c| c.request_id == id("parent")).unwrap();
    assert_eq!(parent.dialogs.len(), 2);
    for (input, message) in parent.dialogs.iter().zip(["first", "second"]) {
        let attempt = input.attempt.as_ref().unwrap();
        assert_eq!(attempt.origin, frontcap_core::inputs::DialogAnswerOrigin::LeaseCleanup);
        assert_eq!(attempt.dialog.message, message);
        assert!(!attempt.accept);
        assert!(matches!(&input.result, frontcap_core::inputs::DialogInputOutcome::Answered { receipt } if !receipt.accept));
    }
}

#[tokio::test]
async fn disconnect_dismisses_chained_dialogs_before_replacement_lease() { cleanup_chain(false, false).await; }

#[tokio::test]
async fn release_dismisses_chained_dialogs_before_returning() { cleanup_chain(true, false).await; }

#[tokio::test]
async fn unbounded_dialog_continuation_faults_instead_of_hanging_cleanup() {
    let (_temp, dir, rec, serving, mut client, lease, browser) = fixture().await;
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("setup"), op: Op::Evaluate {
        expression: "setTimeout(()=>{while(true)confirm('again');},10);true".into(),
    }}).await.unwrap();
    assert!(matches!(call(&mut client, Call::Mutate { lease: Some(lease), request_id: id("parent"), op: Op::Advance { by_us: 50_000 } }).await.unwrap(), Reply::Suspended { .. }));
    drop(client);
    tokio::time::timeout(Duration::from_secs(25), serving).await.expect("unbounded cleanup").unwrap().unwrap();
    assert!(rec.is_stopped());
    assert!(!browser.is_alive(), "bounded failure left the owned browser running");
    let state: ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
    assert_eq!(state.dialog_answers.len(), 64);
    assert!(state.faults.iter().any(|f| f.detail.contains("64 consecutive dialogs") && f.at_us.0 == 10_000));
}

#[tokio::test]
async fn releasing_a_held_key_can_dismiss_its_own_cleanup_dialog() {
    let (_temp, dir, _rec, serving, mut client, lease, _browser) = fixture().await;
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("ready"), op: Op::Advance { by_us: 1_000 } }).await.unwrap();
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("setup"), op: Op::Evaluate {
        expression: "window.released=[];document.body.tabIndex=0;document.body.focus();window.addEventListener('keyup',()=>released.push(confirm('keyup cleanup')));true".into(),
    }}).await.unwrap();
    call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("shift"), op: Op::Act {
        action: frontcap_core::action::Action::KeyDown { key: "Shift".into() }, then: None, context: None,
    }}).await.unwrap();
    call(&mut client, Call::Release { lease, request_id: id("release") }).await.unwrap();
    let Reply::Attached { lease, status, .. } = call(&mut client, Call::Reattach { request_id: id("again") }).await.unwrap() else { panic!() };
    assert!(status.keys.is_empty());
    assert_eq!(status.t_us.0, 1_000, "cleanup introduced an undeclared virtual grant");
    let value = call(&mut client, Call::Mutate { lease: Some(lease.clone()), request_id: id("read"), op: Op::Evaluate { expression: "released".into() } }).await.unwrap();
    assert!(matches!(value, Reply::Done { outcome: Outcome::Evaluated { value }, .. } if value == serde_json::json!([false])));
    let state: ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
    assert_eq!(state.dialog_answers.len(), 1);
    assert!(state.dialog_answers.values().all(|p| p.origin == frontcap_recorder::control::DialogAnswerOrigin::LeaseCleanup));
    call(&mut client, Call::Mutate { lease: Some(lease), request_id: id("stop"), op: Op::Stop }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn predicate_wait_retains_its_watch_during_suspension_and_cleans_it_after_disconnect() {
    cleanup_chain(false, true).await;
}
