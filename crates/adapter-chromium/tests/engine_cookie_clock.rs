//! Persistent cookie lifetime follows the generation across real renderer replacement.
mod support;
use cdp::protocol::{network, storage, system_info, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{action::Action, ids::RequestId, rules::RuleSet, time::VirtualDuration};
use std::collections::BTreeSet;

async fn renderers(client: &cdp::Client) -> BTreeSet<i64> {
    client.call(None, &system_info::GetProcessInfoParams::default()).await.unwrap()
        .process_info.into_iter().filter(|p| p.r#type == "renderer").map(|p| p.id).collect()
}

#[tokio::test]
async fn cookies_follow_the_clock_across_renderer_replacement() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let start = "https://cookie.example.test/start";
    let back = "https://cookie.example.test/back";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"start","match":{"url":start},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<script>setTimeout(()=>location.href='http://127.0.0.1:1/x',300)</script>"}},
        {"id":"back","match":{"url":back},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>back</p>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "cookie-renderer-handoff", start.into(), rules);
    options.passthrough_origins = Some(vec!["http://127.0.0.1:1".into()]);
    options.profile.initial_date_ms = 1_893_456_000_000;
    let mut g = Generation::launch(options).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    let native = Session::new(client.clone(), attached.session_id.0);
    let expiry = 1_893_456_002.0;
    let mut single = network::SetCookieParams::new("single".into(), "kept".into());
    single.url = Some(start.into()); single.path = Some("/".into());
    single.expires = Some(network::TimeSinceEpoch(expiry));
    assert!(native.call(&single).await.unwrap().success);
    let batch = network::CookieParam {
        frontcap_timestamps: None,
        name: "batch".into(), value: "kept".into(), url: Some(start.into()),
        domain: None, path: Some("/".into()), secure: None, http_only: None,
        same_site: None, expires: Some(network::TimeSinceEpoch(expiry)),
        priority: None, source_scheme: None, source_port: None, partition_key: None,
    };
    native.call(&storage::SetCookiesParams::new(vec![batch])).await.unwrap();
    g.eval_main("document.cookie='script=kept; Max-Age=2; Secure; Path=/'; true").await.unwrap();
    g.advance(VirtualDuration(10_000)).await.unwrap();
    let before = renderers(&client).await;
    assert!(!before.is_empty());
    g.advance(VirtualDuration(990_000)).await.unwrap();
    assert!(!renderers(&client).await.is_subset(&before), "error page must replace the renderer");
    let cookies = native.call(&storage::GetCookiesParams::default()).await.unwrap().cookies;
    assert_eq!(cookies.iter().map(|c| c.name.as_str()).collect::<BTreeSet<_>>(), BTreeSet::from(["batch", "script", "single"]));
    assert!(cookies.iter().all(|c| c.expires == expiry && c.value == "kept"));
    g.navigate_action(&RequestId::new("return-to-app").unwrap(), &Action::Navigate { url: back.into() }).await.unwrap();
    let visible = "document.cookie.split('; ').filter(Boolean).sort()";
    let expected = serde_json::json!(["batch=kept", "script=kept", "single=kept"]);
    assert_eq!(g.eval_main(visible).await.unwrap(), expected);
    g.advance(VirtualDuration(999_999)).await.unwrap();
    assert_eq!(g.eval_main(visible).await.unwrap(), expected, "visible just before expiry after returning to the app");
    g.advance(VirtualDuration(1)).await.unwrap();
    assert_eq!(g.eval_main(visible).await.unwrap(), serde_json::json!([]));
    assert!(native.call(&storage::GetCookiesParams::default()).await.unwrap().cookies.is_empty());
    g.stop().await.unwrap();
}

async fn attach(client: &cdp::Client, id: target::TargetID) -> Session {
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(id)
    }).await.unwrap();
    Session::new(client.clone(), attached.session_id.0)
}

#[tokio::test]
async fn independent_contexts_keep_distinct_cookie_clocks_and_competing_tabs_are_refused() {
    use cdp::protocol::emulation;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://cookie.example.test/";
    let rules = RuleSet::from_json(r#"[{"id":"page","match":{"url":"https://cookie.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>owner</p>"}}]"#).unwrap();
    let mut options = support::options(runs.path(), "cookie-clock-owners", url.into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.profile.initial_date_ms = 1_893_456_000_000;
    let mut g = Generation::launch(options).await.unwrap();
    g.eval_main("document.cookie='first=kept; Max-Age=1; Secure; Path=/';true").await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let contender = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap();
    let competing = attach(&client, contender.target_id.clone()).await;
    let pause = emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(network::TimeSinceEpoch(2_051_222_400.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    };
    assert!(competing.call(&pause).await.is_err(), "two live tabs cannot drive the same store");
    g.advance(VirtualDuration(10_000)).await.unwrap();
    assert_eq!(g.eval_main("document.cookie").await.unwrap(), serde_json::json!("first=kept"));
    client.call(None, &target::CloseTargetParams::new(contender.target_id)).await.unwrap();
    let context = client.call(None, &target::CreateBrowserContextParams {
        dispose_on_detach: Some(true), ..Default::default()
    }).await.unwrap().browser_context_id;
    let created = client.call(None, &target::CreateTargetParams {
        browser_context_id: Some(context.clone()),
        ..target::CreateTargetParams::new("about:blank".into())
    }).await.unwrap();
    let independent = attach(&client, created.target_id).await;
    independent.call(&pause).await.unwrap();
    let mut cookie = network::SetCookieParams::new("second".into(), "kept".into());
    cookie.url = Some(url.into());
    cookie.expires = Some(network::TimeSinceEpoch(2_051_222_401.0));
    assert!(independent.call(&cookie).await.unwrap().success);
    let inventory = storage::GetCookiesParams { browser_context_id: Some(context) };
    let cookies = client.call(None, &inventory).await.unwrap().cookies;
    assert_eq!(cookies.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["second"]);
    g.advance(VirtualDuration(10_000)).await.unwrap();
    assert_eq!(independent.eval("Date.now()", None).await.unwrap(), serde_json::json!(2_051_222_400_000u64));
    let mut expired = independent.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>();
    independent.call(&emulation::SetVirtualTimePolicyParams {
        budget: Some(1000.0),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    }).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), expired.next()).await.unwrap().unwrap();
    assert!(client.call(None, &inventory).await.unwrap().cookies.is_empty());
    assert_eq!(g.eval_main("({cookie:document.cookie,now:Date.now()})").await.unwrap(),
               serde_json::json!({"cookie":"first=kept","now":1_893_456_000_020u64}));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn http_only_relative_expiry_uses_response_time_and_filters_request_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !support::controlled_engine_or_check_baseline().await { return; }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (requests, mut received_requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 { return; }
                    request.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let received = request.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("cookie").then(|| value.trim())
                }).unwrap_or("");
                requests.send((path.to_owned(), received.to_owned())).unwrap();
                let cookie = match path {
                    "/start" => "Set-Cookie: first=kept; Max-Age=2; HttpOnly; Path=/\r\n",
                    "/later" => "Set-Cookie: later=kept; Max-Age=2; HttpOnly; Path=/\r\n",
                    _ => "",
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{received}", received.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "http-cookie-expiry", format!("{origin}/start"), RuleSet::default());
    options.profile.initial_date_ms = 1_893_456_000_000;
    options.passthrough_origins = Some(vec![origin.clone()]);
    let mut g = Generation::launch(options).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let native = attach(&client, target::TargetID(endpoint.target_id)).await;
    g.advance(VirtualDuration(1_000_000)).await.unwrap();
    g.navigate_action(&RequestId::new("later-cookie").unwrap(), &Action::Navigate { url: format!("{origin}/later") }).await.unwrap();
    assert_eq!(received_cookie(&mut received_requests, "/later").await, "first=kept");
    assert_eq!(g.eval_main("document.cookie").await.unwrap(), serde_json::json!(""), "HttpOnly must remain hidden");
    let cookies = native.call(&storage::GetCookiesParams::default()).await.unwrap().cookies;
    assert_eq!(cookies.len(), 2);
    for cookie in cookies {
        assert!(cookie.http_only);
        assert_eq!(cookie.expires, if cookie.name == "first" { 1_893_456_002.0 } else { 1_893_456_003.0 });
    }
    for (id, delta, expected) in [("before", 999_999, "first=kept; later=kept"), ("first-expired", 1, "later=kept"), ("both-expired", 1_000_000, "")] {
        g.advance(VirtualDuration(delta)).await.unwrap();
        g.navigate_action(&RequestId::new(id).unwrap(), &Action::Navigate { url: format!("{origin}/echo?{id}") }).await.unwrap();
        assert_eq!(received_cookie(&mut received_requests, &format!("/echo?{id}")).await, expected, "{id}");
    }
    assert!(native.call(&storage::GetCookiesParams::default()).await.unwrap().cookies.is_empty());
    g.stop().await.unwrap();
    server.abort();
}

async fn received_cookie(requests: &mut tokio::sync::mpsc::UnboundedReceiver<(String, String)>, path: &str) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (received_path, cookie) = requests.recv().await.unwrap();
            if received_path == path { return cookie; }
        }
    }).await.unwrap()
}

#[tokio::test]
async fn same_site_unsafe_post_grace_uses_controlled_cookie_age() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let destination = format!("http://localhost:{port}/post");
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..3 {
            let request = server.recv_timeout(std::time::Duration::from_secs(40)).unwrap().expect("cross-site POST");
            let cookie = request.headers().iter().find(|h| h.field.equiv("Cookie")).map(|h| h.value.as_str()).unwrap_or("").to_owned();
            requests.send((request.method().to_string(), cookie)).unwrap();
            request.respond(tiny_http::Response::from_string("<!doctype html><p>received</p>")).unwrap();
        }
    });
    for (name, delay_ms, expected) in [("before",119_999,"default=kept"), ("at",120_000,"default=kept"), ("after",120_001,"")] {
        let runs = tempfile::tempdir().unwrap();
        // 127.0.0.1 and localhost are different sites. Explicit Lax is a negative
        // control; only an unspecified SameSite cookie has the unsafe-POST grace.
        let source = "http://127.0.0.1/start";
        let body = format!("<!doctype html><form method='POST' action='{destination}'></form><script>setTimeout(()=>document.forms[0].submit(),{delay_ms})</script>");
        let rules = RuleSet::from_json(&serde_json::json!([{"id":"source","match":{"url":source},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]).to_string()).unwrap();
        let mut options = support::options(runs.path(), &format!("same-site-{name}"), source.into(), rules);
        options.profile.initial_date_ms = 1_893_456_000_000;
        options.passthrough_origins = Some(vec![format!("http://localhost:{port}")]);
        let mut g = Generation::launch(options).await.unwrap();
        let endpoint = g.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
        let native = attach(&client, target::TargetID(endpoint.target_id)).await;
        for (cookie_name, same_site) in [("default", None), ("explicit", Some(network::CookieSameSite::Lax))] {
            let mut cookie = network::SetCookieParams::new(cookie_name.into(), "kept".into());
            cookie.url = Some(destination.clone());
            cookie.same_site = same_site;
            assert!(native.call(&cookie).await.unwrap().success);
        }
        g.advance(VirtualDuration((delay_ms + 1) * 1000)).await.unwrap();
        let (method, cookies) = tokio::time::timeout(std::time::Duration::from_secs(5), received.recv()).await.unwrap().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(cookies, expected, "{name} the two-minute threshold");
        g.stop().await.unwrap();
    }
    worker.join().unwrap();
}
