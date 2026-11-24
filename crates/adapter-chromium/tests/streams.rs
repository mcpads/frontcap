//! Streams the platform scripts (`stream_mock`): every event delivered by the platform at its instant,
//! while virtual time is paused there (time-model.md, Delivery barrier), timed from the instant the page
//! issued the handshake.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

fn rules() -> RuleSet {
    RuleSet::from_json(
        r#"[{"id":"feed","match":{"url":"https://api.example.test/feed*"},"action":{"type":"stream_mock","messages":[
              {"at_ms":100,"data":"a","id":"7"},{"at_ms":250,"data":"b"}]}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock","messages":[
              {"at_ms":50,"data":"ping"}],"close_at_ms":300}}]"#,
    )
    .unwrap()
}

async fn launch(addr: std::net::SocketAddr, name: &str, page: &str, rules: RuleSet) -> (Generation, tempfile::TempDir) {
    let runs = tempfile::tempdir().unwrap();
    let g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/{page}"), rules)).await.unwrap();
    (g, runs)
}

async fn log(g: &Generation) -> Vec<serde_json::Value> {
    serde_json::from_str(g.eval_main("JSON.stringify(log)").await.unwrap().as_str().unwrap()).unwrap()
}

fn trace(g: &Generation) -> String {
    std::fs::read_to_string(g.trace_path()).unwrap()
}

#[tokio::test]
async fn scripted_streams_deliver_each_event_at_its_instant() {
    let (mut g, _runs) = launch(support::serve(), "st-1", "stream.html", RuleSet::from_json(
        r#"[{"id":"feed","match":{"url":"https://api.example.test/feed"},"action":{"type":"stream_mock","messages":[
              {"at_ms":100,"data":"a"},{"at_ms":250,"data":"b","event":"tick","id":"7"}]}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock","messages":[
              {"at_ms":50,"data":"ping"}],"close_at_ms":300}}]"#).unwrap()).await;
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    // Issued at 0 during the load. On the patched engine the document's completion step waits while the
    // handshakes count (engine patch 1): the streams open at their issue. On the baseline they reach the
    // platform only after the engine moved on: the opens are late (and say so). Everything scripted is at
    // its instant.
    let patched = browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some();
    let open = if patched { 0 } else { 10 };
    assert_eq!(serde_json::Value::Array(log(&g).await), serde_json::json!([
        ["sse-open", open], ["ws-open", open], ["ws", "ping", 50], ["sse", "a", 100], ["sse-tick", "b", "7", 250], ["ws-close", 1000, 300]
    ]));
    let t = trace(&g);
    assert_eq!(t.matches("\"late_delivery\"").count(), if patched { 0 } else { 2 }, "{t}");
    let outs: Vec<&str> = t.lines().filter(|l| l.contains("\"direction\":\"out\"")).collect();
    assert_eq!(outs.len(), 2, "hello and pong, as the page sent them: {outs:?}");
    assert!(outs[0].contains("\"data\":\"hello\"") && outs[1].contains("\"data\":\"pong\""));
}

/// Opened mid-run, a stream opens at its issue, and its script does not depend on how virtual time is
/// advanced (one advance, or many short ones).
#[tokio::test]
async fn a_streams_instants_do_not_depend_on_how_time_is_advanced() {
    let addr = support::serve();
    let mut logs = Vec::new();
    for (i, slices) in [1u64, 60, 300].into_iter().enumerate() {
        let (mut g, _runs) = launch(addr, &format!("st-s{i}"), "stream2.html", rules()).await;
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        g.eval_main("openLater(17)").await.unwrap();
        for _ in 0..slices {
            g.advance(VirtualDuration::from_ms(600 / slices)).await.unwrap();
        }
        assert!(!trace(&g).contains("late_delivery"), "{}", trace(&g));
        logs.push(log(&g).await);
    }
    assert_eq!(logs[0], logs[1]);
    assert_eq!(logs[0], logs[2]);
    let open = logs[0].iter().find(|e| e[0] == "sse-open").unwrap()[1].as_i64().unwrap();
    assert_eq!(logs[0].iter().find(|e| e[0] == "sse").unwrap()[3], open + 100, "{:?}", logs[0]);
}

/// Criterion 9's scripted half: five runs with equal behavior projections.
#[tokio::test]
async fn scripted_streams_repeat_exactly() {
    let addr = support::serve();
    let mut traces = Vec::new();
    for i in 0..5 {
        let (mut g, _runs) = launch(addr, &format!("st-r{i}"), "stream2.html?open", rules()).await;
        g.advance(VirtualDuration::from_ms(500)).await.unwrap();
        traces.push(trace(&g));
    }
    for t in &traces[1..] {
        assert_eq!(frontcap_core::projection::compare_behavior(&traces[0], t), Ok(()));
    }
}

/// The URL as the page wrote it reaches the rules (`feed*` with `?topics=a,b&x`); `lastEventId` stays
/// with the stream; a server close makes an EventSource reconnect, which opens the script again.
#[tokio::test]
async fn event_source_keeps_its_last_id_and_reconnects() {
    let addr = support::serve();
    let rules = RuleSet::from_json(
        r#"[{"id":"feed","match":{"url":"https://api.example.test/feed?topics=a,b&x"},"action":{"type":"stream_mock","messages":[
              {"at_ms":10,"data":"1","id":"7"},{"at_ms":20,"data":"2"}],"close_at_ms":30}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock"}}]"#,
    )
    .unwrap();
    let (mut g, _runs) = launch(addr, "st-e", "stream2.html", rules).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    g.advance(VirtualDuration::from_ms(3100)).await.unwrap();
    let names: Vec<String> = log(&g).await.iter().filter(|e| e[0].as_str().unwrap().starts_with("sse")).map(|e| e.to_string()).collect();
    assert!(names[1].contains("\"1\",\"7\"") && names[2].contains("\"2\",\"7\""), "the last id stays: {names:?}");
    assert!(names[3].starts_with("[\"sse-error\",0,"), "reconnecting after the server's close: {names:?}");
    assert!(names[4].starts_with("[\"sse-open\""), "reopened after the retry delay: {names:?}");
}

/// The page's own close: a WebSocket closes cleanly with its code; closed while connecting it fails.
#[tokio::test]
async fn websocket_close_by_the_page_is_reported_as_a_browser_would() {
    let addr = support::serve();
    let (mut g, _runs) = launch(addr, "st-w", "stream2.html", rules()).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    g.advance(VirtualDuration::from_ms(30)).await.unwrap();
    g.eval_main("ws.close(4000, 'bye'); 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let l = log(&g).await;
    let ws: Vec<&serde_json::Value> = l.iter().filter(|e| e[0].as_str().unwrap().starts_with("ws")).collect();
    assert_eq!(ws.len(), 2, "open, then the page's close — no scripted ping or close after it: {ws:?}");
    assert_eq!((&ws[1][1], &ws[1][2]), (&serde_json::json!(4000), &serde_json::json!(true)));
    // Closed before it opened.
    g.eval_main("openAll(); ws.close(); 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let l = log(&g).await;
    let tail: Vec<String> = l.iter().rev().filter(|e| e[0].as_str().unwrap().starts_with("ws")).take(2).map(|e| e[0].to_string()).collect();
    assert_eq!(tail, ["\"ws-close\"", "\"ws-error\""], "{l:?}");
}

/// Navigated top and child documents receive their own streams.
#[tokio::test]
async fn streams_belong_to_their_navigated_top_and_child_documents() {
    let addr = support::serve();
    let (mut g, _runs) = launch(addr, "st-n", "form.html", rules()).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    let to = frontcap_core::action::Action::Navigate { url: format!("http://{addr}/stream2.html?open&frame") };
    g.navigate_action(&frontcap_core::ids::RequestId::new("n-1").unwrap(), &to).await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let l = log(&g).await;
    assert!(l.iter().any(|e| e[0] == "sse" && e[1] == "b"), "the 250 ms message reached the new document: {l:?}");
    assert!(l.iter().any(|e| e[0] == "child-message" && e[1] == "b"), "{l:?}");
    let t = trace(&g);
    assert!(t.contains("\"document\":2") && !t.contains("child frame"), "{t}");
}

/// A stream URL a non-stream rule matches fails, with the reason; so does a failing handshake.
#[tokio::test]
async fn streams_without_a_stream_rule_or_with_a_failed_handshake_fail() {
    let addr = support::serve();
    let rules = RuleSet::from_json(
        r#"[{"id":"feed","match":{"url":"https://api.example.test/feed*"},"action":{"type":"mock","status":200}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock","handshake":{"status":403}}}]"#,
    )
    .unwrap();
    let (mut g, _runs) = launch(addr, "st-u", "stream2.html", rules).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let l: Vec<String> = log(&g).await.iter().map(|e| e[0].to_string()).collect();
    assert_eq!(l, ["\"sse-error\"", "\"ws-error\"", "\"ws-close\""], "{l:?}");
    assert!(trace(&g).contains("\"unsupported_stream\""));
}

/// An open stream that says nothing is no activity: a settle ends, and timers run.
#[tokio::test]
async fn a_silent_stream_does_not_hold_quiescence_or_timers() {
    let silent = RuleSet::from_json(
        r#"[{"id":"feed","match":{"url":"https://api.example.test/feed"},"action":{"type":"stream_mock","messages":[{"at_ms":60000,"data":"late"}]}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock"}}]"#,
    )
    .unwrap();
    let (mut g, _runs) = launch(support::serve(), "st-q", "stream.html", silent).await;
    let settled = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(5000)).await.unwrap();
    assert!(matches!(settled, frontcap_core::clock::SettleOutcome::Quiescent { .. }), "{settled:?}");
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert_eq!(g.eval_main("timerFired").await.unwrap(), serde_json::json!(true));
}

/// A relaunched recorder goes on with a stream's script where the page's wrapper left it: each event
/// once, the rest at their instants.
#[tokio::test]
async fn a_reconnected_generation_continues_a_streams_script() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut opts = support::options(runs.path(), "st-rc", format!("http://{addr}/stream2.html?open"), rules());
    profile_dir.apply(&mut opts);
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(150)).await.unwrap();
    let (endpoint, launch) = (g.endpoint(), g.launch_id().clone());
    g.abandon();
    let reconnect = frontcap_chromium::generation::ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: frontcap_core::ids::RunId::new("st-rc").unwrap(),
        launch_id: launch,
        profile: support::options(runs.path(), "st-rc", String::new(), RuleSet::default()).profile,
        endpoint,
        watches: Default::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(),
        upstream: None,
    };
    let (mut g, _) = Generation::reconnect(reconnect).await.unwrap();
    g.advance(VirtualDuration::from_ms(350)).await.unwrap();
    let l = log(&g).await;
    let names: Vec<String> = l.iter().map(|e| e[0].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["sse-open", "ws-open", "ws", "sse", "sse", "ws-close"], "{l:?}");
    assert_eq!(l[4][3], 250, "a message due after the fault at its instant");
    g.stop().await.unwrap();
}

/// Opened while another request's latency holds virtual time in segments, the handshake reaches the
/// platform late: the open is delivered late and says so, and every scripted event keeps its instant
/// from the issue — the same for any slicing of time.
#[tokio::test]
async fn a_stream_opened_under_a_held_request_keeps_its_script() {
    let addr = support::serve();
    let mut rules = rules();
    rules.rules.insert(0, serde_json::from_str(r#"{"id":"slow","match":{"url":"https://api.example.test/slow"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"latency_ms":200}}"#).unwrap());
    let mut messages = Vec::new();
    for (i, slices) in [1u64, 120].into_iter().enumerate() {
        let (mut g, _runs) = launch(addr, &format!("st-h{i}"), "stream2.html", rules.clone()).await;
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        g.eval_main("fetch('https://api.example.test/slow'); openLater(17)").await.unwrap();
        for _ in 0..slices {
            g.advance(VirtualDuration::from_ms(600 / slices)).await.unwrap();
        }
        let l = log(&g).await;
        messages.push(l.iter().filter(|e| e[0] == "sse" || e[0] == "ws" || e[0] == "ws-close").cloned().collect::<Vec<_>>());
    }
    assert_eq!(messages[0], messages[1], "the script does not move with the slicing");
    assert_eq!(messages[0][0], serde_json::json!(["ws", "ping", 87]), "issued at 37, ping at +50: {:?}", messages[0]);
}

/// A frame that opens a stream and removes itself in that task leaves one failed request at the stream's
/// issue, and its sibling's stream runs on, however late the browser reports the removed stream's anchor.
#[tokio::test]
async fn a_stream_whose_frame_goes_in_its_opening_task_is_abandoned_however_late_its_anchor_arrives() {
    for close in [false, true] {
        let vanishing = format!("<!doctype html><script>setTimeout(()=>{{const s=new WebSocket('wss://api.example.test/removed');{}frameElement.remove()}},250)</script>",
            if close { "s.close();" } else { "" });
        let rules = RuleSet::from_json(&serde_json::json!([
            {"id": "page", "match": {"url": "https://abandon.example.test/"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"},
             "body": "<!doctype html><output id='result'>waiting</output><iframe src='/vanishing'></iframe><iframe src='/living'></iframe>"}},
            {"id": "vanishing", "match": {"url": "https://abandon.example.test/vanishing"}, "action": {"type": "mock", "status": 200,
             "headers": {"content-type": "text/html"}, "body": vanishing}},
            {"id": "living", "match": {"url": "https://abandon.example.test/living"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"},
             "body": "<!doctype html><script>setTimeout(()=>{const s=new WebSocket('wss://api.example.test/living');s.onmessage=e=>parent.document.querySelector('#result').textContent=e.data},250)</script>"}},
            {"id": "streams", "match": {"url": "wss://api.example.test/*"}, "action": {"type": "stream_mock", "messages": [{"at_ms": 100, "data": "alive"}]}}
        ]).to_string()).unwrap();
        let mut traces = Vec::new();
        for late in [false, true] {
            let runs = tempfile::tempdir().unwrap();
            let mut g = Generation::launch(support::options(runs.path(), "abandoned", "https://abandon.example.test/".into(), rules.clone())).await.unwrap();
            if late {
                g.delay_interceptions("api.example.test/removed", std::time::Duration::from_millis(500));
            }
            g.advance(VirtualDuration::from_ms(500)).await.unwrap();
            // The late interception has arrived by the next advance, which does not record it again.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            g.advance(VirtualDuration::from_ms(100)).await.unwrap();
            assert_eq!(g.eval_main("document.querySelector('#result').textContent").await.unwrap(), "alive");
            let text = trace(&g);
            let events: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            let removed: Vec<_> = events.iter().filter(|e| e["kind"] == "request" && e["payload"]["url"] == "wss://api.example.test/removed").collect();
            assert_eq!(removed.len(), 1, "late {late}, close {close}:\n{text}");
            let id = &removed[0]["payload"]["id"];
            let failed: Vec<_> = events.iter().filter(|e| e["kind"] == "response" && e["payload"]["id"] == *id).collect();
            assert_eq!(failed.len(), 1, "late {late}, close {close}:\n{text}");
            assert_eq!((&removed[0]["t_us"], &failed[0]["t_us"], &failed[0]["payload"]["status"]), (&250_000.into(), &250_000.into(), &0.into()), "{text}");
            let of_removed = |event: &str, direction: &str| events.iter().filter(|e| e["kind"] == "stream" && e["payload"]["stream"] == *id
                && e["payload"]["event"] == event && e["payload"]["direction"] == direction).count();
            assert_eq!((of_removed("open", "in"), of_removed("close", "out")), (0, usize::from(close)), "late {late}:\n{text}");
            traces.push(text);
        }
        frontcap_core::projection::compare_behavior(&traces[0], &traces[1])
            .unwrap_or_else(|d| panic!("close {close}: {d:?}\n{}\n---\n{}", traces[0], traces[1]));
    }
}
