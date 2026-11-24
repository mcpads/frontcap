//! `record` and `replay`: a run against a real backend becomes repeatable without it.
mod support;

use frontcap_core::ids::RequestId;
use frontcap_core::rules::RuleSet;
use frontcap_recorder::launcher::{self, Attached, Workspace};
use frontcap_recorder_protocol::{Call, Op, Outcome, Reply};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// A backend on another origin answering `<path> #<n>`, n counting its requests; `/gz…` gzip-compressed
/// as a real server does, `/r` a redirect to `/n`.
fn backend() -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let n = Arc::new(AtomicU32::new(0));
    std::thread::spawn(move || {
        let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k, v).unwrap();
        for req in server.incoming_requests() {
            let k = n.fetch_add(1, Ordering::SeqCst) + 1;
            let body = format!("{} #{k}", req.url());
            let cors = header("Access-Control-Allow-Origin", "*");
            let resp = if req.url() == "/r" {
                tiny_http::Response::from_string("").with_status_code(302).with_header(header("Location", "/n")).with_header(cors).boxed()
            } else if req.url().starts_with("/gz") {
                use std::io::Write as _;
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(body.as_bytes()).unwrap();
                tiny_http::Response::from_data(gz.finish().unwrap()).with_header(header("Content-Encoding", "gzip")).with_header(cors).boxed()
            } else {
                tiny_http::Response::from_string(body).with_header(cors).boxed()
            };
            let _ = req.respond(resp);
        }
    });
    addr
}

async fn launch(ws: &Workspace, url: String, rules: RuleSet, env: &[(String, String)]) -> Attached {
    let req = launcher::LaunchRequest { rules, ..support::launch_request(&RequestId::generate().to_string(), &url) };
    launcher::launch(ws, req, &support::recorder_bin(), env).await.unwrap()
}

async fn mutate(a: &mut Attached, id: &str, op: Op) -> Reply {
    a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: RequestId::new(id).unwrap(), op }).await.unwrap()
}

async fn eval(a: &mut Attached, id: &str, expression: &str) -> serde_json::Value {
    match mutate(a, id, Op::Evaluate { expression: expression.into() }).await {
        Reply::Done { outcome: Outcome::Evaluated { value }, .. } => value,
        other => panic!("{other:?}"),
    }
}

/// Calls each path in turn, letting the page receive each answer.
async fn calls(a: &mut Attached, paths: &[&str]) -> Vec<String> {
    mutate(a, "load", Op::Advance { by_us: 50_000 }).await;
    for (i, p) in paths.iter().enumerate() {
        eval(a, &format!("call-{i}"), &format!("call({p:?})")).await;
        mutate(a, &format!("wait-{i}"), Op::Advance { by_us: 100_000 }).await;
    }
    serde_json::from_value(eval(a, "got", "got").await).unwrap()
}

async fn stop(ws: &Workspace, mut a: Attached) -> PathBuf {
    mutate(&mut a, "stop", Op::Stop).await;
    support::wait_until_exited(&ws.runtime_dir(&a.launch_id)).await;
    ws.runs().join(launcher::run_id_of(&a.launch_id).as_str())
}

fn rules(json: String) -> RuleSet {
    RuleSet::from_json(&json).unwrap()
}

#[tokio::test]
async fn a_recorded_backend_is_replayed_without_it() {
    let (page, api) = (support::serve(), backend());
    let (_tmp, ws) = support::workspace();
    let url = format!("http://{page}/api.html?api=http://{api}");
    let record = rules(format!(r#"[{{"id":"api","match":{{"url":"http://{api}/*"}},"action":{{"type":"record","cassette":"api"}}}}]"#));
    let mut a = launch(&ws, url.clone(), record, &[]).await;
    let recorded = calls(&mut a, &["/n?b=2&a=1", "/m", "/n?a=1&b=2", "/gz", "/r"]).await;
    assert_eq!(recorded, ["/n?b=2&a=1 #1", "/m #2", "/n?a=1&b=2 #3", "/gz #4", "/n #6"], "the redirect followed");
    let source = stop(&ws, a).await;
    let cassette = source.join("cassettes/api.jsonl");
    let lines = frontcap_core::cassette::parse(&std::fs::read_to_string(&cassette).unwrap()).unwrap();
    assert_eq!(lines.len(), 6, "every hop of the redirect is a line: {lines:#?}");
    assert_eq!(lines[0].key, lines[2].key, "the same request, its query in another order");
    assert_eq!(frontcap_core::encode::unbase64(&lines[3].response.body_base64).unwrap(), b"/gz #4", "the body as the page received it, decoded");
    assert_eq!((lines[4].response.status, lines[4].response.body_base64.as_str()), (302, ""));
    let trace = std::fs::read_to_string(source.join("trace.jsonl")).unwrap();
    assert!(!trace.contains("transport_bypass") && !trace.contains("cassette_miss"), "{trace}");

    // The backend is gone: the page is answered from the cassette, each key in recorded order.
    let gone = format!("http://127.0.0.1:{}", { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() });
    let url = format!("http://{page}/api.html?api={gone}");
    let kept = std::env::temp_dir().join(format!("cassette-{}.jsonl", std::process::id()));
    std::fs::copy(&cassette, &kept).unwrap();
    let text = std::fs::read_to_string(&kept).unwrap().replace(&format!("http://{api}"), &gone);
    std::fs::write(&kept, text).unwrap();
    let replay = rules(format!(r#"[{{"id":"api","match":{{"url":"{gone}/*"}},"action":{{"type":"replay","cassette":{:?}}}}}]"#, kept.display().to_string()));
    let mut b = launch(&ws, url, replay, &[]).await;
    let replayed = calls(&mut b, &["/n?a=1&b=2", "/m", "/n?b=2&a=1", "/gz", "/r", "/m"]).await;
    assert_eq!(replayed, ["/n?b=2&a=1 #1", "/m #2", "/n?a=1&b=2 #3", "/gz #4", "/n #6", "failed"], "after the last: a failure");
    let bundle = stop(&ws, b).await;
    std::fs::remove_file(&kept).unwrap();
    let trace = std::fs::read_to_string(bundle.join("trace.jsonl")).unwrap();
    assert_eq!(trace.matches("\"source\":\"replay\"").count(), 6, "{trace}");
    assert!(trace.contains("\"cassette_miss\""));

    // The replayed run re-executes from its bundle's copy of the cassette, the original file gone.
    let options = frontcap_recorder::replay::ReplayOptions { url: None, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![] };
    let report = frontcap_recorder::replay::replay(&ws, &bundle, options).await.unwrap();
    assert!(report.equal, "{report:#?}");

    // The recording run itself re-executes from what it recorded, the backend gone.
    let options = frontcap_recorder::replay::ReplayOptions { url: None, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![] };
    let report = frontcap_recorder::replay::replay(&ws, &source, options).await.unwrap();
    assert!(report.equal, "{report:#?}");
}

/// Criterion 6 for a cassette: half consumed when the recorder dies, the relaunched recorder answers
/// with the next recorded response, not the first again (consumption re-derived from the trace).
#[tokio::test]
async fn a_half_consumed_cassette_survives_a_recorder_fault() {
    use frontcap_core::cassette::{Key, Latency, Line, Response};
    let page = support::serve();
    let (_tmp, ws) = support::workspace();
    let api = "https://api.example.test";
    let line = |n: u32| Line {
        requires_progress: false,
        key: Key::of("GET", &format!("{api}/n"), None),
        response: Response { status: 200, headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(), body_base64: frontcap_core::encode::base64(format!("n {n}").as_bytes()) },
        latency: Latency { virtual_ms: 20, wall_ms: 1 },
    };
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), (1..=3).map(|n| line(n).to_line() + "\n").collect::<String>()).unwrap();
    let replay = rules(format!(r#"[{{"id":"api","match":{{"url":"{api}/*"}},"action":{{"type":"replay","cassette":{:?}}}}}]"#, file.path().display().to_string()));
    let env = [("FRONTCAP_FAULT".to_string(), "wait-1@after_effect".to_string())];
    let mut a = launch(&ws, format!("http://{page}/api.html?api={api}"), replay, &env).await;
    mutate(&mut a, "load", Op::Advance { by_us: 50_000 }).await;
    eval(&mut a, "call-0", "call('/n')").await;
    mutate(&mut a, "wait-0", Op::Advance { by_us: 100_000 }).await;
    eval(&mut a, "call-1", "call('/n')").await;
    let died = a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: RequestId::new("wait-1").unwrap(), op: Op::Advance { by_us: 100_000 } }).await;
    assert!(died.is_err(), "{died:?}");
    launcher::recover(&ws, &a.launch_id, &support::recorder_bin(), &[]).await.unwrap();
    let mut b = launcher::reattach(&ws, &a.launch_id, RequestId::new("reattach").unwrap()).await.unwrap();
    mutate(&mut b, "wait-1", Op::Advance { by_us: 100_000 }).await;
    eval(&mut b, "call-2", "call('/n')").await;
    mutate(&mut b, "wait-2", Op::Advance { by_us: 100_000 }).await;
    let got: Vec<String> = serde_json::from_value(eval(&mut b, "got", "got").await).unwrap();
    assert_eq!(got, ["n 1", "n 2", "n 3"]);
    stop(&ws, b).await;
}

/// One app served at two origins: a page and its same-origin `/data`.
fn same_app() -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let text = if req.url() == "/data" { "answer" } else { "<!doctype html><title>same app</title>" };
            let _ = req.respond(tiny_http::Response::from_string(text).with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
        }
    });
    addr
}

/// A run that recorded its same-origin API replays at another origin of the same app: the cassette is
/// matched with the origin moved, as the launch and the commands are. A rule that recorded nothing replays
/// from an empty cassette; neither it nor the moved copies outlive the replay.
#[tokio::test]
async fn a_recorded_run_replays_at_another_origin() {
    let (old, new) = (same_app(), same_app());
    let (_tmp, ws) = support::workspace();
    let recording = rules(format!(
        r#"[{{"id":"api","match":{{"url":"http://{old}/data"}},"action":{{"type":"record","cassette":"api"}}}},{{"id":"unused","match":{{"url":"http://{old}/never"}},"action":{{"type":"record","cassette":"unused"}}}}]"#
    ));
    let mut a = launch(&ws, format!("http://{old}/"), recording, &[]).await;
    mutate(&mut a, "load", Op::Advance { by_us: 50_000 }).await;
    eval(&mut a, "fetch", "window.got = 'pending'; fetch('/data').then((r) => r.text()).then((t) => { window.got = t; }, () => { window.got = 'failed'; }); 0").await;
    mutate(&mut a, "wait", Op::Advance { by_us: 100_000 }).await;
    assert_eq!(eval(&mut a, "read", "window.got").await, "answer");
    let source = stop(&ws, a).await;
    let options = |url| frontcap_recorder::replay::ReplayOptions { url, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![] };
    let same = frontcap_recorder::replay::replay(&ws, &source, options(None)).await.unwrap();
    assert!(same.equal, "{same:#?}");
    let moved = frontcap_recorder::replay::replay(&ws, &source, options(Some(format!("http://{new}/")))).await.unwrap();
    assert!(moved.equal, "{moved:#?}");
    let cassette = std::fs::read_to_string(source.join("cassettes/api.jsonl")).unwrap();
    assert!(cassette.contains(&old.to_string()) && !cassette.contains(&new.to_string()), "the finalized bundle is unchanged");
    let left: Vec<_> = std::fs::read_dir(&ws.root).unwrap().filter_map(|e| e.ok()).filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("replay-cassettes-") || name.starts_with("empty-cassette-")
        })
        .collect();
    assert!(left.is_empty(), "the moved copies serve their replay only: {left:?}");
}

#[tokio::test]
async fn reused_recording_names_keep_each_installations_request_order() {
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    let (page, api) = (support::serve(), backend());
    let (_tmp, ws) = support::workspace();
    let record = rules(format!(r#"[{{"id":"api","match":{{"url":"http://{api}/*"}},"action":{{"type":"record","cassette":"api"}}}}]"#));
    let mut a = launch(&ws, format!("http://{page}/api.html?api=http://{api}"), record.clone(), &[]).await;
    mutate(&mut a,"load",Op::Advance { by_us:50_000 }).await;
    for revision in 0..3 {
        if revision > 0 {
            let Reply::Observed { result: Observation::NetworkRules(snapshot) } = a.client.call(Call::Observe { op:ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!("rules snapshot") };
            mutate(&mut a,&format!("install-{revision}"),Op::SetRules {
                expected_revision:revision-1,boundary:snapshot.boundary,rules:record.clone(),
            }).await;
        }
        for request in 0..2 {
            eval(&mut a,&format!("call-{revision}-{request}"),"call('/n')").await;
            mutate(&mut a,&format!("wait-{revision}-{request}"),Op::Advance { by_us:100_000 }).await;
        }
    }
    let Reply::Observed { result: Observation::NetworkRules(snapshot) } = a.client.call(Call::Observe { op:ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!("rules snapshot") };
    mutate(&mut a,"retitle",Op::StateApply {
        boundary:snapshot.boundary, changes:vec![frontcap_core::intervention::StateChange {
            target:frontcap_core::predicate::Locator::Css { selector:"title".into() },
            field:frontcap_core::intervention::StateField::Text, value:serde_json::json!("Recorded all installations"), expected:None,
        }], context:None, progress_us:None,
    }).await;
    assert_eq!(eval(&mut a,"title","document.title").await,"Recorded all installations");
    eval(&mut a,"button","(()=>{const b=document.createElement('button');b.id='recorded-click';b.textContent='Confirm';b.style='position:fixed;left:20px;top:20px;width:100px;height:40px';b.onclick=()=>document.title='Confirmed';document.body.append(b);return 0})()").await;
    let Reply::Observed { result: Observation::NetworkRules(snapshot) } = a.client.call(Call::Observe { op:ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!("rules snapshot") };
    mutate(&mut a,"confirm",Op::Act {
        action:frontcap_core::action::Action::ClickAt {
            target:frontcap_core::predicate::Locator::Css { selector:"#recorded-click".into() },
            point:[50,40], boundary:snapshot.boundary,
        }, then:None, context: None,
    }).await;
    assert_eq!(eval(&mut a,"confirmed","document.title").await,"Confirmed");
    assert_eq!(eval(&mut a,"got","got").await,serde_json::json!(["/n #1","/n #2","/n #3","/n #4","/n #5","/n #6"]));
    let source = stop(&ws,a).await;
    let options = frontcap_recorder::replay::ReplayOptions {
        url:None,watchdog_ms:10_000,repo_root:support::repo_root(),recorder_bin:support::recorder_bin(),env:vec![],
    };
    let report = frontcap_recorder::replay::replay(&ws,&source,options).await.unwrap();
    assert!(report.equal,"{report:#?}");
}
