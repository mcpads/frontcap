//! Criterion 9, pass-through half: a stream from a real server is opened by the platform; what the
//! server sends is delivered at slots on the frame interval inside advancing operations — never while
//! frozen — and a recording of it replays at the same slots.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_core::clock::{frame_slot, frame_tick};
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FRAME: VirtualDuration = VirtualDuration(16_667);

/// A slot: a frame tick (a multiple of the frame interval, rounded up to a whole millisecond).
fn at_slot(t_us: u64) -> bool {
    frame_tick(frame_slot(VirtualTime(t_us), FRAME), FRAME).0 == t_us
}

/// An event-stream server: `m1`, `m2`, `m3` (each its own id) after 50, 250, and 400 ms of wall time,
/// then silence. It keeps each connection's `Last-Event-ID`.
async fn sse_server() -> std::net::SocketAddr {
    sse_server_seen().await.0
}

async fn sse_server_seen() -> (std::net::SocketAddr, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = kept.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    seen.lock().unwrap().push(req.headers().get("last-event-id").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()));
                    async move {
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>>(8);
                    tokio::spawn(async move {
                        for (ms, m) in [(50, "m1"), (200, "m2"), (150, "m3")] {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            if tx.send(Ok(hyper::body::Frame::data(bytes::Bytes::from(format!("id: {m}\ndata: {m}\n\n"))))).await.is_err() {
                                return;
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    });
                    let body = http_body_util::StreamBody::new(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|f| (f, rx)) }));
                    let mut r = hyper::Response::new(body);
                    r.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                    Ok::<_, std::convert::Infallible>(r)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
            });
        }
    });
    (addr, seen)
}

/// A WebSocket server that echoes what it receives.
async fn ws_echo() -> std::net::SocketAddr {
    use futures_util::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
                while let Some(Ok(m)) = ws.next().await {
                    if m.is_text() && ws.send(m).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn launch(name: &str, query: &str, rules: RuleSet, origins: Vec<String>) -> (Generation, tempfile::TempDir) {
    let runs = tempfile::tempdir().unwrap();
    let g = launch_in(&runs, name, query, rules, origins, None).await;
    (g, runs)
}

async fn launch_in(runs: &tempfile::TempDir, name: &str, query: &str, rules: RuleSet, origins: Vec<String>, profile_dir: Option<&support::DetachedProfile>) -> Generation {
    let addr = support::serve();
    let mut o = support::options(runs.path(), name, format!("http://{addr}/passthrough.html?{query}"), rules);
    if let Some(p) = profile_dir {
        p.apply(&mut o);
    }
    let mut origins = origins;
    origins.push(format!("http://{addr}"));
    o.passthrough_origins = Some(origins);
    o.upstream = Some(frontcap_proxy::upstream::Upstream::new(vec![]).unwrap());
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g
}

/// The instants the trace records stream messages delivered at.
fn delivered_at(g: &Generation) -> Vec<u64> {
    std::fs::read_to_string(g.trace_path()).unwrap().lines().filter(|l| l.contains("\"direction\":\"in\",\"event\":\"message\"")).map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["t_us"].as_u64().unwrap()).collect()
}

/// Each delivered message's instant from its stream's open, from the trace.
fn offsets(g: &Generation) -> Vec<u64> {
    let t = std::fs::read_to_string(g.trace_path()).unwrap();
    let v: Vec<serde_json::Value> = t.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let open = v.iter().find(|e| e["kind"] == "stream" && e["payload"]["event"] == "open").unwrap()["t_us"].as_u64().unwrap();
    v.iter().filter(|e| e["kind"] == "stream" && e["payload"]["event"] == "message" && e["payload"]["direction"] == "in").map(|e| e["t_us"].as_u64().unwrap() - open).collect()
}

async fn log(g: &Generation) -> Vec<serde_json::Value> {
    serde_json::from_str(g.eval_main("JSON.stringify(log)").await.unwrap().as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn a_passed_through_stream_is_delivered_at_slots_and_never_while_frozen() {
    let server = sse_server().await;
    let (mut g, _runs) = launch("pt-1", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(log(&g).await.len(), 1, "frozen: only the open, though the server has sent everything");
    let before = g.now();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let l = log(&g).await;
    let messages: Vec<&str> = l.iter().filter(|e| e[0] == "sse").map(|e| e[1].as_str().unwrap()).collect();
    assert_eq!(messages, ["m1", "m2", "m3"], "{l:?}");
    let first = frame_tick(frame_slot(before, FRAME) + 1, FRAME).0;
    assert_eq!(delivered_at(&g), [first, first, first], "all three at the first slot of the advance, after {before:?}");
    assert!(std::fs::read_to_string(g.trace_path()).unwrap().contains("\"passthrough_content\""));
}

#[tokio::test]
async fn a_message_arriving_during_an_until_is_delivered_within_it() {
    let server = sse_server().await;
    let (mut g, _runs) = launch("pt-2", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    let name = frontcap_core::ids::WatchName::new("m3").unwrap();
    g.watch_add(name.clone(), Predicate::TextEquals { target: Locator::Css { selector: "#last".into() }, text: "m3".into() }).await.unwrap();
    let outcome = g.until(&name, VirtualDuration::from_ms(60_000)).await.unwrap();
    assert!(format!("{outcome:?}").contains("ConditionMet"), "{outcome:?}");
    assert!(at_slot(g.now().0), "the condition met at the slot that delivered m3: {:?}", g.now());
}

#[tokio::test]
async fn a_recorded_stream_replays_at_its_recorded_slots() {
    let server = sse_server().await;
    let origin = format!("http://{server}");
    let record = RuleSet::from_json(&format!(r#"[{{"id":"feed","match":{{"url":"{origin}/events"}},"action":{{"type":"record","cassette":"feed"}}}}]"#)).unwrap();
    let (mut g, runs) = launch("pt-3", &format!("sse={origin}/events"), record, vec![]).await;
    for _ in 0..40 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let recorded = log(&g).await;
    assert_eq!(recorded.iter().filter(|e| e[0] == "sse").count(), 3, "{recorded:?}");
    let cassette = runs.path().join("pt-3/cassettes/feed.jsonl");
    let kept = std::env::temp_dir().join(format!("feed-{}.jsonl", std::process::id()));
    std::fs::copy(&cassette, &kept).unwrap();
    // The server is not there for the replay.
    let replay = RuleSet::from_json(&format!(r#"[{{"id":"feed","match":{{"url":"{origin}/events"}},"action":{{"type":"replay","cassette":{:?}}}}}]"#, kept.display().to_string())).unwrap();
    let (mut h, _runs2) = launch("pt-3r", &format!("sse={origin}/events"), replay, vec![]).await;
    h.advance(VirtualDuration::from_ms(2000)).await.unwrap();
    std::fs::remove_file(&kept).unwrap();
    let data = |l: Vec<serde_json::Value>| l.into_iter().filter(|e| e[0] == "sse").map(|e| e[1].clone()).collect::<Vec<_>>();
    assert_eq!(data(log(&h).await), data(recorded));
    assert_eq!(offsets(&h), offsets(&g), "the recorded slots, exactly");
}

#[tokio::test]
async fn a_passed_through_websocket_echoes_what_the_page_sends() {
    let server = ws_echo().await;
    let (mut g, _runs) = launch("pt-4", &format!("ws=ws://{server}/echo"), RuleSet::default(), vec![format!("ws://{server}")]).await;
    for _ in 0..20 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        if log(&g).await.iter().any(|e| e[0] == "ws") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let l = log(&g).await;
    let echo = l.iter().find(|e| e[0] == "ws").unwrap_or_else(|| panic!("{l:?}"));
    assert_eq!(echo[1], "hi");
    assert!(delivered_at(&g).iter().all(|t| at_slot(*t)), "{:?}", delivered_at(&g));
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(trace.contains("\"direction\":\"out\",\"event\":\"message\""), "{trace}");
}

#[tokio::test]
async fn a_silent_passed_through_stream_holds_neither_quiescence_nor_timers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|_req| async move {
                    let (_tx, rx) = tokio::sync::mpsc::channel::<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>>(1);
                    let body = http_body_util::StreamBody::new(futures_util::stream::unfold((rx, _tx), |(mut rx, tx)| async move { rx.recv().await.map(|f| (f, (rx, tx))) }));
                    let mut r = hyper::Response::new(body);
                    r.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                    Ok::<_, std::convert::Infallible>(r)
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
            });
        }
    });
    let (mut g, _runs) = launch("pt-5", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    let settled = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(5000)).await.unwrap();
    assert!(matches!(settled, frontcap_core::clock::SettleOutcome::Quiescent { .. }), "{settled:?}");
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert_eq!(g.eval_main("timerFired").await.unwrap(), serde_json::json!(true));
}

/// Advancing in steps shorter than a frame with a stream open: every step ends, and the messages land on
/// frame ticks, whatever the slicing.
#[tokio::test]
async fn small_steps_with_a_stream_open_reach_every_goal_and_keep_the_slots() {
    let server = sse_server().await;
    let (mut g, _runs) = launch("pt-6", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    for step in [1u64, 2, 5, 16, 17] {
        for _ in 0..30 {
            let before = g.now();
            g.advance(VirtualDuration::from_ms(step)).await.unwrap();
            assert_eq!(g.now().0, before.0 + step * 1000, "a step of {step} ms");
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // The server sends its last message 400 ms (wall) after the connection: small steps go on until it came.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while log(&g).await.iter().filter(|e| e[0] == "sse").count() < 3 && std::time::Instant::now() < deadline {
        let before = g.now();
        g.advance(VirtualDuration::from_ms(3)).await.unwrap();
        assert_eq!(g.now().0, before.0 + 3000, "a step of 3 ms");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let l = log(&g).await;
    assert_eq!(l.iter().filter(|e| e[0] == "sse").count(), 3, "{l:?}");
    assert!(delivered_at(&g).iter().all(|t| at_slot(*t)), "{:?}", delivered_at(&g));
}

/// A server that accepts the connection and never answers: the stream fails within half the watchdog,
/// and the generation goes on.
#[tokio::test]
async fn a_server_that_never_answers_fails_the_stream_not_the_generation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let (mut g, _runs) = launch("pt-7", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let l = log(&g).await;
    assert!(l.iter().any(|e| e[0] == "sse-error") && !l.iter().any(|e| e[0] == "sse-open"), "{l:?}");
    assert!(std::fs::read_to_string(g.trace_path()).unwrap().contains("did not answer within"));
}

/// An EventSource the server ended reconnects with the last event id it received, as a browser's does.
#[tokio::test]
async fn a_reconnecting_event_source_sends_its_last_event_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = kept.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    seen.lock().unwrap().push(req.headers().get("last-event-id").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()));
                    async move {
                        let mut r = hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from("id: 칠\ndata: a\n\n")));
                        r.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
                        Ok::<_, std::convert::Infallible>(r)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
            });
        }
    });
    let (mut g, _runs) = launch("pt-8", &format!("sse=http://{server}/events"), RuleSet::default(), vec![format!("http://{server}")]).await;
    for _ in 0..10 {
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        if seen.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[..2], [None, Some("칠".to_string())], "{seen:?}");
}

/// A recorder relaunch loses the platform's connection: the page is told, its EventSource reconnects as a
/// new session numbered after the recorded ones, and the recording (the loss included) replays the same.
#[tokio::test]
async fn a_relaunch_during_a_recorded_stream_is_recorded_and_replayed() {
    let (server, seen) = sse_server_seen().await;
    let origin = format!("http://{server}");
    let record = RuleSet::from_json(&format!(r#"[{{"id":"feed","match":{{"url":"{origin}/events"}},"action":{{"type":"record","cassette":"feed"}}}}]"#)).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launch_in(&runs, "pt-9", &format!("sse={origin}/events"), record, vec![], Some(&profile_dir)).await;
    for _ in 0..12 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (endpoint, launch_id) = (g.endpoint(), g.launch_id().clone());
    g.abandon();
    let reconnect = frontcap_chromium::generation::ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: frontcap_core::ids::RunId::new("pt-9").unwrap(),
        launch_id,
        profile: support::options(runs.path(), "pt-9", String::new(), RuleSet::default()).profile,
        endpoint,
        watches: Default::default(),
        watchdog: Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(),
        upstream: Some(frontcap_proxy::upstream::Upstream::new(vec![]).unwrap()),
    };
    let (mut g, _) = Generation::reconnect(reconnect).await.unwrap();
    for _ in 0..80 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let recorded = log(&g).await;
    let names = |l: &[serde_json::Value]| l.iter().map(|e| e[0].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(&recorded), ["sse-open", "sse", "sse", "sse", "sse-error", "sse-open", "sse", "sse", "sse"], "{recorded:?}");
    assert_eq!(seen.lock().unwrap().clone(), [None, Some("m3".to_string())]);
    let cassette = std::fs::read_to_string(runs.path().join("pt-9/cassettes/feed.jsonl")).unwrap();
    let (_, streams) = frontcap_core::cassette::parse_all(&cassette).unwrap();
    assert_eq!(streams.iter().map(|e| e.session).max(), Some(1), "{cassette}");
    assert!(streams.iter().any(|e| e.session == 0 && e.event == "close" && e.code == Some(1006)), "{cassette}");
    let kept = std::env::temp_dir().join(format!("feed-rl-{}.jsonl", std::process::id()));
    std::fs::write(&kept, &cassette).unwrap();
    let replay = RuleSet::from_json(&format!(r#"[{{"id":"feed","match":{{"url":"{origin}/events"}},"action":{{"type":"replay","cassette":{:?}}}}}]"#, kept.display().to_string())).unwrap();
    let (mut h, _runs2) = launch("pt-9r", &format!("sse={origin}/events"), replay, vec![]).await;
    h.advance(VirtualDuration::from_ms(8000)).await.unwrap();
    std::fs::remove_file(&kept).unwrap();
    assert_eq!(names(&log(&h).await), names(&recorded));
}

#[tokio::test]
async fn recorded_streams_keep_installation_destinations_through_replacement_and_loss() {
    let server = sse_server().await;
    let page = support::serve();
    let url = format!("http://{server}/events");
    let rules = RuleSet::from_json(&format!(r#"[{{"id":"feed","match":{{"url":"{url}"}},"action":{{"type":"record","cassette":"feed"}}}}]"#)).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(runs.path(),"stream-installations",format!("http://{page}/passthrough.html?sse={url}"),rules.clone());
    detached.apply(&mut options);
    options.upstream=Some(frontcap_proxy::upstream::Upstream::new(vec![]).unwrap());
    let profile=options.profile.clone();
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    let before=g.network_rules().await.unwrap();
    g.replace_network_rules(&frontcap_core::ids::RequestId::new("replace").unwrap(),0,&before.boundary,rules,&[format!("http://{page}")]).await.unwrap();
    g.eval_main("openAll()").await.unwrap();
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    }
    assert_eq!(log(&g).await.iter().filter(|e|e[0]=="sse").count(),6);
    let endpoint=g.endpoint(); let launch_id=g.launch_id().clone(); let now=g.now();
    g.abandon();
    let (mut g,_)=Generation::reconnect(frontcap_chromium::generation::ReconnectOptions {
        repo_root:support::repo_root(),runs_root:runs.path().to_path_buf(),run:frontcap_core::ids::RunId::new("stream-installations").unwrap(),
        launch_id,profile,endpoint,watches:Default::default(),watchdog:Duration::from_secs(10),
        granted_until:None,dialog_rules:Default::default(),upstream:Some(frontcap_proxy::upstream::Upstream::new(vec![]).unwrap()),
    }).await.unwrap();
    assert_eq!(g.now(),now);
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    assert_eq!(log(&g).await.iter().filter(|e|e[0]=="sse-error").count(),2);
    for path in ["cassettes/feed.jsonl","cassettes/revisions/0001/feed.jsonl"] {
        let text=std::fs::read_to_string(runs.path().join("stream-installations").join(path)).unwrap();
        let (_,events)=frontcap_core::cassette::parse_all(&text).unwrap();
        assert!(events.iter().all(|e|e.session==0),"{path}: {text}");
        assert_eq!(events.iter().filter(|e|e.event=="open").count(),1,"{path}: {text}");
        assert_eq!(events.iter().filter_map(|e|e.data.as_deref()).collect::<Vec<_>>(),["m1","m2","m3"],"{path}: {text}");
        assert_eq!(events.iter().filter(|e|e.event=="close" && e.code==Some(1006)).count(),1,"{path}: {text}");
    }
    // Both EventSources reconnect under the active installation. Its restored counter must follow
    // its own earlier session, while the replaced installation receives no new recording.
    g.advance(VirtualDuration::from_ms(3100)).await.unwrap();
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    }
    assert_eq!(log(&g).await.iter().filter(|e|e[0]=="sse").count(),12);
    let read = |path: &str| frontcap_core::cassette::parse_all(&std::fs::read_to_string(runs.path().join("stream-installations").join(path)).unwrap()).unwrap().1;
    let old=read("cassettes/feed.jsonl");
    let active=read("cassettes/revisions/0001/feed.jsonl");
    assert!(old.iter().all(|e|e.session==0));
    assert_eq!(active.iter().filter(|e|e.event=="open").map(|e|e.session).collect::<Vec<_>>(),[0,1,2]);
    for session in [1,2] {
        assert_eq!(active.iter().filter(|e|e.session==session).filter_map(|e|e.data.as_deref()).collect::<Vec<_>>(),["m1","m2","m3"]);
    }
    g.stop().await.unwrap();
}
