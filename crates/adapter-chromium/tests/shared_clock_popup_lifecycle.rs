//! Popup creation inside a shared-time grant, including its first synchronous load.
mod support;
use cdp::protocol::{emulation, page, runtime, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{
    event::{EventKind, ResponseSource},
    log::BehaviorLog,
    profile::PopupPolicy,
    rules::RuleSet,
    time::VirtualDuration,
};
use serde_json::{Value, json};
use std::time::Duration;

static RUN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy)]
enum Load {
    Fetch,
    Synchronous,
    WorkerFetch,
}

async fn read(client: &cdp::Client, session: &str, expression: &str) -> Value {
    let result = client
        .call(
            Some(session),
            &runtime::EvaluateParams {
                return_by_value: Some(true),
                ..runtime::EvaluateParams::new(expression.into())
            },
        )
        .await
        .unwrap();
    assert!(
        result.exception_details.is_none(),
        "{:?}",
        result.exception_details
    );
    result.result.value.unwrap_or(Value::Null)
}

#[tokio::test]
async fn popup_created_during_progress_joins_before_its_first_timer() {
    check(Load::Fetch, false, true, 100).await;
}

#[tokio::test]
async fn popup_created_during_progress_can_start_with_a_synchronous_request() {
    check(Load::Synchronous, false, true, 100).await;
}

#[tokio::test]
async fn popup_normal_closure_preserves_shared_progress() {
    check(Load::Fetch, true, true, 100).await;
}

#[tokio::test]
async fn closing_a_shared_renderer_popup_preserves_its_live_opener() {
    check(Load::Fetch, true, false, 100).await;
}

#[tokio::test]
async fn isolated_popup_closes_with_a_pending_response() {
    check(Load::Fetch, true, true, 300).await;
}

#[tokio::test]
async fn shared_renderer_popup_closes_with_a_pending_response() {
    check(Load::Fetch, true, false, 300).await;
}

#[tokio::test]
async fn isolated_popup_closes_with_a_pending_worker_response() {
    check(Load::WorkerFetch, true, true, 300).await;
}

#[tokio::test]
async fn shared_renderer_popup_closes_with_a_pending_worker_response() {
    check(Load::WorkerFetch, true, false, 300).await;
}

#[tokio::test]
async fn isolated_popup_closes_while_its_opener_waits_synchronously() {
    check_with_opener_wait(Load::Fetch, true, true, 300, true).await;
}

#[tokio::test]
async fn isolated_popup_worker_load_ends_while_its_opener_waits_synchronously() {
    check_with_opener_wait(Load::WorkerFetch, true, true, 300, true).await;
}

async fn check(load: Load, closing: bool, isolated: bool, latency_ms: u64) {
    check_with_opener_wait(load, closing, isolated, latency_ms, false).await;
}

async fn check_with_opener_wait(
    load: Load,
    closing: bool,
    isolated: bool,
    latency_ms: u64,
    opener_wait: bool,
) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let _run = RUN.lock().await;
    tokio::time::timeout(
        Duration::from_secs(90),
        run(load, closing, isolated, latency_ms, opener_wait),
    )
    .await
    .expect("popup admission watchdog");
}

async fn run(load: Load, closing: bool, isolated: bool, latency_ms: u64, opener_wait: bool) {
    let runs = tempfile::tempdir().unwrap();
    let popup_url = if isolated {
        "https://auth.example.test/"
    } else {
        "https://app.example.test/popup"
    };
    let response_url = if isolated {
        "https://auth.example.test/response"
    } else {
        "https://app.example.test/response"
    };
    let request = match load {
        Load::Synchronous => "const x=new XMLHttpRequest();x.open('GET','/response',false);x.send();samples.push({kind:'response',at:Date.now(),body:x.responseText});".to_owned(),
        Load::Fetch => "fetch('/response').then(r=>r.text()).then(body=>samples.push({kind:'response',at:Date.now(),body}));".to_owned(),
        Load::WorkerFetch => format!("globalThis.worker=new Worker(URL.createObjectURL(new Blob([\"fetch('{response_url}').then(r=>r.text()).then(body=>postMessage(body));\"],{{type:'text/javascript'}})));worker.onmessage=e=>samples.push({{kind:'response',at:Date.now(),body:e.data}});"),
    };
    let close = if closing && !isolated {
        "opener.popupSamples.push(Date.now());window.close();"
    } else if closing {
        "window.close();"
    } else {
        ""
    };
    let opened = if closing && !isolated {
        "opener.popupSamples.push(Date.now());"
    } else {
        ""
    };
    let popup = format!(
        "<!doctype html><script>{opened}globalThis.samples=[{{kind:'first',at:Date.now()}}];setTimeout(()=>{{samples.push({{kind:'timer',at:Date.now()}});{close}}},200);{request}</script>"
    );
    let headers = json!({"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"});
    let rules = RuleSet::from_json(&json!([
        {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":headers,"body":"<!doctype html>root"}},
        {"id":"popup","match":{"url":popup_url},"action":{"type":"mock","status":200,"headers":headers,"body":popup}},
        {"id":"response","match":{"url":response_url},"action":{"type":"mock","status":200,"body":"ready","latency_ms":latency_ms}},
        {"id":"sync-response","match":{"url":"https://app.example.test/sync-response"},"action":{"type":"mock","status":200,"body":"sync-ready","latency_ms":300}},
        {"id":"live-response","match":{"url":"https://app.example.test/live-response"},"action":{"type":"mock","status":200,"body":"live","latency_ms":300}}
    ]).to_string()).unwrap();
    let mut options = support::options(
        runs.path(),
        "popup-lifecycle",
        "https://app.example.test/".into(),
        rules,
    );
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(50))
        .await
        .unwrap();
    generation.test_enable_shared_time().await.unwrap();
    if closing && !isolated {
        generation
            .eval_main("globalThis.popupSamples=[];true")
            .await
            .unwrap();
    }
    generation.eval_main(&format!("globalThis.rootSamples=[];setTimeout(()=>{{rootSamples.push(Date.now());window.open('{popup_url}')}},100);setTimeout(()=>rootSamples.push(Date.now()),200);true")).await.unwrap();
    if latency_ms > 200 {
        generation.eval_main("globalThis.liveResponses=[];setTimeout(()=>fetch('/live-response').then(r=>r.text()).then(body=>liveResponses.push({at:Date.now(),body})),150);true").await.unwrap();
    }
    if opener_wait {
        generation.eval_main("globalThis.syncSamples=[];setTimeout(()=>{syncSamples.push(Date.now());const x=new XMLHttpRequest();x.open('GET','/sync-response',false);x.send();syncSamples.push(Date.now(),x.responseText)},150);true").await.unwrap();
    }
    if closing {
        generation
            .eval_main("setTimeout(()=>rootSamples.push(Date.now()),400);true")
            .await
            .unwrap();
    }
    generation
        .advance(VirtualDuration::from_ms(500))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 550_000);
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let popup = targets
        .target_infos
        .iter()
        .find(|t| t.r#type == "page" && t.url.starts_with(popup_url));
    assert_eq!(popup.is_none(), closing, "popup closure state");
    let mut ids = vec![target::TargetID(endpoint.target_id.clone())];
    if let Some(popup) = popup {
        ids.push(popup.target_id.clone());
    }
    let mut sessions = Vec::new();
    for id in ids {
        sessions.push(
            client
                .call(
                    None,
                    &target::AttachToTargetParams {
                        flatten: Some(true),
                        ..target::AttachToTargetParams::new(id)
                    },
                )
                .await
                .unwrap()
                .session_id
                .0,
        );
    }
    assert_eq!(
        read(&client, &sessions[0], "rootSamples").await,
        if opener_wait {
            json!([epoch + 150, epoch + 500, epoch + 500])
        } else if closing {
            json!([epoch + 150, epoch + 250, epoch + 450])
        } else {
            json!([epoch + 150, epoch + 250])
        }
    );
    if opener_wait {
        assert_eq!(
            read(&client, &sessions[0], "syncSamples").await,
            json!([epoch + 200, epoch + 500, "sync-ready"])
        );
    }
    if latency_ms > 200 {
        assert_eq!(
            read(&client, &sessions[0], "liveResponses").await,
            json!([{"at":epoch+500,"body":"live"}])
        );
    }
    if !closing {
        assert_eq!(
            read(&client, &sessions[1], "samples").await,
            json!([
                {"kind":"first","at":epoch+150},
                {"kind":"response","at":epoch+250,"body":"ready"},
                {"kind":"timer","at":epoch+350}
            ])
        );
        let root = client
            .call(
                Some(&sessions[0]),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        let popup = client
            .call(
                Some(&sessions[1]),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        assert_ne!(root.renderer_id, popup.renderer_id);
    }
    let end = if closing {
        if !isolated {
            assert_eq!(
                read(&client, &sessions[0], "popupSamples").await,
                json!([epoch + 150, epoch + 350])
            );
        }
        let before = client
            .call(
                Some(&sessions[0]),
                &page::FrontcapGetSharedClockParams::default(),
            )
            .await
            .unwrap();
        assert!(!before.failed);
        assert!(
            client
                .call(
                    Some(&sessions[0]),
                    &emulation::FrontcapGetWorkersParams::default()
                )
                .await
                .unwrap()
                .workers
                .is_empty(),
            "closed popup workers must leave native membership"
        );
        let retired: Vec<_> = before.participants.iter().filter(|p| p.retired).collect();
        assert_eq!(retired.len(), usize::from(isolated));
        assert!(
            retired
                .iter()
                .all(|p| !p.armed && p.proposed_time_us.is_none())
        );
        generation.eval_main(&format!("setTimeout(()=>window.open('{popup_url}'),100);setTimeout(()=>rootSamples.push(Date.now()),400);true")).await.unwrap();
        generation
            .advance(VirtualDuration::from_ms(500))
            .await
            .unwrap();
        assert_eq!(
            read(&client, &sessions[0], "rootSamples").await,
            if opener_wait {
                json!([epoch + 150, epoch + 500, epoch + 500, epoch + 950])
            } else {
                json!([epoch + 150, epoch + 250, epoch + 450, epoch + 950])
            }
        );
        if !isolated {
            assert_eq!(
                read(&client, &sessions[0], "popupSamples").await,
                json!([epoch + 150, epoch + 350, epoch + 650, epoch + 850])
            );
        }
        let after = client
            .call(
                Some(&sessions[0]),
                &page::FrontcapGetSharedClockParams::default(),
            )
            .await
            .unwrap();
        assert!(!after.failed);
        assert!(
            client
                .call(
                    Some(&sessions[0]),
                    &emulation::FrontcapGetWorkersParams::default()
                )
                .await
                .unwrap()
                .workers
                .is_empty(),
            "repeated closure must not retain worker votes"
        );
        assert_eq!(after.participants.iter().filter(|p| !p.retired).count(), 1);
        assert_eq!(
            after.participants.iter().filter(|p| p.retired).count(),
            if isolated { 2 } else { 0 }
        );
        for old in retired {
            let retained = after
                .participants
                .iter()
                .find(|p| p.participant_id == old.participant_id)
                .unwrap();
            assert!(retained.retired && !retained.armed && retained.proposed_time_us.is_none());
        }
        assert!(
            client
                .call(None, &target::GetTargetsParams::default())
                .await
                .unwrap()
                .target_infos
                .iter()
                .all(|t| !t.url.starts_with(popup_url))
        );
        1_050_125
    } else {
        550_125
    };
    generation
        .advance(VirtualDuration::from_micros(125))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    for session in sessions {
        assert_eq!(
            client
                .call(Some(&session), &emulation::GetVirtualTimeParams::default())
                .await
                .unwrap()
                .unix_time_us,
            (i128::from(epoch) * 1000 + end).to_string()
        );
    }
    let trace = generation.trace_path();
    generation.stop().await.unwrap();
    if latency_ms > 200 {
        let events = BehaviorLog::read(&trace).unwrap();
        if opener_wait {
            let closed: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::Context(context)
                        if context.event == frontcap_core::event::ContextEvent::Closed =>
                    {
                        Some(event.t_us.0)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                closed,
                vec![350_000, 850_000],
                "closure must be recorded before the opener response"
            );
        }
        let requests: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Request(request) if request.url == response_url => {
                    Some(request.id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            requests.len(),
            2,
            "both closing popups must issue their load"
        );
        for id in requests {
            let responses: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::Response(response) if response.id == id => Some(response),
                    _ => None,
                })
                .collect();
            assert_eq!(
                responses.len(),
                1,
                "a closed load must resolve exactly once"
            );
            assert_eq!(responses[0].status, 0);
            assert_eq!(responses[0].source, ResponseSource::Fail);
        }
    }
}

#[tokio::test]
async fn closing_a_window_before_admission_does_not_hold_shared_time() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let _run = RUN.lock().await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let runs = tempfile::tempdir().unwrap();
        let rules = RuleSet::from_json(&json!([
            {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>root"}}
        ]).to_string()).unwrap();
        let mut options = support::options(runs.path(), "closed-before-admission", "https://app.example.test/".into(), rules);
        options.profile.popups = PopupPolicy::Controlled;
        options.passthrough_origins = Some(vec![]);
        let mut generation = Generation::launch(options).await.unwrap();
        generation.advance(VirtualDuration::from_ms(50)).await.unwrap();
        generation.test_enable_shared_time().await.unwrap();
        assert_eq!(generation.eval_main("const pendingWindow=window.open('about:blank');if(pendingWindow)pendingWindow.close();pendingWindow!==null").await.unwrap(), json!(true));
        generation.advance(VirtualDuration::from_ms(100)).await.unwrap();
        assert_eq!(generation.now().0, 150_000);
        let endpoint = generation.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
        let targets = client.call(None, &target::GetTargetsParams::default()).await.unwrap();
        assert_eq!(targets.target_infos.iter().filter(|t|t.r#type=="page").count(),1);
        let root = client.call(None, &target::AttachToTargetParams {
            flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
        }).await.unwrap().session_id.0;
        let state = client.call(Some(&root), &page::FrontcapGetSharedClockParams::default()).await.unwrap();
        assert!(!state.failed);
        assert_eq!(state.participants.iter().filter(|p|!p.retired).count(), 1);
        generation.stop().await.unwrap();
    }).await.expect("unadmitted window closure watchdog");
}

#[tokio::test]
async fn a_closing_popup_can_finish_its_current_synchronous_request() {
    close_then_request(true, false).await;
}

#[tokio::test]
async fn local_clock_closing_popup_can_finish_its_current_synchronous_request() {
    close_then_request(false, false).await;
}

#[tokio::test]
async fn an_enrolled_closing_popup_can_finish_its_current_synchronous_request() {
    close_then_request(true, true).await;
}

#[tokio::test]
async fn local_clock_enrolled_closing_popup_can_finish_its_current_synchronous_request() {
    close_then_request(false, true).await;
}

async fn close_then_request(shared: bool, precreated: bool) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let _run = RUN.lock().await;
    tokio::time::timeout(Duration::from_secs(45), async {
        let runs = tempfile::tempdir().unwrap();
        let action = "opener.samples.push({kind:'close',at:Date.now()});window.close();try{const x=new XMLHttpRequest();x.open('GET','/closing-response',false);x.send();opener.samples.push({kind:'response',at:Date.now(),body:x.responseText});}catch(e){opener.samples.push({kind:'error',at:Date.now(),name:e.name});}";
        let popup = if precreated { format!("<!doctype html><script>setTimeout(()=>{{{action}}},100)</script>") } else { format!("<!doctype html><script>{action}</script>") };
        let rules = RuleSet::from_json(&json!([
            {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>root"}},
            {"id":"popup","match":{"url":"https://app.example.test/closing"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":popup}},
            {"id":"response","match":{"url":"https://app.example.test/closing-response"},"action":{"type":"mock","status":200,"body":"ready","latency_ms":300}}
        ]).to_string()).unwrap();
        let mut options = support::options(runs.path(), "close-then-request", "https://app.example.test/".into(), rules);
        options.profile.popups = PopupPolicy::Controlled;
        options.passthrough_origins = Some(vec![]);
        let epoch = options.profile.initial_date_ms;
        // Ordinary launches activate shared time. Keep the local-clock controls
        // on the explicit pre-activation hook so they test a distinct mechanism.
        let mut generation = if shared {
            Generation::launch(options).await
        } else {
            Generation::test_launch_before_shared_activation(options).await
        }.unwrap();
        generation.advance(VirtualDuration::from_ms(50)).await.unwrap();
        generation.eval_main("globalThis.samples=[];true").await.unwrap();
        if precreated {
            generation.eval_main("window.open('/closing');true").await.unwrap();
            generation.advance(VirtualDuration::from_micros(0)).await.unwrap();
        }
        if !precreated { generation.eval_main("setTimeout(()=>window.open('/closing'),100);true").await.unwrap(); }
        generation.eval_main("setTimeout(()=>samples.push({kind:'timer',at:Date.now()}),200);true").await.unwrap();
        generation.advance(VirtualDuration::from_ms(500)).await.unwrap();
        let samples = generation.eval_main("samples").await.unwrap();
        assert_eq!(samples,json!([
            {"kind":"close","at":epoch+150},
            {"kind":"response","at":epoch+450,"body":"ready"},
            {"kind":"timer","at":epoch+450}
        ]));
        let endpoint = generation.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
        let targets = client.call(None,&target::GetTargetsParams::default()).await.unwrap();
        assert_eq!(targets.target_infos.iter().filter(|t|t.r#type=="page").count(),1);
        assert_eq!(generation.now().0,550_000);
        generation.stop().await.unwrap();
    }).await.expect("close followed by synchronous request watchdog");
}
