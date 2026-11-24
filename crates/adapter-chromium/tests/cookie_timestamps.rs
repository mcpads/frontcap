//! Exact native timestamps preserve SameSite age; ordinary setters provide the negative control.
mod support;
use cdp::protocol::{network, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

async fn native(g: &Generation) -> Session {
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}

#[tokio::test]
async fn restored_timestamps_preserve_cross_site_post_grace_and_readback() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let destination = format!("http://localhost:{port}/post");
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..3 {
            let request = server.recv_timeout(std::time::Duration::from_secs(40)).unwrap().expect("cross-site POST");
            let cookies = request.headers().iter().find(|h| h.field.equiv("Cookie")).map(|h|h.value.as_str()).unwrap_or("").to_owned();
            send.send((request.method().to_string(), cookies)).unwrap();
            request.respond(tiny_http::Response::from_string("<!doctype html>received")).unwrap();
        }
    });
    for (name, age_ms, preserve, expected) in [("young",30_000,true,"session=kept"),("aged",121_000,true,""),("reset",121_000,false,"session=kept")] {
        let runs = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1/source";
        let mock = |body: String| RuleSet::from_json(&json!([{"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]).to_string()).unwrap();
        let mut options = support::options(runs.path(), &format!("source-{name}"), url.into(), mock("<!doctype html>".into()));
        options.profile.initial_date_ms = 1_893_456_000_000;
        let mut source = Generation::launch(options).await.unwrap();
        let from = native(&source).await;
        let mut param = network::CookieParam {
            name: "session".into(), value: "kept".into(), url: Some(destination.clone()),
            domain: None, path: None, secure: None, http_only: None, same_site: None,
            expires: None, priority: None, source_scheme: None, source_port: None,
            partition_key: None, frontcap_timestamps: None,
        };
        from.call(&storage::SetCookiesParams::new(vec![param.clone()])).await.unwrap();
        source.advance(VirtualDuration::from_ms(age_ms)).await.unwrap();
        let before = source.now();
        let cookies = from.call(&storage::GetCookiesParams::default()).await.unwrap().cookies;
        assert_eq!(cookies.len(),1);
        let captured = cookies[0].frontcap_timestamps.clone().expect("native timestamp capture");
        let again = from.call(&storage::GetCookiesParams::default()).await.unwrap().cookies;
        assert_eq!(again[0].frontcap_timestamps.as_ref(),Some(&captured));
        assert_eq!(source.now(), before);
        let page = format!("<!doctype html><form method='POST' action='{destination}'></form><script>setTimeout(()=>document.forms[0].submit(),1)</script>");
        let mut options = support::options(runs.path(), &format!("restored-{name}"), url.into(), mock(page));
        options.profile.initial_date_ms = 1_893_456_000_000 + age_ms;
        options.passthrough_origins = Some(vec![format!("http://localhost:{port}")]);
        let mut restored = Generation::launch(options).await.unwrap();
        let to = native(&restored).await;
        if preserve { param.frontcap_timestamps = Some(captured.clone()); }
        to.call(&storage::SetCookiesParams::new(vec![param])).await.unwrap();
        let readback = to.call(&storage::GetCookiesParams::default()).await.unwrap().cookies;
        assert_eq!(readback.len(),1);
        if preserve { assert_eq!(readback[0].frontcap_timestamps.as_ref(),Some(&captured)); }
        else { assert_ne!(readback[0].frontcap_timestamps.as_ref(),Some(&captured)); }
        assert_eq!(restored.now().0,0);
        restored.advance(VirtualDuration::from_ms(2)).await.unwrap();
        let (method,cookie) = tokio::time::timeout(std::time::Duration::from_secs(5),receive.recv()).await.unwrap().unwrap();
        assert_eq!(method,"POST");
        assert_eq!(cookie,expected,"{name}");
        restored.stop().await.unwrap();
        source.stop().await.unwrap();
    }
    worker.join().unwrap();
}

#[tokio::test]
async fn live_recipe_timing_preserves_submillisecond_remaining_expiry() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://timing-page.test/start";
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "precise-source", url.into(), rules.clone());
    options.profile.initial_cookies = serde_json::from_value(json!([{
        "origin":"https://unvisited-timing.test","name":"precise","value":"kept","path":"/","same_site":"unspecified",
        "timing":{"creation_age_us":121000000,"last_access_age_us":120000000,"last_update_age_us":60000000,"remaining_lifetime_us":1501}
    }])).unwrap();
    let mut source = Generation::launch(options).await.unwrap();
    source.advance(VirtualDuration(1000)).await.unwrap();
    let live = source.live_state().await.unwrap();
    assert_eq!(live.cookies.len(),1);
    let timing = live.cookies[0].timing.as_ref().unwrap();
    assert_eq!(timing.creation_age_us,121001000);
    assert_eq!(timing.last_access_age_us,120001000);
    assert_eq!(timing.last_update_age_us,60001000);
    assert_eq!(timing.remaining_lifetime_us,Some(501));
    assert_eq!(source.now().0,1000);
    let mut options = support::options(runs.path(), "precise-restored", url.into(), rules);
    options.profile.initial_cookies = live.cookies;
    let mut restored = Generation::launch(options).await.unwrap();
    let to = native(&restored).await;
    restored.advance(VirtualDuration(500)).await.unwrap();
    assert_eq!(restored.now().0,500);
    assert_eq!(to.call(&storage::GetCookiesParams::default()).await.unwrap().cookies.len(),1);
    restored.advance(VirtualDuration(1)).await.unwrap();
    assert_eq!(restored.now().0,501);
    assert!(to.call(&storage::GetCookiesParams::default()).await.unwrap().cookies.is_empty());
    restored.stop().await.unwrap(); source.stop().await.unwrap();
}

#[tokio::test]
async fn restored_renewed_cookie_keeps_expiry_beyond_its_original_creation_cap() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://renewed-cookie.test/start";
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let day = 24 * 60 * 60 * 1_000_000u64;
    let mut options = support::options(runs.path(), "renewed-cookie", url.into(), rules);
    options.profile.initial_cookies = serde_json::from_value(json!([{
        "origin":"https://unvisited-renewed.test","name":"renewed","value":"kept","path":"/","same_site":"lax",
        "timing":{"creation_age_us":800*day,"last_access_age_us":2*day,"last_update_age_us":day,"remaining_lifetime_us":399*day}
    }])).unwrap();
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration(1000)).await.unwrap();
    let live = g.live_state().await.unwrap();
    assert_eq!(live.cookies.len(),1);
    let timing = live.cookies[0].timing.as_ref().unwrap();
    assert_eq!(timing.creation_age_us,800*day+1000);
    assert_eq!(timing.last_access_age_us,2*day+1000);
    assert_eq!(timing.last_update_age_us,day+1000);
    assert_eq!(timing.remaining_lifetime_us,Some(399*day-1000));
    g.stop().await.unwrap();
}
