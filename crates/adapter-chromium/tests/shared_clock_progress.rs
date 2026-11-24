//! Opt-in native shared-time integration. This is not complete generation admission/recovery.
mod support;
use cdp::protocol::{emulation, page, runtime, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{
    ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration,
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Suspension {
    None,
    Explicit,
    ReplaceBound,
    ControllerLoss,
    ControllerDisable,
    ProcessControl,
}

async fn attach(client: &cdp::Client, id: target::TargetID) -> String {
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
        .0
}
async fn eval(client: &cdp::Client, session: &str, expression: String) -> Value {
    let result = client
        .call(
            Some(session),
            &runtime::EvaluateParams {
                return_by_value: Some(true),
                ..runtime::EvaluateParams::new(expression)
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
async fn grant(client: &cdp::Client, session: &str, owner: bool) {
    client
        .call(
            Some(session),
            &emulation::SetVirtualTimePolicyParams {
                bind_storage_clock: Some(owner),
                budget: Some(2000.0),
                ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn shared_time_preserves_callback_expiry_with_owner_granted_first() {
    check(false, false, false, false, Suspension::None).await;
}
#[tokio::test]
async fn shared_time_preserves_callback_expiry_with_popup_granted_first() {
    check(true, false, false, false, Suspension::None).await;
}
#[tokio::test]
async fn shared_time_includes_a_worker_atomic_wait_and_its_message_delivery() {
    check(false, true, false, false, Suspension::None).await;
}
#[tokio::test]
async fn shared_time_visits_popup_callbacks_while_the_owner_waits_for_a_synchronous_arrival() {
    check(true, false, true, false, Suspension::None).await;
}

#[tokio::test]
async fn native_loss_during_a_shared_grant_fails_remaining_participants() {
    check(false, false, false, true, Suspension::None).await;
}

#[tokio::test]
async fn a_shared_clock_hold_survives_inspector_detach_without_renewing_its_bound() {
    check(false, false, false, false, Suspension::Explicit).await;
}

#[tokio::test]
async fn resuming_with_a_new_bound_cannot_publish_the_previous_endpoint() {
    check(false, false, false, false, Suspension::ReplaceBound).await;
}

#[tokio::test]
async fn controller_loss_and_stale_controller_teardown_preserve_shared_progress() {
    check(false, false, false, false, Suspension::ControllerLoss).await;
}

#[tokio::test]
async fn disabling_the_controller_suspends_shared_time_without_renewing_the_bound() {
    check(false, false, false, false, Suspension::ControllerDisable).await;
}

#[tokio::test]
async fn independent_process_control_rejects_stale_and_foreign_identities() {
    check(false, false, false, false, Suspension::ProcessControl).await;
}

async fn check(
    popup_first: bool,
    worker: bool,
    synchronous: bool,
    crash: bool,
    suspension: Suspension,
) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    tokio::time::timeout(
        Duration::from_secs(45),
        run(popup_first, worker, synchronous, crash, suspension),
    )
    .await
    .expect("shared native time check timed out");
}
async fn run(
    popup_first: bool,
    worker: bool,
    synchronous: bool,
    crash: bool,
    suspension: Suspension,
) {
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", runs.display());
    let worker_script = r#"globalThis.workerSamples=[];globalThis.workerReady=false;
      globalThis.worker=new Worker(URL.createObjectURL(new Blob([
        `onmessage=e=>{if(e.data==='ready'){postMessage({ready:true});return;}const a=new Int32Array(e.data);const at=Date.now();const result=Atomics.wait(a,0,0,300);postMessage({at,result,after:Date.now()});}`
      ],{type:'application/javascript'})));
      worker.onmessage=e=>{if(e.data.ready)workerReady=true;else workerSamples.push({data:e.data,received:Date.now(),cookie:document.cookie})};worker.postMessage('ready');"#;
    let body = "<!doctype html><button id=open onclick=\"window.open('https://auth.example.test/')\">open</button><script></script>";
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":body}},
        {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html>popup"}},
        {"id":"sync","match":{"url":"https://app.example.test/sync"},"action":{"type":"mock","status":200,"body":"done","latency_ms":1000}}
    ]).to_string()).unwrap();
    let mut options = support::options(
        &runs,
        "shared-time",
        "https://app.example.test/".into(),
        rules,
    );
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::test_launch_before_shared_activation(options).await.unwrap();
    if worker {
        generation
            .advance(VirtualDuration::from_ms(0))
            .await
            .unwrap();
        generation
            .eval_main(&format!("{worker_script};true"))
            .await
            .unwrap();
    }
    let advanced = generation.advance(VirtualDuration::from_ms(50)).await;
    if let Err(error) = &advanced {
        let endpoint = generation.endpoint();
        let diagnostic = cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap();
        let root = attach(&diagnostic, target::TargetID(endpoint.target_id)).await;
        let targets = diagnostic
            .call(None, &target::GetTargetsParams::default())
            .await
            .unwrap();
        eprintln!("startup error: {error:?}; targets: {targets:?}");
        eprintln!(
            "native workers: {:?}",
            tokio::time::timeout(
                Duration::from_secs(2),
                diagnostic.call(Some(&root), &emulation::FrontcapGetWorkersParams::default())
            )
            .await
        );
        eprintln!(
            "root ready: {:?}",
            tokio::time::timeout(
                Duration::from_secs(2),
                diagnostic.call(
                    Some(&root),
                    &runtime::EvaluateParams {
                        return_by_value: Some(true),
                        ..runtime::EvaluateParams::new(
                            "({ready:globalThis.workerReady,isolated:crossOriginIsolated})".into()
                        )
                    }
                )
            )
            .await
        );
        for info in targets.target_infos.iter().filter(|t| t.r#type == "worker") {
            let worker = attach(&diagnostic, info.target_id.clone()).await;
            eprintln!(
                "worker clock: {:?}",
                tokio::time::timeout(
                    Duration::from_secs(2),
                    diagnostic.call(Some(&worker), &emulation::FrontcapGetClockParams::default())
                )
                .await
            );
        }
        if let Ok(trace) = std::fs::read_to_string(generation.trace_path()) {
            eprintln!(
                "startup trace: {}",
                trace
                    .lines()
                    .rev()
                    .take(12)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    advanced.unwrap();
    generation
        .evaluate("document.cookie='shared=live; Domain=example.test; Path=/; Max-Age=1'")
        .await
        .unwrap();
    generation
        .click(
            RequestId::new("open").unwrap(),
            &Locator::Css {
                selector: "#open".into(),
            },
        )
        .await
        .unwrap();
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let root = attach(&client, target::TargetID(endpoint.target_id.clone())).await;
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let popup = attach(
        &client,
        targets
            .target_infos
            .iter()
            .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
            .unwrap()
            .target_id
            .clone(),
    )
    .await;
    assert_ne!(
        client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .renderer_id,
        client
            .call(Some(&popup), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .renderer_id
    );
    let prepare = emulation::FrontcapPrepareSharedClockParams::default();
    let owner = client.call(Some(&root), &prepare).await.unwrap();
    let participant = client.call(Some(&popup), &prepare).await.unwrap();
    assert!(!client.call(Some(&root), &page::FrontcapGetSharedClockParams::default()).await.unwrap().activated,
        "this native fixture must own the activation transition");
    assert_eq!(owner.clock_id, participant.clock_id);
    let activate = page::FrontcapActivateSharedClockParams::new(owner.clock_id.clone());
    assert!(
        client.call(Some(&root), &activate).await.is_err(),
        "unarmed participants must refuse activation"
    );
    for session in [&root, &popup] {
        client
            .call(
                Some(session),
                &emulation::FrontcapArmSharedClockParams::default(),
            )
            .await
            .unwrap();
    }
    client.call(Some(&root), &activate).await.unwrap();
    let state = client
        .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    assert!(
        state.activated
            && !state.failed
            && state.participants.iter().all(|p| p.armed && p.confirmed)
    );
    eprintln!(
        "shared time activated: worker={worker}, synchronous={synchronous}, popup_first={popup_first}"
    );
    if synchronous {
        // Native sessions own activation above. The response service must also
        // enroll its generation driver before it issues frames in that authority.
        generation.test_enable_shared_time().await.unwrap();
    }
    if suspension == Suspension::ProcessControl {
        check_process_control(&client, &root, &popup, &owner.clock_id).await;
    } else if suspension != Suspension::None {
        if !matches!(suspension, Suspension::Explicit | Suspension::ReplaceBound) {
            check_controller_loss(
                &client,
                &root,
                &popup,
                &owner.clock_id,
                epoch,
                &endpoint.browser_ws_url,
                suspension == Suspension::ControllerDisable,
            )
            .await;
        } else {
            check_suspension(
                &client,
                &root,
                &popup,
                &owner.clock_id,
                epoch,
                suspension == Suspension::ReplaceBound,
            )
            .await;
        }
        return;
    }
    if crash {
        for session in [&root, &popup] {
            eval(&client, session, "setInterval(()=>{},1);null".into()).await;
        }
        grant(&client, &root, true).await;
        grant(&client, &popup, false).await;
        let bound = ((epoch as i128 + 2050) * 1000).to_string();
        client
            .call(
                Some(&root),
                &page::FrontcapGrantSharedClockParams::new(owner.clock_id.clone(), bound.clone()),
            )
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = client
                .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
                .await
                .unwrap();
            if state.unix_time_us.parse::<i128>().unwrap() > (epoch as i128 + 50) * 1000 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let mut crashes = client.subscribe::<target::TargetCrashedEvent>(None);
        client
            .call(None, &target::SetDiscoverTargetsParams::new(true))
            .await
            .unwrap();
        let _crash = client
            .send(Some(&popup), &page::CrashParams::default())
            .unwrap();
        crashes
            .next_timeout(Duration::from_secs(5))
            .await
            .unwrap()
            .expect("confirmed native crash");
        let fresh = client
            .call(None, &target::CreateTargetParams::new("about:blank".into()))
            .await
            .unwrap();
        let reader = attach(&client, fresh.target_id).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let failed = loop {
            let state = client
                .call(
                    Some(&reader),
                    &page::FrontcapGetSharedClockParams::default(),
                )
                .await
                .unwrap();
            assert!(state.failed);
            if state.participants.iter().all(|p| !p.alive) {
                break state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "armed participants did not fail closed: {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        assert_eq!(failed.participants.len(), 2);
        assert_eq!(failed.clock_id, owner.clock_id);
        assert!(
            failed.unix_time_us.parse::<i128>().unwrap() < bound.parse::<i128>().unwrap(),
            "dead membership must not complete the grant"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            client
                .call(
                    Some(&reader),
                    &page::FrontcapGetSharedClockParams::default()
                )
                .await
                .unwrap()
                .unix_time_us,
            failed.unix_time_us
        );
        return;
    }
    for session in [&root, &popup] {
        eval(&client, session, format!("globalThis.samples=[];globalThis.sample=()=>samples.push({{at:Date.now()-{epoch},cookie:document.cookie}});sample();[400,1500,2000].forEach(t=>setTimeout(sample,t));null")).await;
    }
    if worker {
        assert_eq!(
            eval(&client, &root, "workerReady && crossOriginIsolated".into()).await,
            json!(true)
        );
        eval(
            &client,
            &root,
            "worker.postMessage(new SharedArrayBuffer(4));null".into(),
        )
        .await;
    }
    if synchronous {
        eval(&client, &root, "globalThis.syncSample=null;setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','/sync',false);x.send();syncSample={at:Date.now(),body:x.responseText,cookie:document.cookie}},500);null".into()).await;
    }
    if popup_first || synchronous {
        grant(&client, &popup, false).await;
    }
    if !synchronous {
        grant(&client, &root, true).await;
    }
    if !popup_first && !synchronous {
        grant(&client, &popup, false).await;
    }
    // Scheduler grants alone cannot publish time before browser authorization.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        client
            .call(Some(&root), &page::FrontcapGetStorageClockParams::default())
            .await
            .unwrap()
            .unix_time_us,
        ((epoch as i128 + 50) * 1000).to_string()
    );
    let end = ((epoch as i128 + 2050) * 1000).to_string();
    client
        .call(
            Some(&root),
            &page::FrontcapGrantSharedClockParams::new(owner.clock_id, end.clone()),
        )
        .await
        .unwrap();
    if synchronous {
        let advanced = generation.advance(VirtualDuration::from_ms(2000)).await;
        if advanced.is_err() {
            fn observed<T: serde::Serialize>(value: Result<Result<T, cdp::CdpError>, tokio::time::error::Elapsed>) -> Value {
                match value {
                    Ok(Ok(value)) => json!({"ok":value}),
                    Ok(Err(error)) => json!({"error":error.to_string()}),
                    Err(error) => json!({"timeout":error.to_string()}),
                }
            }
            let mut frames = serde_json::Map::new();
            for (name, session) in [("root", &root), ("popup", &popup)] {
                let clock = observed(tokio::time::timeout(Duration::from_secs(2), client.call(Some(session), &emulation::FrontcapGetClockParams::default())).await);
                let shared = observed(tokio::time::timeout(Duration::from_secs(2), client.call(Some(session), &page::FrontcapGetSharedClockParams::default())).await);
                let workers = observed(tokio::time::timeout(Duration::from_secs(2), client.call(Some(session), &emulation::FrontcapGetWorkersParams::default())).await);
                let tree = observed(tokio::time::timeout(Duration::from_secs(2), client.call(Some(session), &page::GetFrameTreeParams::default())).await);
                frames.insert(name.into(), json!({"clock":clock,"shared":shared,"workers":workers,"tree":tree}));
            }
            std::fs::write(runs.join("shared-frame-failure.json"), serde_json::to_vec_pretty(&json!({"outcome":format!("{advanced:?}"),"now_us":generation.now().0,"requested_end":end,"frames":frames})).unwrap()).unwrap();
        }
        advanced.unwrap();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let state = client
            .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        assert!(!state.failed);
        if state.unix_time_us == end {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shared clock stalled: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for session in [&root, &popup] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let samples = loop {
            let samples = eval(&client, session, "samples".into()).await;
            if samples.as_array().unwrap().len() == 4 {
                break samples;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "final-boundary callback missing: {samples}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        assert_eq!(
            samples,
            json!([
                {"at":50,"cookie":"shared=live"},{"at":450,"cookie":"shared=live"},
                {"at":1550,"cookie":""},{"at":2050,"cookie":""}
            ])
        );
        assert_eq!(
            client
                .call(Some(session), &emulation::FrontcapGetClockParams::default())
                .await
                .unwrap()
                .unix_time_us,
            Some(end.clone())
        );
    }
    if worker {
        assert_eq!(
            eval(&client, &root, "workerSamples".into()).await,
            json!([
                {"data":{"at":epoch+50,"after":epoch+350,"result":"timed-out"},"received":epoch+350,"cookie":"shared=live"}
            ])
        );
    }
    if synchronous {
        assert_eq!(
            eval(&client, &root, "syncSample".into()).await,
            json!({"at":epoch+1550,"body":"done","cookie":""})
        );
    }
    // A fractional-millisecond grant must retain microsecond precision across
    // both monotonic origins and the shared storage region.
    for (session, owner) in [(&root, true), (&popup, false)] {
        client
            .call(
                Some(session),
                &emulation::SetVirtualTimePolicyParams {
                    bind_storage_clock: Some(owner),
                    budget: Some(0.125),
                    ..emulation::SetVirtualTimePolicyParams::new(
                        emulation::VirtualTimePolicy::Advance,
                    )
                },
            )
            .await
            .unwrap();
    }
    let end = ((epoch as i128 + 2050) * 1000 + 125).to_string();
    if synchronous {
        // The generation now owns a held clock. Hand control back explicitly
        // for this native precision check instead of granting from a stale owner.
        let held = client
            .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        std::fs::write(runs.join("shared-frame-native-handoff-before.json"), serde_json::to_vec_pretty(&held).unwrap()).unwrap();
        assert_eq!(held.clock_id, state.clock_id);
        assert_eq!(held.unix_time_us, ((epoch as i128 + 2050) * 1000).to_string());
        assert!(held.controller_attached && held.activated && !held.failed, "{held:?}");
        assert!(!held.suspension_id.is_empty() || held.bound_us == held.unix_time_us,
            "the completed generation must be stopped: {held:?}");
        // A finite publication bound is also a stop. Acquire an explicit token
        // before transferring the generation's controller to this native session.
        let suspension = client
            .call(Some(&root), &page::FrontcapSuspendSharedClockParams::new(held.clock_id.clone()))
            .await
            .unwrap();
        assert_eq!(suspension.unix_time_us, held.unix_time_us);
        assert!(!suspension.suspension_id.is_empty());
        std::fs::write(runs.join("shared-frame-native-handoff.json"), serde_json::to_vec_pretty(&suspension).unwrap()).unwrap();
        client
            .call(Some(&root), &page::FrontcapResumeSharedClockParams {
                bound_us: Some(end.clone()),
                ..page::FrontcapResumeSharedClockParams::new(held.clock_id, suspension.suspension_id)
            })
            .await
            .unwrap();
    } else {
        client
            .call(
                Some(&root),
                &page::FrontcapGrantSharedClockParams::new(state.clock_id, end.clone()),
            )
            .await
            .unwrap();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while client
        .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap()
        .unix_time_us
        != end
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fractional grant stalled"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for session in [&root, &popup] {
        assert_eq!(
            client
                .call(Some(session), &emulation::FrontcapGetClockParams::default())
                .await
                .unwrap()
                .unix_time_us,
            Some(end.clone())
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        client
            .call(Some(&root), &page::FrontcapGetStorageClockParams::default())
            .await
            .unwrap()
            .unix_time_us,
        end
    );
}

async fn check_suspension(
    client: &cdp::Client,
    root: &str,
    popup: &str,
    clock: &str,
    epoch: u64,
    replace_bound: bool,
) {
    let bound = ((epoch as i128 + 1_000_050) * 1000).to_string();
    for (session, owner) in [(root, true), (popup, false)] {
        eval(client, session, "setInterval(()=>{},1);null".into()).await;
        client
            .call(
                Some(session),
                &emulation::SetVirtualTimePolicyParams {
                    budget: Some(1_000_000.0),
                    bind_storage_clock: Some(owner),
                    ..emulation::SetVirtualTimePolicyParams::new(
                        emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
                    )
                },
            )
            .await
            .unwrap();
    }
    client
        .call(
            Some(root),
            &page::FrontcapGrantSharedClockParams::new(clock.into(), bound.clone()),
        )
        .await
        .unwrap();
    let start = ((epoch as i128 + 50) * 1000).to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = client
            .call(Some(root), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        if state.unix_time_us != start {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let held = client
        .call(
            Some(root),
            &page::FrontcapSuspendSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    assert_eq!(held.bound_us, bound);
    assert!(held.unix_time_us.parse::<i128>().unwrap() < bound.parse::<i128>().unwrap());
    let same = client
        .call(
            Some(popup),
            &page::FrontcapSuspendSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    assert_eq!(same.suspension_id, held.suspension_id);
    assert_eq!(same.unix_time_us, held.unix_time_us);
    assert_eq!(same.bound_us, held.bound_us);
    client
        .call(
            None,
            &target::DetachFromTargetParams {
                session_id: Some(target::SessionID(root.to_owned())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = client
        .call(Some(popup), &page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    assert_eq!(state.unix_time_us, held.unix_time_us);
    assert_eq!(state.suspension_id, held.suspension_id);
    assert_eq!(state.bound_us, bound);
    assert!(
        client
            .call(
                Some(popup),
                &page::FrontcapGrantSharedClockParams::new(clock.into(), bound.clone())
            )
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                Some(popup),
                &page::FrontcapResumeSharedClockParams::new(clock.into(), "stale".into())
            )
            .await
            .is_err()
    );
    let bound = if replace_bound {
        let now = held.unix_time_us.parse::<i128>().unwrap();
        let replacement = (now + 1000).to_string();
        for (token, endpoint) in [
            ("stale", replacement.clone()),
            (&held.suspension_id, "invalid".into()),
            (&held.suspension_id, (now - 1).to_string()),
            (&held.suspension_id, i64::MAX.to_string()),
        ] {
            assert!(
                client
                    .call(
                        Some(popup),
                        &page::FrontcapResumeSharedClockParams {
                            bound_us: Some(endpoint),
                            ..page::FrontcapResumeSharedClockParams::new(clock.into(), token.into())
                        }
                    )
                    .await
                    .is_err()
            );
            let state = client
                .call(Some(popup), &page::FrontcapGetSharedClockParams::default())
                .await
                .unwrap();
            assert_eq!(state.unix_time_us, held.unix_time_us);
            assert_eq!(state.bound_us, bound);
            assert_eq!(state.suspension_id, held.suspension_id);
            assert!(!state.controller_attached);
        }
        replacement
    } else {
        bound
    };
    client
        .call(
            Some(popup),
            &page::FrontcapResumeSharedClockParams {
                bound_us: replace_bound.then(|| bound.clone()),
                ..page::FrontcapResumeSharedClockParams::new(
                    clock.into(),
                    held.suspension_id.clone(),
                )
            },
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = client
            .call(Some(popup), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        assert_eq!(state.bound_us, bound);
        assert!(state.unix_time_us.parse::<i128>().unwrap() <= bound.parse::<i128>().unwrap());
        if (replace_bound && state.unix_time_us == bound)
            || (!replace_bound && state.unix_time_us != held.unix_time_us)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "resumed clock stalled: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    if replace_bound {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state = client
            .call(Some(popup), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        assert_eq!(state.unix_time_us, bound);
        assert!(state.suspension_id.is_empty() && state.controller_attached);
    }
    let next = client
        .call(
            Some(popup),
            &page::FrontcapSuspendSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    assert_ne!(next.suspension_id, held.suspension_id);
    assert!(
        client
            .call(
                Some(popup),
                &page::FrontcapResumeSharedClockParams::new(clock.into(), held.suspension_id)
            )
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = client
        .call(Some(popup), &page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    assert_eq!(state.unix_time_us, next.unix_time_us);
    assert_eq!(state.bound_us, bound);
    assert!(!state.failed);
}

async fn check_controller_loss(
    client: &cdp::Client,
    root: &str,
    popup: &str,
    clock: &str,
    epoch: u64,
    url: &str,
    disable: bool,
) {
    let target_id = client
        .call(Some(root), &target::GetTargetInfoParams::default())
        .await
        .unwrap()
        .target_info
        .target_id;
    let reader = cdp::Client::connect(url).await.unwrap();
    let session = attach(&reader, target_id.clone()).await;
    let bound = ((epoch as i128 + 1_000_050) * 1000).to_string();
    for (session, owner) in [(root, true), (popup, false)] {
        eval(client, session, "setInterval(()=>{},1);null".into()).await;
        client
            .call(
                Some(session),
                &emulation::SetVirtualTimePolicyParams {
                    budget: Some(1_000_000.0),
                    bind_storage_clock: Some(owner),
                    ..emulation::SetVirtualTimePolicyParams::new(
                        emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
                    )
                },
            )
            .await
            .unwrap();
    }
    assert!(
        reader
            .call(
                Some(&session),
                &page::FrontcapGrantSharedClockParams::new(clock.into(), bound.clone())
            )
            .await
            .is_err()
    );
    client
        .call(
            Some(root),
            &page::FrontcapGrantSharedClockParams::new(clock.into(), bound.clone()),
        )
        .await
        .unwrap();
    wait_shared_change(
        &reader,
        &session,
        &((epoch as i128 + 50) * 1000).to_string(),
    )
    .await;
    if disable {
        client
            .call(Some(root), &page::DisableParams::default())
            .await
            .unwrap();
    } else {
        client.close();
    }
    let held = wait_controller_loss(&reader, &session).await;
    assert_eq!(held.bound_us, bound);
    assert!(held.unix_time_us.parse::<i128>().unwrap() < bound.parse::<i128>().unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let still = reader
        .call(
            Some(&session),
            &page::FrontcapGetSharedClockParams::default(),
        )
        .await
        .unwrap();
    assert_eq!(still.unix_time_us, held.unix_time_us);
    assert_eq!(still.suspension_id, held.suspension_id);
    // Re-activation is not adoption and cannot erase a disconnected controller's hold.
    reader
        .call(
            Some(&session),
            &page::FrontcapActivateSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    assert!(
        reader
            .call(
                Some(&session),
                &page::FrontcapGrantSharedClockParams::new(clock.into(), bound.clone())
            )
            .await
            .is_err()
    );
    reader
        .call(
            Some(&session),
            &page::FrontcapResumeSharedClockParams::new(clock.into(), held.suspension_id),
        )
        .await
        .unwrap();
    wait_shared_change(&reader, &session, &held.unix_time_us).await;
    let transfer = reader
        .call(
            Some(&session),
            &page::FrontcapSuspendSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    let replacement = cdp::Client::connect(url).await.unwrap();
    let replacement_session = attach(&replacement, target_id.clone()).await;
    replacement
        .call(
            Some(&replacement_session),
            &page::FrontcapResumeSharedClockParams::new(clock.into(), transfer.suspension_id),
        )
        .await
        .unwrap();
    reader.close();
    wait_shared_change(&replacement, &replacement_session, &transfer.unix_time_us).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = replacement
        .call(
            Some(&replacement_session),
            &page::FrontcapGetSharedClockParams::default(),
        )
        .await
        .unwrap();
    assert!(state.controller_attached && state.suspension_id.is_empty() && !state.failed);
    assert_eq!(state.bound_us, bound);
    let final_reader = cdp::Client::connect(url).await.unwrap();
    let final_session = attach(&final_reader, target_id).await;
    replacement.close();
    let second = wait_controller_loss(&final_reader, &final_session).await;
    assert_ne!(second.suspension_id, still.suspension_id);
    assert_eq!(second.bound_us, bound);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = final_reader
        .call(
            Some(&final_session),
            &page::FrontcapGetSharedClockParams::default(),
        )
        .await
        .unwrap();
    assert_eq!(state.unix_time_us, second.unix_time_us);
    assert!(!state.failed);
}

async fn wait_shared_change(client: &cdp::Client, session: &str, before: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = client
            .call(
                Some(session),
                &page::FrontcapGetSharedClockParams::default(),
            )
            .await
            .unwrap();
        assert!(!state.failed);
        if state.unix_time_us != before {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shared progress stalled: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn wait_controller_loss(
    client: &cdp::Client,
    session: &str,
) -> page::FrontcapGetSharedClockReturns {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = client
            .call(
                Some(session),
                &page::FrontcapGetSharedClockParams::default(),
            )
            .await
            .unwrap();
        assert!(!state.failed);
        if !state.controller_attached && !state.suspension_id.is_empty() {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "controller loss not retained: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn check_process_control(client: &cdp::Client, root: &str, popup: &str, clock: &str) {
    let renderer = client
        .call(Some(root), &emulation::FrontcapGetClockParams::default())
        .await
        .unwrap()
        .renderer_id;
    let snapshot = page::FrontcapGetSharedProcessParams::new(clock.into(), renderer.clone());
    assert!(
        client.call(Some(root), &snapshot).await.is_err(),
        "snapshot requires publication hold"
    );
    let held = client
        .call(
            Some(root),
            &page::FrontcapSuspendSharedClockParams::new(clock.into()),
        )
        .await
        .unwrap();
    let native = client.call(Some(root), &snapshot).await.unwrap();
    assert!(native.workers.is_empty());
    assert!(native.main_thread_arrival_us.is_none());
    for (identity, process) in [("foreign", renderer.as_str()), (clock, "foreign")] {
        assert!(
            client
                .call(
                    Some(root),
                    &page::FrontcapGetSharedProcessParams::new(identity.into(), process.into(),)
                )
                .await
                .is_err()
        );
    }
    for (session, identity, token, process, request) in [
        (
            root,
            "foreign",
            held.suspension_id.as_str(),
            renderer.as_str(),
            "1.1",
        ),
        (root, clock, "stale", renderer.as_str(), "1.1"),
        (root, clock, held.suspension_id.as_str(), "foreign", "1.1"),
        (
            root,
            clock,
            held.suspension_id.as_str(),
            renderer.as_str(),
            "malformed",
        ),
        (
            popup,
            clock,
            held.suspension_id.as_str(),
            renderer.as_str(),
            "1.1",
        ),
        (
            root,
            clock,
            held.suspension_id.as_str(),
            renderer.as_str(),
            "1.1",
        ),
    ] {
        assert!(
            client
                .call(
                    Some(session),
                    &page::FrontcapSetSharedWorkerRequestPausedParams::new(
                        identity.into(),
                        token.into(),
                        process.into(),
                        cdp::protocol::network::RequestId(request.into()),
                        false,
                    )
                )
                .await
                .is_err()
        );
        let state = client
            .call(Some(root), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        assert!(!state.failed && state.controller_attached);
        assert_eq!(state.unix_time_us, held.unix_time_us);
        assert_eq!(state.suspension_id, held.suspension_id);
        assert_eq!(state.bound_us, held.bound_us);
    }
    // Use real renderer prefixes as well: one reaches this process's native
    // registry, while foreign process prefixes must be rejected before mutation.
    let processes = client
        .call(
            None,
            &cdp::protocol::system_info::GetProcessInfoParams::default(),
        )
        .await
        .unwrap();
    let mut native_absence = false;
    for process in processes
        .process_info
        .iter()
        .filter(|p| p.r#type == "renderer")
    {
        let result = client
            .call(
                Some(root),
                &page::FrontcapSetSharedWorkerRequestPausedParams::new(
                    clock.into(),
                    held.suspension_id.clone(),
                    renderer.clone(),
                    cdp::protocol::network::RequestId(format!("{}.{}", process.id, u64::MAX)),
                    false,
                ),
            )
            .await;
        match result {
            Err(cdp::CdpError::Protocol { code: -32000, .. }) => native_absence = true,
            Err(cdp::CdpError::Protocol { code: -32602, .. }) => {}
            other => panic!("unexpected process load-control result: {other:?}"),
        }
    }
    assert!(
        native_absence,
        "a correctly scoped absent load must reach the native registry"
    );
    let state = client
        .call(Some(root), &page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    assert!(!state.failed);
    assert_eq!(state.unix_time_us, held.unix_time_us);
    assert_eq!(state.bound_us, held.bound_us);
    assert_eq!(state.suspension_id, held.suspension_id);
    client
        .call(
            Some(root),
            &page::FrontcapResumeSharedClockParams::new(clock.into(), held.suspension_id),
        )
        .await
        .unwrap();
}
