mod support;

use frontcap_chromium::generation::{DialogSession, Generation};
use frontcap_core::dialog::{DialogDescriptor, DialogRules};
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::event::{ClockOutcome, EventKind};
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::time::Duration;

async fn pending(dialogs: &DialogSession) -> DialogDescriptor {
    tokio::time::timeout(Duration::from_secs(5), dialogs.wait_pending()).await.unwrap().unwrap()
}

async fn launch(run: &str) -> (tempfile::TempDir, Generation) {
    let root = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut opts = support::options(root.path(), run, format!("http://{addr}/dialogs.html"), RuleSet::default());
    opts.watchdog = Duration::from_secs(2);
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    (root, g)
}

#[tokio::test]
async fn held_timer_dialogs_keep_the_parent_grant_and_require_both_receipt_acknowledgements() {
    let (_root, mut g) = launch("held-dialog-timer").await;
    let rules: DialogRules = serde_json::from_str(r#"[{"message":"manual*","answer":"hold"},{"answer":"accept"}]"#).unwrap();
    g.set_dialog_rules(rules).await.unwrap();
    let dialogs = g.dialog_session().expect("native dialog control requires the patched engine");
    g.eval_main("window.answers=[]; setTimeout(()=>{answers.push(confirm('automatic')); answers.push(confirm('manual confirm')); answers.push(confirm('automatic after first')); answers.push(prompt('manual prompt',null)); answers.push(confirm('manual final'));},100); setTimeout(()=>answers.push('later'),200)").await.unwrap();
    let work = tokio::spawn(async move {
        let result = g.advance(VirtualDuration::from_ms(150)).await;
        (g, result)
    });
    let first = pending(&dialogs).await;
    assert_eq!(first.id, "dialog-1");
    assert_eq!(first.t_us, VirtualTime(101_000));
    assert!(first.source.frame_path.is_empty());
    assert_eq!(first.source.frame_urls.as_slice(), std::slice::from_ref(&first.url));
    tokio::time::sleep(Duration::from_secs(21)).await;
    assert!(!work.is_finished(), "agent wait consumed an execution watchdog");
    assert_eq!(dialogs.pending().unwrap(), Some(first.clone()));
    let r1 = RequestId::new("answer-1").unwrap();
    assert!(dialogs.answer("unknown", &r1, true, None).await.is_err());
    assert_eq!(dialogs.pending().unwrap(), Some(first.clone()));
    let receipt = dialogs.answer(&first.id, &r1, true, None).await.unwrap();
    assert_eq!(receipt.dialog, first);
    let prompt = pending(&dialogs).await;
    assert_eq!(prompt.id, "dialog-2");
    assert_eq!(prompt.default_prompt, "null");
    assert_eq!(dialogs.answer(&first.id, &r1, true, None).await.unwrap(), receipt);
    assert_eq!(dialogs.pending().unwrap(), Some(prompt.clone()));
    let r2 = RequestId::new("answer-2").unwrap();
    let answer = dialogs.answer(&prompt.id, &r2, true, Some("한글 🍎".into())).await.unwrap();
    assert_eq!(answer.prompt_text, "한글 🍎");
    let last = pending(&dialogs).await;
    assert_eq!(last.id, "dialog-3");
    assert_eq!(last.t_us, first.t_us);
    // A caller-chosen id cannot impersonate an automatic answer and bypass its journal acknowledgement.
    let r3 = RequestId::new("native-auto-forged").unwrap();
    dialogs.answer(&last.id, &r3, false, None).await.unwrap();
    let (mut g, result) = tokio::time::timeout(Duration::from_secs(5), work).await.unwrap().unwrap();
    assert_eq!(result.unwrap(), ClockOutcome::Reached);
    assert_eq!(g.now(), VirtualTime(151_000));
    assert_eq!(g.eval_main("answers").await.unwrap(), serde_json::json!([true,true,true,"한글 🍎",false]));
    let trace = BehaviorLog::read(&g.trace_path()).unwrap();
    let recorded: Vec<_> = trace.iter().filter_map(|e| match &e.kind {
        EventKind::Dialog(d) => Some((e.t_us, d)), _ => None,
    }).collect();
    assert_eq!(recorded.len(), 5, "held native dialogs must not also emit a shim record");
    assert_eq!(recorded.iter().map(|(_, d)| d.message.as_str()).collect::<Vec<_>>(), ["automatic", "manual confirm", "automatic after first", "manual prompt", "manual final"]);
    assert!(recorded.iter().all(|(t, _)| *t == VirtualTime(101_000)));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.native_dialog_control().unwrap().inspect().await.unwrap().receipts.len(), 3,
        "trace commit alone discarded an unacknowledged dependent answer");
    assert_eq!(dialogs.saved_answer(&r1).await.unwrap(), Some(receipt));
    for id in [&r1, &r2, &r3] { dialogs.acknowledge(id); }
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert!(g.native_dialog_control().unwrap().inspect().await.unwrap().receipts.is_empty());
}

#[tokio::test]
async fn native_input_and_evaluate_can_wait_for_independent_dialog_answers() {
    let (_root, mut g) = launch("held-dialog-input").await;
    g.set_dialog_rules(serde_json::from_str(r#"[{"kind":"confirm","answer":"hold"}]"#).unwrap()).await.unwrap();
    let dialogs = g.dialog_session().unwrap();
    let input = tokio::spawn(async move {
        let result = g.click(RequestId::new("click").unwrap(), &Locator::Css { selector: "#del".into() }).await;
        (g, result)
    });
    let dialog = pending(&dialogs).await;
    assert_eq!(dialog.message, "Delete?");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!input.is_finished());
    let request = RequestId::new("keep-item").unwrap();
    dialogs.answer(&dialog.id, &request, false, None).await.unwrap();
    dialogs.acknowledge(&request);
    let (mut g, result) = tokio::time::timeout(Duration::from_secs(5), input).await.unwrap().unwrap();
    result.unwrap();
    assert_eq!(g.now(), VirtualTime(1_000));
    assert_eq!(g.eval_main("!!document.getElementById('item')").await.unwrap(), true);
    let evaluation = tokio::spawn(async move {
        let result = g.evaluate("confirm('debug dialog')").await;
        (g, result)
    });
    let dialog = pending(&dialogs).await;
    assert_eq!(dialog.message, "debug dialog");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!evaluation.is_finished(), "evaluate used a wall deadline during the native hold");
    let request = RequestId::new("debug-answer").unwrap();
    dialogs.answer(&dialog.id, &request, true, None).await.unwrap();
    dialogs.acknowledge(&request);
    let (_g, result) = tokio::time::timeout(Duration::from_secs(5), evaluation).await.unwrap().unwrap();
    assert_eq!(result.unwrap(), true);
}

#[tokio::test]
async fn held_recovery_is_serviceable_before_renderer_initialization_and_transfers_one_binding_writer() {
    use frontcap_chromium::generation::ReconnectOptions;
    use frontcap_core::predicate::WatchRegistry;
    let root = tempfile::tempdir().unwrap();
    let detached = support::DetachedProfile::new();
    let addr = support::serve();
    let mut opts = support::options(root.path(), "held-dialog-recovery", format!("http://{addr}/dialogs.html"), RuleSet::default());
    detached.apply(&mut opts);
    let profile = opts.profile.clone();
    let run = opts.run.clone();
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let rules: DialogRules = serde_json::from_str(r#"[{"message":"auto*","answer":"accept"},{"kind":"confirm","answer":"hold"}]"#).unwrap();
    g.set_dialog_rules(rules.clone()).await.unwrap();
    let endpoint = g.endpoint();
    let launch_id = g.launch_id().clone();
    let dialogs = g.dialog_session().unwrap();
    g.eval_main("window.recoveryAnswers=[]; setTimeout(()=>{ recoveryAnswers.push(confirm('auto before')); recoveryAnswers.push(confirm('first')); recoveryAnswers.push(confirm('auto between')); recoveryAnswers.push(confirm('second')); recoveryAnswers.push(confirm('auto after')); },100)").await.unwrap();
    let work = tokio::spawn(async move { g.advance(VirtualDuration::from_ms(150)).await });
    let first = pending(&dialogs).await;
    assert_eq!(first.id, "dialog-1");
    work.abort();
    assert!(work.await.unwrap_err().is_cancelled());
    drop(dialogs);
    let options = || ReconnectOptions {
        repo_root: support::repo_root(), runs_root: root.path().to_path_buf(), run: run.clone(),
        launch_id: launch_id.clone(), endpoint: endpoint.clone(), profile: profile.clone(), watches: WatchRegistry::default(), watchdog: Duration::from_secs(2),
        granted_until: Some(VirtualTime(151_000)), dialog_rules: rules.clone(), upstream: None,
    };
    let recovery = Generation::prepare_dialog_recovery(&options()).await.unwrap().unwrap();
    let service = recovery.dialogs();
    assert_eq!(service.refresh_pending().await.unwrap(), Some(first.clone()));
    let reconnect = tokio::spawn(Generation::reconnect_with_dialog_recovery(options(), recovery));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!reconnect.is_finished(), "renderer initialized through a pending native modal");
    let r1 = RequestId::new("recovered-first").unwrap();
    service.answer(&first.id, &r1, true, None).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(dialog) = service.refresh_pending().await.unwrap() { break dialog }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(second.id, "dialog-2");
    let r2 = RequestId::new("recovered-second").unwrap();
    service.answer(&second.id, &r2, false, None).await.unwrap();
    let (g, _) = tokio::time::timeout(Duration::from_secs(5), reconnect).await.unwrap().unwrap().unwrap();
    assert!(g.now() <= VirtualTime(151_000));
    assert_eq!(g.eval_main("recoveryAnswers").await.unwrap(), serde_json::json!([true,true,true,false,true]));
    let recorded = BehaviorLog::read(&g.trace_path()).unwrap();
    let messages: Vec<_> = recorded.iter().filter_map(|e| match &e.kind { EventKind::Dialog(d) => Some(d.message.as_str()), _ => None }).collect();
    assert_eq!(messages, ["auto before", "first", "auto between", "second", "auto after"]);
    assert_eq!(g.dialog_session().unwrap().saved_answer(&r1).await.unwrap().unwrap().dialog, first);
    let bindings = std::fs::read_to_string(root.path().join("held-dialog-recovery/dialog-bindings.jsonl")).unwrap();
    assert_eq!(bindings.lines().count(), 2, "prepared and resumed paths wrote conflicting bindings");
    // The trace is durable, but neither dependent answer is acknowledged yet. Reconnect again while
    // the engine still has both receipts; retained observer reports must not create duplicate slots.
    drop(g);
    let (mut g, _) = Generation::reconnect(options()).await.unwrap();
    let after = BehaviorLog::read(&g.trace_path()).unwrap();
    let after_messages: Vec<_> = after.iter().filter_map(|e| match &e.kind { EventKind::Dialog(d) => Some(d.message.as_str()), _ => None }).collect();
    assert_eq!(after_messages, messages);
    let service = g.dialog_session().unwrap();
    service.acknowledge(&r1);
    service.acknowledge(&r2);
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert!(g.native_dialog_control().unwrap().inspect().await.unwrap().receipts.is_empty());
    // Exercise a native dialog from an isolated world after replacing the rules. A stale rule cell
    // would still hold this confirm, even though the main-world shim already uses the new policy.
    g.set_dialog_rules(serde_json::from_str(r#"[{"kind":"confirm","answer":"accept"}]"#).unwrap()).await.unwrap();
    use cdp::protocol::{page, target};
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    let session = frontcap_chromium::session::Session::new(client, attached.session_id.0);
    let frame = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id;
    let world = session.call(&page::CreateIsolatedWorldParams {
        world_name: Some("after-held-recovery".into()), ..page::CreateIsolatedWorldParams::new(frame)
    }).await.unwrap().execution_context_id;
    let value = tokio::time::timeout(Duration::from_secs(5), session.eval("confirm('after recovery')", Some(world))).await.unwrap().unwrap();
    assert_eq!(value, true);
}

#[tokio::test]
async fn held_beforeunload_preserves_the_navigation_deadline() {
    let root = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut g = Generation::launch(support::options(root.path(), "held-beforeunload", format!("http://{addr}/leave.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.set_dialog_rules(serde_json::from_str(r#"[{"kind":"beforeunload","answer":"hold"}]"#).unwrap()).await.unwrap();
    // Native user activation is necessary for beforeunload. Its scheduled navigation stays at +10 ms.
    g.click(RequestId::new("activate").unwrap(), &Locator::Css { selector: "#go".into() }).await.unwrap();
    let dialogs = g.dialog_session().unwrap();
    let navigation = tokio::spawn(async move {
        let result = g.navigate_action(&RequestId::new("navigate").unwrap(), &frontcap_core::action::Action::Navigate { url: format!("http://{addr}/nav2.html") }).await;
        (g, result)
    });
    let dialog = pending(&dialogs).await;
    assert_eq!(dialog.kind, frontcap_core::event::DialogKind::Beforeunload);
    assert_eq!(dialog.t_us, VirtualTime(50_000));
    // The real 21-second modal wait is covered above. Advance the Rust host clock here to exercise
    // the separate navigation deadline without spending another 21 seconds in the same suite.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(21)).await;
    tokio::task::yield_now().await;
    assert!(!navigation.is_finished(), "navigation used a wall deadline while the dialog was held");
    tokio::time::resume();
    let request = RequestId::new("leave").unwrap();
    dialogs.answer(&dialog.id, &request, true, None).await.unwrap();
    dialogs.acknowledge(&request);
    let (g, result) = tokio::time::timeout(Duration::from_secs(5), navigation).await.unwrap().unwrap();
    result.unwrap();
    assert_eq!(g.now(), VirtualTime(50_000));
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), "/nav2.html");
}

#[tokio::test]
async fn dialog_source_distinguishes_cross_origin_siblings_and_survives_a_fresh_generation() {
    let root = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let other = support::serve();
    let main_url = format!("http://{addr}/dialogs.html");
    let leaf_url = format!("http://{addr}/dialog-source-frame.html");
    let child_url = format!("http://{other}/dialog-source-frame.html?child={leaf_url}");
    let mut previous: Option<Vec<DialogDescriptor>> = None;
    for run in ["source-record", "source-replay"] {
        let mut options = support::options(root.path(), run, main_url.clone(), RuleSet::default());
        options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://{other}")]);
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(1)).await.unwrap();
        g.set_dialog_rules(serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap()).await.unwrap();
        g.eval_main(&format!("for(let i=0;i<2;i++){{const f=document.createElement('iframe');f.src={};document.body.append(f)}}", serde_json::to_string(&child_url).unwrap())).await.unwrap();
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        let mut found = Vec::new();
        for (index, message, path) in [(0, "ask", vec![0]), (1, "ask", vec![1]), (1, "ask-child", vec![1, 0])] {
            g.eval_main(&format!("setTimeout(()=>frames[{index}].postMessage('{message}','*'),10)")).await.unwrap();
            let dialogs = g.dialog_session().unwrap();
            let mut work = tokio::spawn(async move { let result = g.advance(VirtualDuration::from_ms(20)).await; (g, result) });
            let dialog = tokio::select! {
                dialog = pending(&dialogs) => dialog,
                finished = &mut work => {
                    let (g, result) = finished.unwrap();
                    panic!("advance completed without holding source {path:?}: {result:?}; {:?}", BehaviorLog::read(&g.trace_path()).unwrap());
                }
            };
            assert_eq!(dialog.source.frame_path, path);
            let mut urls = vec![main_url.clone(), child_url.clone()];
            if path.len() == 2 { urls.push(leaf_url.clone()); }
            assert_eq!(dialog.source.frame_urls, urls);
            let request = RequestId::new(format!("answer-{}", found.len())).unwrap();
            let receipt = dialogs.answer(&dialog.id, &request, false, None).await.unwrap();
            assert_eq!(receipt.dialog, dialog, "receipt changed the opening source");
            dialogs.acknowledge(&request);
            let (generation, result) = tokio::time::timeout(Duration::from_secs(5), work).await.unwrap().unwrap();
            result.unwrap(); g = generation;
            found.push(dialog);
        }
        if let Some(before) = &previous {
            for (recorded, replayed) in before.iter().zip(&found) {
                assert_ne!(recorded.frame, replayed.frame, "fixture reused an inspector identity");
                assert_eq!(recorded.source, replayed.source);
            }
        }
        previous = Some(found);
    }
}
