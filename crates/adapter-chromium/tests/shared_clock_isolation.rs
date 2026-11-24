//! The six retained isolation cases through normal shared-generation operations.
mod support;
use cdp::protocol::{emulation, runtime, target};
use frontcap_chromium::Generation;
use frontcap_core::{profile::PopupPolicy, rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Policy {
    None,
    Coop,
    Isolated,
}
impl Policy {
    fn headers(self) -> Value {
        let mut headers = json!({"content-type":"text/html"});
        if !matches!(self, Self::None) {
            headers["cross-origin-opener-policy"] = json!("same-origin");
        }
        if matches!(self, Self::Isolated) {
            headers["cross-origin-embedder-policy"] = json!("require-corp");
        }
        headers
    }
    fn isolated(self) -> bool {
        matches!(self, Self::Isolated)
    }
}

#[tokio::test]
async fn neither_document_isolated() {
    check(Policy::None, Policy::None, false).await;
}
#[tokio::test]
async fn coop_only_popup_has_no_opener_and_keeps_shared_time() {
    check(Policy::None, Policy::Coop, false).await;
}
#[tokio::test]
async fn isolated_opener_admits_an_unisolated_popup() {
    check(Policy::Isolated, Policy::None, false).await;
}
#[tokio::test]
async fn unisolated_opener_admits_an_isolated_popup() {
    check(Policy::None, Policy::Isolated, false).await;
}
#[tokio::test]
async fn isolated_documents_at_different_origins_share_time() {
    check(Policy::Isolated, Policy::Isolated, false).await;
}
#[tokio::test]
async fn opener_renderer_replacement_preserves_its_surviving_popup() {
    check(Policy::None, Policy::None, true).await;
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
    result.result.value.unwrap()
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
async fn check(root: Policy, popup: Policy, replace: bool) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    static RUN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _run = RUN.lock().await;
    tokio::time::timeout(Duration::from_secs(90), run(root, popup, replace))
        .await
        .unwrap();
}
async fn run(root_policy: Policy, popup_policy: Policy, replace: bool) {
    let runs = tempfile::tempdir().unwrap();
    let popup_body = r#"<!doctype html><script>
      globalThis.first={date:Date.now(),origin:performance.timeOrigin,now:performance.now(),isolated:crossOriginIsolated,opener:opener!==null};
      globalThis.samples=[];
      setTimeout(()=>samples.push({at:Date.now(),cookie:document.cookie}),100);
      setTimeout(()=>samples.push({at:Date.now(),cookie:document.cookie}),1100);
    </script>"#;
    let popup_body = if replace {
        format!(
            "{popup_body}<script>globalThis.pendingResponses=[];setTimeout(()=>fetch('/pending').then(r=>r.text()).then(body=>pendingResponses.push({{at:Date.now(),body}})),400);</script>"
        )
    } else {
        popup_body.to_owned()
    };
    let root_body = "<!doctype html><script>globalThis.first={date:Date.now(),origin:performance.timeOrigin,now:performance.now(),isolated:crossOriginIsolated};globalThis.samples=[];</script>";
    let rules = RuleSet::from_json(&json!([
        {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":root_policy.headers(),"body":root_body}},
        {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,"headers":popup_policy.headers(),"body":popup_body}},
        {"id":"pending","match":{"url":"https://auth.example.test/pending"},"action":{"type":"mock","status":200,"body":"survived","latency_ms":400}},
        {"id":"replacement","match":{"url":"https://app.example.test/isolated"},"action":{"type":"mock","status":200,"headers":Policy::Isolated.headers(),"body":root_body}}
    ]).to_string()).unwrap();
    let mut options = support::options(
        runs.path(),
        "shared-isolation",
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
    generation.eval_main("document.cookie='matrix=live; Domain=example.test; Path=/; Max-Age=1';setTimeout(()=>window.open('https://auth.example.test/'),100);setTimeout(()=>samples.push({at:Date.now(),cookie:document.cookie}),200);true").await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(500))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 550_000);
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let root = attach(&client, target::TargetID(endpoint.target_id.clone())).await;
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let popup_target = targets
        .target_infos
        .iter()
        .find(|t| t.r#type == "page" && t.url == "https://auth.example.test/")
        .expect("timer-created popup");
    let popup = attach(&client, popup_target.target_id.clone()).await;
    for (session, first_time, isolated) in [
        (&root, epoch, root_policy.isolated()),
        (&popup, epoch + 150, popup_policy.isolated()),
    ] {
        let first = read(&client, session, "first").await;
        assert_eq!(first["date"], json!(first_time));
        assert_eq!(first["isolated"], json!(isolated));
        let origin = first["origin"].as_f64().unwrap();
        let now = first["now"].as_f64().unwrap();
        assert!(
            (origin + now - first_time as f64).abs() < 1.0,
            "first performance clock: {first}"
        );
        assert_eq!(
            read(&client, session, "samples").await,
            json!([{ "at":epoch+250,"cookie":"matrix=live" }])
        );
    }
    if !matches!(root_policy, Policy::None) || !matches!(popup_policy, Policy::None) {
        assert_eq!(read(&client, &popup, "first.opener").await, json!(false));
    }
    let before = client
        .call(Some(&root), &emulation::FrontcapGetClockParams::default())
        .await
        .unwrap();
    if replace {
        generation
            .eval_main("setTimeout(()=>location.href='/isolated',200);true")
            .await
            .unwrap();
    } else {
        generation
            .eval_main(
                "setTimeout(()=>samples.push({at:Date.now(),cookie:document.cookie}),700);true",
            )
            .await
            .unwrap();
    }
    generation
        .advance(VirtualDuration::from_ms(700))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 1_250_000);
    assert_eq!(
        read(&client, &popup, "samples").await,
        json!([
            {"at":epoch+250,"cookie":"matrix=live"}, {"at":epoch+1250,"cookie":""}
        ])
    );
    if replace {
        assert_eq!(
            read(&client, &popup, "pendingResponses").await,
            json!([{"at":epoch+950,"body":"survived"}])
        );
        let first = read(&client, &root, "first").await;
        assert_eq!(first["date"], json!(epoch + 750));
        assert_eq!(first["isolated"], json!(true));
        let after = client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap();
        assert_ne!(
            before.renderer_id, after.renderer_id,
            "the navigation must exercise renderer replacement"
        );
    } else {
        assert_eq!(
            read(&client, &root, "samples").await,
            json!([
                {"at":epoch+250,"cookie":"matrix=live"}, {"at":epoch+1250,"cookie":""}
            ])
        );
    }
    generation.advance(VirtualDuration(125)).await.unwrap();
    let exact = (i128::from(epoch) * 1000 + 1_250_125).to_string();
    for session in [&root, &popup] {
        let clock = client
            .call(Some(session), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap();
        assert_eq!(clock.unix_time_us.as_ref(), Some(&exact));
    }
    let expression = "[Date.now(),performance.timeOrigin,performance.now(),samples]";
    let frozen_root = read(&client, &root, expression).await;
    let frozen_popup = read(&client, &popup, expression).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(read(&client, &root, expression).await, frozen_root);
    assert_eq!(read(&client, &popup, expression).await, frozen_popup);
    generation.stop().await.unwrap();
}
