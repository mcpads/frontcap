//! Shared native time must survive replacement of its adapter controller.
mod support;

use frontcap_chromium::generation::{Generation, ReconnectOptions};
use frontcap_core::{
    ids::RunId,
    predicate::WatchRegistry,
    profile::PopupPolicy,
    rules::RuleSet,
    time::{VirtualDuration, VirtualTime},
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn stopped_shared_time_resumes_after_controller_replacement() {
    replace_controller(Recovery::Stopped).await;
}

#[tokio::test]
async fn granted_shared_time_resumes_after_controller_replacement() {
    replace_controller(Recovery::Granted).await;
}

#[tokio::test]
async fn blocked_shared_time_recovers_after_controller_replacement() {
    replace_controller(Recovery::Held).await;
}

#[tokio::test]
async fn selected_shared_response_recovers_after_controller_replacement() {
    replace_controller(Recovery::Selected).await;
}

#[tokio::test]
async fn overlapping_shared_responses_recover_after_controller_replacement() {
    replace_controller(Recovery::Overlapping).await;
}

#[tokio::test]
async fn later_overlapping_shared_response_recovers_after_controller_replacement() {
    replace_controller(Recovery::LaterOverlapping).await;
}

#[tokio::test]
async fn worker_fetch_recovers_during_a_shared_response() {
    replace_controller(Recovery::WorkerFetch).await;
}

#[tokio::test]
async fn worker_synchronous_request_recovers_during_a_shared_response() {
    replace_controller(Recovery::WorkerSynchronous).await;
}

#[tokio::test]
async fn shared_response_recovers_after_loss_inside_recovery() {
    replace_controller(Recovery::InterruptedService).await;
}

#[tokio::test]
async fn consecutive_shared_responses_recover_in_the_same_task() {
    replace_controller(Recovery::Consecutive).await;
}

#[tokio::test]
async fn consecutive_shared_response_extends_the_recovery_boundary() {
    replace_controller(Recovery::ConsecutiveBoundary).await;
}

#[tokio::test]
async fn synchronous_hold_during_shared_recovery_can_be_cleaned_up() {
    replace_controller(Recovery::HeldDuringResponse).await;
}

#[tokio::test]
async fn synchronous_hold_survives_another_loss_during_shared_recovery() {
    replace_controller(Recovery::LostHeldResponse).await;
}

#[tokio::test]
async fn ordinary_hold_survives_shared_response_recovery() {
    replace_controller(Recovery::OrdinaryHeld).await;
}

#[tokio::test]
async fn worker_hold_survives_shared_response_recovery() {
    replace_controller(Recovery::WorkerHeld).await;
}

#[tokio::test]
async fn synchronous_worker_hold_survives_shared_response_recovery() {
    replace_controller(Recovery::WorkerSynchronousHeld).await;
}

#[tokio::test]
async fn other_windows_progress_while_a_recovered_synchronous_worker_is_held() {
    replace_controller(Recovery::WorkerHeldProgress).await;
}

#[derive(Clone, Copy)]
enum Recovery {
    Stopped,
    Granted,
    Held,
    Selected,
    Overlapping,
    LaterOverlapping,
    WorkerFetch,
    WorkerSynchronous,
    InterruptedService,
    Consecutive,
    ConsecutiveBoundary,
    HeldDuringResponse,
    LostHeldResponse,
    OrdinaryHeld,
    WorkerHeld,
    WorkerSynchronousHeld,
    WorkerHeldProgress,
}

async fn replace_controller(mode: Recovery) {
    let grant = matches!(mode, Recovery::Granted);
    let blocking = matches!(mode, Recovery::Held);
    let selected = matches!(
        mode,
        Recovery::Selected
            | Recovery::Overlapping
            | Recovery::LaterOverlapping
            | Recovery::WorkerFetch
            | Recovery::WorkerSynchronous
            | Recovery::InterruptedService
            | Recovery::Consecutive
            | Recovery::ConsecutiveBoundary
            | Recovery::HeldDuringResponse
            | Recovery::LostHeldResponse
            | Recovery::OrdinaryHeld
            | Recovery::WorkerHeld
            | Recovery::WorkerSynchronousHeld
            | Recovery::WorkerHeldProgress
    );

    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    tokio::time::timeout(Duration::from_secs(45), async {
        let runs = tempfile::tempdir().unwrap();
        let detached = support::DetachedProfile::new();
        let rules = RuleSet::from_json(&json!([
            {"id":"worker","match":{"url":"https://auth.example.test/worker.js"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":if matches!(mode, Recovery::WorkerSynchronous | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {"onmessage=()=>setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','/early',false);x.send();postMessage({body:x.responseText,at:Date.now()});},25);postMessage('ready');"} else {"onmessage=()=>setTimeout(()=>fetch('/early').then(r=>r.text()).then(body=>postMessage({body,at:Date.now()})),25);postMessage('ready');"}}},
            {"id":"second","match":{"url":"https://app.example.test/second"},"action":{"type":"mock","status":200,"body":"second","latency_ms":40}},
            {"id":"middle","match":{"url":"https://app.example.test/middle"},"action":{"type":"mock","status":200,"body":"middle","latency_ms":5}},
            {"id":"early","match":{"url":"https://auth.example.test/early"},"action":if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse | Recovery::OrdinaryHeld | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {json!({"type":"hold"})} else {json!({"type":"mock","status":200,"body":"early","latency_ms":if matches!(mode, Recovery::LaterOverlapping | Recovery::Consecutive) {50} else {10}})}},
            {"id":"selected","match":{"url":"https://app.example.test/selected"},"action":{"type":"mock","status":200,"body":"ready","latency_ms":50}},
            {"id":"hold","match":{"url":"https://app.example.test/held"},"action":{"type":"hold"}},
            {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html>root"}},
            {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>popup"}}
        ]).to_string()).unwrap();
        let mut options = support::options(runs.path(), "shared-recovery", "https://app.example.test/".into(), rules);
        options.profile.popups = PopupPolicy::Controlled;
        options.passthrough_origins = Some(vec![]);
        detached.apply(&mut options);
        let profile = options.profile.clone();
        let epoch = profile.initial_date_ms;
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        g.test_enable_shared_time().await.unwrap();
        g.eval_main("window.open('https://auth.example.test/');true").await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        let popup = g.contexts().await.unwrap().into_iter()
            .find(|c| c.open && c.url == "https://auth.example.test/").unwrap().id;
        if matches!(mode, Recovery::WorkerFetch | Recovery::WorkerSynchronous | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {
            g.enter_context(&popup).unwrap();
            g.eval_main("globalThis.early=[];globalThis.workerReady=false;globalThis.w=new Worker('/worker.js');w.onmessage=e=>{if(e.data==='ready')workerReady=true;else early.push(e.data);};true").await.unwrap();
            g.leave_context();
            g.advance(VirtualDuration::from_micros(0)).await.unwrap();
            g.enter_context(&popup).unwrap();
            assert_eq!(g.eval_main("workerReady").await.unwrap(), json!(true));
            g.leave_context();
        }
        let launch_id = g.launch_id().clone();
        for round in 0..2 {
            for context in ["page", popup.as_str()] {
                g.enter_context(context).unwrap();
                g.eval_main("globalThis.samples??=[];setTimeout(()=>samples.push(Date.now()),100);true").await.unwrap();
                g.leave_context();
            }
            if matches!(mode, Recovery::LaterOverlapping | Recovery::WorkerHeldProgress) {
                g.eval_main("globalThis.middle??=[];setTimeout(()=>fetch('/middle').then(r=>r.text()).then(body=>middle.push({body,at:Date.now()})),60);true").await.unwrap();
            }
            if selected {
                g.enter_context(&popup).unwrap();
                let early = if matches!(mode, Recovery::WorkerFetch | Recovery::WorkerSynchronous | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {
                    "w.postMessage('go');true"
                } else if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse) {
                    "globalThis.early??=[];setTimeout(()=>{try{const x=new XMLHttpRequest();x.open('GET','/early',false);x.send();early.push({body:x.responseText,at:Date.now()});}catch(e){early.push({body:e.name,at:Date.now()});}},25);true"
                } else if matches!(mode, Recovery::Overlapping | Recovery::LaterOverlapping | Recovery::Consecutive) {
                    "globalThis.early??=[];setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','/early',false);x.send();early.push({body:x.responseText,at:Date.now()});},25);true"
                } else {
                    "globalThis.early??=[];setTimeout(()=>fetch('/early').then(r=>r.text()).then(body=>early.push({body,at:Date.now()})),25);true"
                };
                g.eval_main(early).await.unwrap();
                g.leave_context();
                g.test_interrupt_synchronous_binding("https://app.example.test/selected".into());
                let script = if matches!(mode, Recovery::Consecutive | Recovery::ConsecutiveBoundary) {
                    "globalThis.selectedResult=null;{const x=new XMLHttpRequest();x.open('GET','/selected',false);x.send();selectedResult={body:x.responseText,at:Date.now()};const y=new XMLHttpRequest();y.open('GET','/second',false);y.send();globalThis.secondResult={body:y.responseText,at:Date.now()};}true"
                } else {
                    "globalThis.selectedResult=null;{const x=new XMLHttpRequest();x.open('GET','/selected',false);x.send();selectedResult={body:x.responseText,at:Date.now()};}true"
                };
                let error = g.evaluate(script).await.unwrap_err();
                assert!(error.to_string().contains("test interrupted synchronous binding"), "round {round}: {error:?}");
            }
            if blocking {
                let holds = g.blocking_holds();
                let evaluation = g.evaluate("globalThis.blockedResult='waiting';try{const x=new XMLHttpRequest();x.open('GET','/held',false);x.send();blockedResult='answered';}catch(e){blockedResult=e.name;}true");
                tokio::pin!(evaluation);
                tokio::select! {
                    result = &mut evaluation => panic!("held request completed before loss: {result:?}"),
                    held = holds.wait_pending() => assert!(held.url.ends_with("/held")),
                }
            }
            let end = VirtualTime(300_000 + round * 200_000);
            let endpoint = if grant {
                g.advance_and_abandon(VirtualDuration::from_ms(200)).await.unwrap()
            } else {
                let endpoint = g.endpoint();
                g.abandon();
                endpoint
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            let reconnect = ReconnectOptions {
                repo_root: support::repo_root(), runs_root: runs.path().into(), run: RunId::new("shared-recovery").unwrap(),
                launch_id: launch_id.clone(), profile: profile.clone(), endpoint, watches: WatchRegistry::default(), watchdog: Duration::from_secs(10),
                granted_until: grant.then_some(end), dialog_rules: Default::default(), upstream: None,
            };
            (g, _) = if blocking || selected {
                let mut recovery = Generation::prepare_dialog_recovery(&reconnect).await.unwrap().unwrap();
                if matches!(mode, Recovery::InterruptedService) {
                    recovery.test_interrupt_response_recovery_at(VirtualTime(135_000+round*200_000));
                    let error = recovery.recover_blocking_request(true).await.unwrap_err();
                    assert!(error.to_string().contains("test interrupted shared response recovery"), "{error:?}");
                    recovery.test_abandon();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    recovery = Generation::prepare_dialog_recovery(&reconnect).await.unwrap().unwrap();
                }
                let held = recovery.recover_blocking_request(selected).await.unwrap();
                if blocking {
                    recovery.abort_blocking_hold(&held.expect("retained synchronous hold").ordinal).await.unwrap();
                } else if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse) {
                    let mut held = held.expect("shared recovery exposes the synchronous hold");
                    if matches!(mode, Recovery::LostHeldResponse) {
                        let ordinal = held.ordinal.clone();
                        recovery.test_abandon();
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        recovery = Generation::prepare_dialog_recovery(&reconnect).await.unwrap().unwrap();
                        held = recovery.recover_blocking_request(true).await.unwrap().expect("retained shared hold");
                        assert_eq!(held.ordinal, ordinal);
                    }
                    assert!(held.url.ends_with("/early"));
                    assert_eq!(held.t_us, VirtualTime(125_000+round*200_000));
                    recovery.preserve_blocking_answer_time(held.ordinal.clone(), held.t_us);
                    recovery.abort_blocking_hold(&held.ordinal).await.unwrap();
                    assert!(recovery.next_blocking_hold(&held.ordinal).await.unwrap().is_none());
                } else { assert!(held.is_none()); }
                Generation::reconnect_with_dialog_recovery(reconnect, recovery).await.unwrap()
            } else {
                Generation::reconnect(reconnect).await.unwrap()
            };
            if matches!(mode, Recovery::OrdinaryHeld | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {
                if matches!(mode, Recovery::WorkerSynchronousHeld) && round == 0 {
                    tokio::time::sleep(Duration::from_secs(21)).await;
                }
                let held = g.held();
                if matches!(mode, Recovery::WorkerHeldProgress) {
                    g.advance(VirtualDuration::from_ms(60)).await.unwrap();
                    assert_eq!(g.now(), VirtualTime(210_000+round*200_000));
                    assert_eq!(g.held(), held);
                    let timers: Vec<_> = (0..=round).map(|i| epoch+200+i*200).collect();
                    for context in ["page", popup.as_str()] {
                        g.enter_context(context).unwrap();
                        assert_eq!(g.eval_main("samples").await.unwrap(), json!(timers));
                        g.leave_context();
                    }
                }
                assert_eq!(held.len(), 1, "ordinary hold remains available after recovery");
                g.enter_context(&popup).unwrap();
                assert_eq!(g.eval_main("early.length").await.unwrap(), json!(round));
                g.leave_context();
                g.respond(&held[0], frontcap_core::rules::MockSpec {
                    status: 200, headers: Default::default(), body: "early".into(), body_base64: None, latency_ms: 0,
                }).await.unwrap();
                g.advance(VirtualDuration::from_micros(0)).await.unwrap();
                assert!(g.held().is_empty());
            }
            if selected {
                assert_eq!(g.eval_main("selectedResult").await.unwrap(), json!({"body":"ready","at":epoch+150+round*200}));
                g.enter_context(&popup).unwrap();
                let early: Vec<_> = (0..=round).map(|i| json!({"body":if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse) {"NetworkError"} else {"early"},"at":epoch+if matches!(mode, Recovery::WorkerHeldProgress) {210} else if matches!(mode, Recovery::OrdinaryHeld | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {150} else if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse) {125} else if matches!(mode, Recovery::LaterOverlapping | Recovery::Consecutive) {175} else {135}+i*200})).collect();
                assert_eq!(g.eval_main("early").await.unwrap(), json!(early));
                g.leave_context();
            }
            if matches!(mode, Recovery::LaterOverlapping | Recovery::WorkerHeldProgress) {
                let middle: Vec<_> = (0..=round).map(|i| json!({"body":"middle","at":epoch+165+i*200})).collect();
                assert_eq!(g.eval_main("middle").await.unwrap(), json!(middle));
            }
            if matches!(mode, Recovery::Consecutive | Recovery::ConsecutiveBoundary) {
                assert_eq!(g.eval_main("secondResult").await.unwrap(), json!({"body":"second","at":epoch+190+round*200}));
            }
            if blocking {
                assert_eq!(g.eval_main("blockedResult").await.unwrap(), json!("NetworkError"));
            }
            assert!(g.now() <= end, "recovery must not exceed the retained grant");
            g.advance(VirtualDuration::from_micros(end.0 - g.now().0)).await.unwrap();
            let expected: Vec<_> = (0..=round).map(|i| epoch + 200 + i * 200).collect();
            for context in ["page", popup.as_str()] {
                g.enter_context(context).unwrap();
                assert_eq!(g.eval_main("samples").await.unwrap(), json!(expected));
                g.leave_context();
            }
            assert_eq!(g.now().0, 300_000 + round * 200_000);
        }
        if selected {
            use frontcap_core::{event::EventKind, log::BehaviorLog};
            let events = BehaviorLog::read(&g.trace_path()).unwrap();
            let mut expected_requests = vec![("/selected", 100_000, 150_000), ("/early", 125_000, if matches!(mode, Recovery::WorkerHeldProgress) {210_000} else if matches!(mode, Recovery::OrdinaryHeld | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {150_000} else if matches!(mode, Recovery::HeldDuringResponse | Recovery::LostHeldResponse) {125_000} else if matches!(mode, Recovery::LaterOverlapping | Recovery::Consecutive) {175_000} else {135_000})];
            if matches!(mode, Recovery::Consecutive | Recovery::ConsecutiveBoundary) { expected_requests.push(("/second", 150_000, 190_000)); }
            for (suffix, issued, answered) in expected_requests {
                let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                    EventKind::Request(r) if r.url.ends_with(suffix) => Some((r.id.clone(), e.t_us.0)),
                    _ => None,
                }).collect();
                assert_eq!(requests.iter().map(|(_, at)| *at).collect::<Vec<_>>(), vec![issued, issued+200_000], "{suffix} issue records");
                for (round, (id, _)) in requests.iter().enumerate() {
                    let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                        EventKind::Response(r) if &r.id == id => Some(e.t_us.0), _ => None,
                    }).collect();
                    assert_eq!(responses, vec![answered + round as u64 * 200_000], "{suffix} exactly one response at its deadline");
                }
            }
        }
        if matches!(mode, Recovery::WorkerFetch | Recovery::WorkerSynchronous | Recovery::WorkerHeld | Recovery::WorkerSynchronousHeld | Recovery::WorkerHeldProgress) {
            use frontcap_chromium::bindings::{BindingEntry, BindingJournal, DocumentPlace};
            let entries = BindingJournal::read(&runs.path().join("shared-recovery/interceptions.jsonl")).unwrap();
            let worker_bindings: Vec<_> = entries.iter().filter_map(|e| match e {
                BindingEntry::Bound(b) if b.url.ends_with("/early") => Some(b.as_ref()), _ => None,
            }).collect();
            assert_eq!(worker_bindings.len(), 2);
            for binding in worker_bindings {
                let lineage = binding.worker_lineage.as_ref().expect("durable worker lineage");
                assert_eq!(lineage.constructors, vec![1]);
                assert!(entries.iter().any(|e| matches!(e, BindingEntry::WorkerRequestOwner {
                    network_id, lineage: frozen, issued_us, document: Some(DocumentPlace::Current { window: 1, frames }),
                } if Some(network_id) == binding.network_id.as_ref() && frozen == lineage && *issued_us == binding.issued_us && frames.is_empty())));
            }
        }
        g.stop().await.unwrap();
    }).await.expect("shared controller replacement watchdog");
}
