//! Native shared-memory transport only. This does not activate shared renderer scheduling.
mod support;
use cdp::protocol::{emulation, page, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{
    ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration,
};
use serde_json::json;
use std::time::Duration;

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

#[tokio::test]
async fn shared_transport_registers_renderer_lifetimes_without_switching_clocks() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), check_transport())
        .await
        .expect("shared transport check timed out");
}

async fn check_transport() {
    let runs = tempfile::tempdir().unwrap();
    let (mut generation, epoch) = launch_transport(runs.path()).await;
    eprintln!("shared transport: attach sessions");
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let root_id = target::TargetID(endpoint.target_id.clone());
    let root = attach(&client, root_id.clone()).await;
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let popup_id = targets
        .target_infos
        .iter()
        .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
        .unwrap()
        .target_id
        .clone();
    let popup = attach(&client, popup_id).await;
    let root_clock = client
        .call(Some(&root), &emulation::FrontcapGetClockParams::default())
        .await
        .unwrap();
    let popup_clock = client
        .call(Some(&popup), &emulation::FrontcapGetClockParams::default())
        .await
        .unwrap();
    assert_ne!(root_clock.renderer_id, popup_clock.renderer_id);
    let prepare = emulation::FrontcapPrepareSharedClockParams::default();
    eprintln!("shared transport: prepare owner");
    let owner = client.call(Some(&root), &prepare).await.unwrap();
    eprintln!("shared transport: prepare popup");
    let participant = client.call(Some(&popup), &prepare).await.unwrap();
    assert!(!client.call(Some(&root), &page::FrontcapGetSharedClockParams::default()).await.unwrap().activated,
        "transport preparation must be tested before shared scheduling activation");
    assert!(!owner.clock_id.is_empty());
    assert_eq!(owner.clock_id, participant.clock_id);
    assert_ne!(owner.participant_id, participant.participant_id);
    assert_eq!(owner.unix_time_us, (epoch + 50_000).to_string());
    assert_eq!(owner.unix_time_us, participant.unix_time_us);
    eprintln!("shared transport: deduplicate");
    let second_root = attach(&client, root_id).await;
    let again = client.call(Some(&second_root), &prepare).await.unwrap();
    assert_eq!(again.clock_id, owner.clock_id);
    assert_eq!(
        again.participant_id, owner.participant_id,
        "a second inspector must not register another process vote"
    );
    eprintln!("shared transport: frozen clocks");
    tokio::time::sleep(Duration::from_millis(100)).await;
    for session in [&root, &popup] {
        assert_eq!(
            client
                .call(Some(session), &emulation::FrontcapGetClockParams::default())
                .await
                .unwrap()
                .unix_time_us,
            Some(owner.unix_time_us.clone())
        );
        assert_eq!(
            client
                .call(
                    Some(session),
                    &page::FrontcapGetStorageClockParams::default()
                )
                .await
                .unwrap()
                .unix_time_us,
            owner.unix_time_us
        );
    }
    eprintln!("shared transport: cookie inspection");
    assert_eq!(
        generation.evaluate("document.cookie").await.unwrap(),
        json!("transport=live")
    );

    // Preparation is not activation. Ordinary native progress still uses the old clock;
    // a stale prepared transport must refuse reuse instead of rewinding either clock.
    eprintln!("shared transport: ordinary advance");
    client
        .call(
            Some(&root),
            &emulation::SetVirtualTimePolicyParams {
                budget: Some(50.0),
                ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
            },
        )
        .await
        .unwrap();
    eprintln!("shared transport: advance accepted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .unix_time_us
            .unwrap();
        if now == (epoch + 100_000).to_string() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    eprintln!("shared transport: reject stale preparation");
    assert!(
        client
            .call(Some(&root), &prepare)
            .await
            .unwrap_err()
            .to_string()
            .contains("transport preparation failed")
    );
    assert_eq!(
        client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .unix_time_us,
        Some((epoch + 100_000).to_string())
    );
    assert_eq!(
        client
            .call(Some(&popup), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .unix_time_us,
        Some((epoch + 50_000).to_string())
    );
    for session in [root, popup, second_root] {
        client
            .call(
                None,
                &target::DetachFromTargetParams {
                    session_id: Some(target::SessionID(session)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
}

async fn launch_transport(runs: &std::path::Path) -> (Generation, i128) {
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":"<!doctype html><button id=open onclick=\"window.open('https://auth.example.test/')\">open</button>"}},
        {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html>popup"}}
    ]).to_string()).unwrap();
    let mut options = support::options(
        runs,
        "shared-transport",
        "https://app.example.test/".into(),
        rules,
    );
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms as i128 * 1000;
    eprintln!("shared transport: launch");
    let mut generation = Generation::test_launch_before_shared_activation(options).await.unwrap();
    eprintln!("shared transport: advance");
    generation
        .advance(VirtualDuration::from_ms(50))
        .await
        .unwrap();
    generation
        .evaluate("document.cookie='transport=live; Max-Age=1; Path=/'")
        .await
        .unwrap();
    eprintln!("shared transport: popup click");
    generation
        .click(
            RequestId::new("open").unwrap(),
            &Locator::Css {
                selector: "#open".into(),
            },
        )
        .await
        .unwrap();
    (generation, epoch)
}

#[tokio::test]
async fn a_registered_popup_crash_invalidates_shared_clock_preparation() {
    check_native_loss(false, true).await;
}

#[tokio::test]
async fn a_registered_owner_crash_invalidates_shared_clock_preparation() {
    check_native_loss(true, true).await;
}

#[tokio::test]
async fn an_unregistered_popup_crash_does_not_invalidate_other_membership() {
    check_native_loss(false, false).await;
}

async fn check_native_loss(crash_owner: bool, register_popup: bool) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let runs = tempfile::tempdir().unwrap();
        let (generation, epoch) = launch_transport(runs.path()).await;
        let endpoint = generation.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap();
        let root_id = target::TargetID(endpoint.target_id.clone());
        let root = attach(&client, root_id.clone()).await;
        let targets = client
            .call(None, &target::GetTargetsParams::default())
            .await
            .unwrap();
        let popup_id = targets
            .target_infos
            .iter()
            .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
            .unwrap()
            .target_id
            .clone();
        let popup = attach(&client, popup_id.clone()).await;
        let prepare = emulation::FrontcapPrepareSharedClockParams::default();
        let owner = client.call(Some(&root), &prepare).await.unwrap();
        if register_popup {
            client.call(Some(&popup), &prepare).await.unwrap();
        }
        let read = page::FrontcapGetSharedClockParams::default();
        let before = client.call(Some(&root), &read).await.unwrap();
        assert!(!before.activated, "native crash fixture must remain before shared activation");
        assert_eq!(before.clock_id, owner.clock_id);
        assert_eq!(before.unix_time_us, (epoch + 50_000).to_string());
        assert!(!before.failed);
        assert_eq!(
            before.participants.len(),
            if register_popup { 2 } else { 1 }
        );
        assert!(before.participants.iter().all(|p| p.alive && p.confirmed));

        // An inspector disconnect does not prove native process death or retire a vote.
        client
            .call(
                None,
                &target::DetachFromTargetParams {
                    session_id: Some(target::SessionID(popup)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let after_detach = client.call(Some(&root), &read).await.unwrap();
        assert!(!after_detach.failed);
        assert!(
            after_detach
                .participants
                .iter()
                .all(|p| p.alive && p.confirmed)
        );
        let survivor_id = if crash_owner {
            popup_id.clone()
        } else {
            root_id.clone()
        };
        let popup = attach(&client, popup_id).await;
        let (victim, survivor) = if crash_owner {
            (&root, &popup)
        } else {
            (&popup, &root)
        };
        let victim_renderer = client
            .call(Some(victim), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .renderer_id;
        let survivor_renderer = client
            .call(
                Some(survivor),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap()
            .renderer_id;
        assert_ne!(victim_renderer, survivor_renderer);
        let mut crashed = client.subscribe::<target::TargetCrashedEvent>(None);
        client
            .call(None, &target::SetDiscoverTargetsParams::new(true))
            .await
            .unwrap();
        let _answer = client
            .send(Some(victim), &page::CrashParams::default())
            .unwrap();
        let expected_target = if crash_owner {
            root_id
        } else {
            targets
                .target_infos
                .iter()
                .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
                .unwrap()
                .target_id
                .clone()
        };
        loop {
            let event = crashed
                .next_timeout(Duration::from_secs(5))
                .await
                .unwrap()
                .expect("native crash notification");
            if event.target_id == expected_target {
                break;
            }
        }
        let state = client.call(Some(survivor), &read).await.unwrap();
        assert_eq!(state.clock_id, before.clock_id);
        assert_eq!(state.unix_time_us, before.unix_time_us);
        assert_eq!(
            state.participants.len(),
            before.participants.len(),
            "dead votes must not disappear"
        );
        assert_eq!(state.failed, register_popup || crash_owner);
        for participant in &state.participants {
            assert!(participant.confirmed);
            assert_eq!(
                participant.alive,
                participant.renderer_id != victim_renderer
            );
        }
        assert_eq!(
            client
                .call(
                    Some(survivor),
                    &emulation::FrontcapGetClockParams::default()
                )
                .await
                .unwrap()
                .unix_time_us,
            Some(before.unix_time_us.clone())
        );
        assert_eq!(
            client
                .call(
                    Some(survivor),
                    &page::FrontcapGetStorageClockParams::default()
                )
                .await
                .unwrap()
                .unix_time_us,
            before.unix_time_us
        );
        if state.failed {
            assert!(
                client
                    .call(Some(survivor), &prepare)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("transport preparation failed")
            );
        } else {
            assert_eq!(
                client
                    .call(Some(survivor), &prepare)
                    .await
                    .unwrap()
                    .participant_id,
                owner.participant_id
            );
        }
        // A fresh inspector still sees native failure. Session replacement is not recovery.
        let other = attach(&client, survivor_id.clone()).await;
        let fresh = client.call(Some(&other), &read).await.unwrap();
        assert_eq!(fresh.failed, state.failed);
        assert_eq!(fresh.participants.len(), state.participants.len());
        if crash_owner && register_popup {
            // Even after every prepared renderer is gone, a new target in the
            // partition must discover the failed authority instead of replacing it.
            let _answer = client
                .send(Some(survivor), &page::CrashParams::default())
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let event = crashed
                    .next_timeout(Duration::from_secs(5))
                    .await
                    .unwrap()
                    .expect("last participant crash");
                if event.target_id == survivor_id {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline);
            }
            let replacement = client
                .call(None, &target::CreateTargetParams::new("about:blank".into()))
                .await
                .unwrap();
            let reader = attach(&client, replacement.target_id).await;
            let failed = client.call(Some(&reader), &read).await.unwrap();
            assert!(failed.failed);
            assert_eq!(failed.clock_id, state.clock_id);
            assert_eq!(failed.unix_time_us, state.unix_time_us);
            assert_eq!(failed.participants.len(), 2);
            assert!(failed.participants.iter().all(|p| !p.alive && p.confirmed));
        }
        drop(generation);
    })
    .await
    .expect("native membership-loss check timed out");
}

#[tokio::test]
async fn shared_window_admission_requires_matching_held_authority_and_armed_renderer() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let runs = tempfile::tempdir().unwrap();
        let (generation, epoch) = launch_transport(runs.path()).await;
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
        let owner = client
            .call(
                Some(&root),
                &emulation::FrontcapPrepareSharedClockParams::default(),
            )
            .await
            .unwrap();
        let admit = page::FrontcapAdmitSharedWindowParams::new(owner.clock_id.clone());
        let acknowledge = page::FrontcapAcknowledgeSharedWindowClosureParams::new(
            owner.clock_id.clone(),
            "absent-target".into(),
        );
        assert!(
            client
                .call(
                    Some(&root),
                    &page::FrontcapAcknowledgeSharedWindowClosureParams::new(
                        "foreign".into(),
                        "absent-target".into()
                    )
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("scope mismatch")
        );
        assert!(
            client
                .call(Some(&root), &acknowledge)
                .await
                .unwrap_err()
                .to_string()
                .contains("held active clock")
        );
        assert!(
            client
                .call(
                    Some(&root),
                    &page::FrontcapAdmitSharedWindowParams::new("foreign".into())
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("scope mismatch")
        );
        assert!(
            client
                .call(Some(&root), &admit)
                .await
                .unwrap_err()
                .to_string()
                .contains("held active clock")
        );
        client
            .call(
                Some(&root),
                &emulation::FrontcapArmSharedClockParams::default(),
            )
            .await
            .unwrap();
        client
            .call(
                Some(&root),
                &page::FrontcapActivateSharedClockParams::new(owner.clock_id.clone()),
            )
            .await
            .unwrap();
        assert!(
            client
                .call(Some(&root), &admit)
                .await
                .unwrap_err()
                .to_string()
                .contains("held active clock")
        );
        assert!(
            client
                .call(Some(&root), &acknowledge)
                .await
                .unwrap_err()
                .to_string()
                .contains("held active clock")
        );
        let held = client
            .call(
                Some(&root),
                &page::FrontcapSuspendSharedClockParams::new(owner.clock_id.clone()),
            )
            .await
            .unwrap();
        assert!(
            client
                .call(Some(&popup), &admit)
                .await
                .unwrap_err()
                .to_string()
                .contains("renderer is not armed")
        );
        client
            .call(
                Some(&popup),
                &emulation::FrontcapPrepareSharedClockParams::default(),
            )
            .await
            .unwrap();
        assert!(
            client
                .call(Some(&popup), &admit)
                .await
                .unwrap_err()
                .to_string()
                .contains("renderer is not armed")
        );
        client
            .call(
                Some(&popup),
                &emulation::FrontcapArmSharedClockParams::default(),
            )
            .await
            .unwrap();
        let popup_clock = client
            .call(Some(&popup), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap();
        let root_clock = client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap();
        assert_ne!(popup_clock.renderer_id, root_clock.renderer_id);
        for _ in 0..2 {
            let admitted = client.call(Some(&popup), &admit).await.unwrap();
            assert_eq!(admitted.renderer_id, popup_clock.renderer_id);
        }
        let admitted = client.call(Some(&root), &admit).await.unwrap();
        assert_eq!(admitted.renderer_id, root_clock.renderer_id);
        // Acknowledging an absent receipt twice is harmless, but
        // must neither release the explicit hold nor alter live registrations.
        client.call(Some(&root), &acknowledge).await.unwrap();
        client.call(Some(&root), &acknowledge).await.unwrap();
        assert!(
            client
                .call(
                    Some(&root),
                    &page::FrontcapGetSharedWindowClosuresParams::default()
                )
                .await
                .unwrap()
                .closures
                .is_empty()
        );
        let state = client
            .call(Some(&root), &page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        assert!(!state.failed);
        assert_eq!(state.unix_time_us, (epoch + 50_000).to_string());
        assert_eq!(
            state.suspension_id, held.suspension_id,
            "admission must not release the controller hold"
        );
        assert_eq!(state.participants.len(), 2);
        assert!(
            state
                .participants
                .iter()
                .all(|p| p.alive && p.confirmed && p.armed)
        );
    })
    .await
    .expect("shared window admission watchdog");
}
