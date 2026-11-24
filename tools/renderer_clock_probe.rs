//! Opt-in renderer clock inventory probe. Reports admission defects without terminating renderers.
//! Successful execution qualifies clock observation, not multi-renderer progress.
mod support;
use cdp::protocol::{emulation, system_info, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration};
use serde_json::json;

#[derive(Clone, Copy, Debug)]
enum Policy {
    None,
    Coop,
    Isolated,
}

async fn renderers(client: &cdp::Client) -> Vec<i64> {
    let info = client.call(None, &system_info::GetProcessInfoParams::default()).await.unwrap();
    let mut ids: Vec<i64> = info.process_info.iter().filter(|p| p.r#type == "renderer").map(|p| p.id as i64).collect();
    ids.sort();
    ids
}

/// Observe through a separate session; no evaluation is needed to identify a renderer or its clock.
async fn clock(client: &cdp::Client, id: target::TargetID) -> serde_json::Value {
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(id)
    }).await.unwrap();
    let session = attached.session_id.0.clone();
    let first = client.call(Some(&session), &emulation::FrontcapGetClockParams::default()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let second = client.call(Some(&session), &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_eq!(first.renderer_id, second.renderer_id, "no navigation during observation");
    assert_eq!(first.enabled, second.enabled, "observation must not enable the clock");
    assert_eq!(first.unix_time_us, second.unix_time_us, "an enabled stopped clock must not progress on host time");
    assert_eq!(first.enabled, first.unix_time_us.is_some());
    assert!(!first.renderer_id.is_empty());
    if let Some(ref now) = first.unix_time_us {
        let legacy = client.call(Some(&session), &emulation::GetVirtualTimeParams::default()).await.unwrap();
        assert_eq!(now, &legacy.unix_time_us);
    } else {
        assert!(client.call(Some(&session), &emulation::GetVirtualTimeParams::default()).await.is_err());
    }
    client.call(None, &target::DetachFromTargetParams {
        session_id: Some(attached.session_id), ..Default::default()
    }).await.unwrap();
    json!({"renderer_id":first.renderer_id,"enabled":first.enabled,"unix_time_us":first.unix_time_us})
}

#[tokio::test]
async fn renderer_clock_probe() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mut admission_failures = Vec::new();
    let mut observed = Vec::new();
    for (page_policy, popup_policy, then_navigate) in [
        (Policy::None, Policy::None, false),
        (Policy::None, Policy::Coop, false),
        (Policy::Isolated, Policy::None, false),
        (Policy::None, Policy::Isolated, false),
        (Policy::Isolated, Policy::Isolated, false),
        (Policy::None, Policy::None, true),
    ] {
        let case = format!("page={page_policy:?} popup={popup_policy:?} then_navigate_isolated={then_navigate}");
        eprintln!("=== {case}");
        let runs = tempfile::tempdir().unwrap();
        let page = "<!doctype html><button id=open onclick=\"window.w=window.open('https://auth.example.test/popup')\">open</button><button id=go onclick=\"location.href='/isolated'\">go</button><script>fetch('/marker');window.ticks=[];setInterval(()=>ticks.push(Date.now()),10)</script>";
        let popup = "<!doctype html><p>popup</p><script>fetch('/marker');window.ticks=[];setInterval(()=>ticks.push(Date.now()),10)</script>";
        let isolated = "<!doctype html><p>isolated</p><script>fetch('/marker')</script>";
        let mock = |id: &str, url: &str, body: &str, policy: Policy| {
            let mut headers = json!({"content-type":"text/html"});
            if matches!(policy, Policy::Coop | Policy::Isolated) {
                headers["cross-origin-opener-policy"] = json!("same-origin");
            }
            if matches!(policy, Policy::Isolated) {
                headers["cross-origin-embedder-policy"] = json!("require-corp");
            }
            json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":headers,"body":body}})
        };
        let rules = RuleSet::from_json(&json!([
            mock("page","https://app.example.test/",page,page_policy),
            mock("isolated","https://app.example.test/isolated",isolated,Policy::Isolated),
            mock("popup","https://auth.example.test/popup",popup,popup_policy),
            {"id":"marker","match":{"url":"https://*/marker"},"action":{"type":"mock","status":200,"body":"ok"}}
        ]).to_string()).unwrap();
        let mut options = support::options(runs.path(), "renderer-admission", "https://app.example.test/".into(), rules);
        options.passthrough_origins = Some(vec![]);
        options.profile.popups = PopupPolicy::Controlled;
        let mut g = Generation::launch(options).await.unwrap();
        let client = cdp::Client::connect(&g.endpoint().browser_ws_url).await.unwrap();
        let at_launch = renderers(&client).await;
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        let before_open = renderers(&client).await;
        let page_before = clock(&client, target::TargetID(g.endpoint().target_id.clone())).await;
        let click = g.click(RequestId::new("open").unwrap(), &Locator::Css { selector: "#open".into() }).await.map_err(|e| e.to_string());
        let after_click = renderers(&client).await;
        let advance = g.advance(VirtualDuration::from_ms(50)).await.map_err(|e| e.to_string());
        let mut navigated = None;
        if then_navigate {
            let go = g.click(RequestId::new("go").unwrap(), &Locator::Css { selector: "#go".into() }).await.map_err(|e| e.to_string());
            let after = g.advance(VirtualDuration::from_ms(50)).await.map_err(|e| e.to_string());
            navigated = Some((go, after));
        }
        let after_advance = renderers(&client).await;
        let opener_view = g.evaluate("({url:location.href,isolated:crossOriginIsolated,closed:window.w?.closed,now:Date.now()})").await.map_err(|e| e.to_string());
        let expected = 946_684_800_000u64 + g.now().0 / 1000;
        let popup_view = match g.enter_context("popup-1") {
            Ok(()) => {
                let v = g
                    .evaluate("({isolated:crossOriginIsolated,openerNull:window.opener===null,now:Date.now(),ready:document.readyState,ticks:globalThis.ticks?.length})")
                    .await
                    .map_err(|e| e.to_string());
                g.leave_context();
                v
            }
            Err(e) => Err(e.to_string()),
        };
        let targets = client.call(None, &target::GetTargetsParams::default()).await.unwrap();
        let popup_target = targets
            .target_infos
            .iter()
            .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
            .map(|t| t.target_id.clone());
        let own_clock = clock(&client, popup_target.clone().expect("retained popup target")).await;
        let page_target = targets.target_infos.iter().find(|t| t.r#type == "page" && t.url.starts_with("https://app.example.test/"))
            .expect("launch page target").target_id.clone();
        let page_clock = clock(&client, page_target).await;
        let same_renderer = own_clock["renderer_id"] == page_clock["renderer_id"];
        if matches!(page_policy, Policy::None) && !matches!(popup_policy, Policy::Isolated) && !then_navigate {
            assert!(same_renderer, "control fixture uses one renderer");
            assert_eq!(own_clock["unix_time_us"], page_clock["unix_time_us"]);
        } else {
            assert!(!same_renderer, "isolation fixture must exercise independent clocks");
        }
        if then_navigate {
            assert_ne!(page_before["renderer_id"], page_clock["renderer_id"]);
            assert_eq!(page_before["renderer_id"], own_clock["renderer_id"]);
        }
        observed.push(json!({"case":case,"page_before":page_before,"page":page_clock,"popup":own_clock}));
        let bindings = frontcap_chromium::bindings::BindingJournal::read(&runs.path().join("renderer-admission/interceptions.jsonl")).unwrap();
        let markers: Vec<_> = bindings
            .iter()
            .filter_map(|e| match e {
                frontcap_chromium::bindings::BindingEntry::Bound(b) if b.url.ends_with("/marker") => Some((b.url.clone(), b.network_id.clone())),
                _ => None,
            })
            .collect();
        eprintln!("click={click:?} advance={advance:?} navigated={navigated:?}");
        eprintln!("renderers: at_launch={at_launch:?} before_open={before_open:?} after_click={after_click:?} final={after_advance:?}");
        eprintln!("opener: {opener_view:?}; popup: {popup_view:?}; expected_now={expected}");
        eprintln!("popup own clock: {own_clock}");
        eprintln!("markers: {markers:?}");
        let admitted = popup_view.as_ref().is_ok_and(|v| v["now"] == json!(expected) && v["ready"] == json!("complete"));
        if click.is_err() || advance.is_err() || !admitted {
            admission_failures.push(case);
        }
        drop(g);
    }
    assert_eq!(observed.len(), 6);
    println!("RENDERER_CLOCK_PROBE {}", json!({"observations":observed,"admission_failures":admission_failures}));
}
