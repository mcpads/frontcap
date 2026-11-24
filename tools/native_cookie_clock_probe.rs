mod support;
use cdp::protocol::{network, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn native_cookie_epoch_diagnostic() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://cookie.example.test/";
    let rules = RuleSet::from_json(r#"[{"id":"page","match":{"url":"https://cookie.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>native cookie clock</p>"}}]"#).unwrap();
    let mut options = support::options(runs.path(), "native-cookie-clock", url.into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.profile.initial_date_ms = 1_893_456_000_000; // 2030-01-01, beyond the host-based lifetime cap.
    let mut g = Generation::launch(options).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let expires = network::TimeSinceEpoch(1_893_456_001.0);
    let mut single = network::SetCookieParams::new("single".into(), "kept".into());
    single.url = Some(url.into()); single.expires = Some(expires.clone());
    let set = native.call(&single).await.unwrap();
    let batch = network::CookieParam {
        name: "batch".into(), value: "kept".into(), url: Some(url.into()),
        domain: None, path: None, secure: None, http_only: None, same_site: None,
        expires: Some(expires), priority: None, source_scheme: None,
        source_port: None, partition_key: None,
    };
    native.call(&storage::SetCookiesParams::new(vec![batch])).await.unwrap();
    g.eval_main("document.cookie='script=kept; Max-Age=1; Secure; Path=/';true").await.unwrap();
    let before = native.call(&storage::GetCookiesParams::default()).await.unwrap();
    let page_before = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    g.advance(VirtualDuration(1_000_000)).await.unwrap();
    let after = native.call(&storage::GetCookiesParams::default()).await.unwrap();
    println!("NATIVE_COOKIE_CLOCK_PROBE {}", serde_json::json!({"set":set,"before":before,"page_before":page_before,"after_exact_expiry":after}));
    g.stop().await.unwrap();
}
