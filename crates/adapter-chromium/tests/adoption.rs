//! Adopting stopped network work from engine evidence (engine patch 24): issue stamps of requests no
//! recorder bound, and receipts of resolutions a dead recorder dispatched.
mod support;

use cdp::protocol::{fetch, target};
use frontcap_chromium::generation::{Endpoint, Generation, ReconnectOptions};
use frontcap_chromium::session::Session;
use frontcap_core::event::{EventKind, ResponseSource};
use frontcap_core::ids::RunId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::WatchRegistry;
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_core::time::{VirtualDuration, VirtualTime};

struct Run {
    runs: std::path::PathBuf,
    run: &'static str,
    profile: frontcap_core::profile::DeterminismProfile,
    launch_id: frontcap_core::ids::LaunchId,
    _detached: support::DetachedProfile,
}

async fn launch(run: &'static str, page: String, rules: RuleSet) -> (Generation, Run) {
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("adoption evidence (retained on failure): {}", runs.display());
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, run, page, rules);
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let g = Generation::launch(options).await.unwrap();
    let launch_id = g.launch_id().clone();
    (g, Run { runs, run, profile, launch_id, _detached: detached })
}

async fn reconnect(r: &Run, endpoint: Endpoint) -> Result<Generation, frontcap_chromium::ChromiumError> {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(), runs_root: r.runs.clone(), run: RunId::new(r.run).unwrap(),
        launch_id: r.launch_id.clone(), profile: r.profile.clone(), endpoint, watches: WatchRegistry::default(),
        watchdog: std::time::Duration::from_secs(10), granted_until: None, dialog_rules: Default::default(), upstream: None,
    })
    .await
    .map(|(g, _)| g)
}

async fn native(endpoint: &Endpoint) -> Session {
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}


type Requests = Vec<(String, VirtualTime)>;
type Responses = Vec<(String, VirtualTime, u16, ResponseSource)>;

/// The trace's request and response records of requests to `url`: (ordinal, issue instant) and
/// (ordinal, instant, status, source).
fn records(r: &Run, url_suffix: &str) -> (Requests, Responses) {
    let events = BehaviorLog::read(&r.runs.join(r.run).join("trace.jsonl")).unwrap();
    let requests: Vec<(String, VirtualTime)> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Request(q) if q.url.ends_with(url_suffix) => Some((q.id.clone(), e.t_us)),
        _ => None,
    }).collect();
    let responses = events.iter().filter_map(|e| match &e.kind {
        EventKind::Response(p) if requests.iter().any(|(id, _)| *id == p.id) => Some((p.id.clone(), e.t_us, p.status, p.source)),
        _ => None,
    }).collect();
    (requests, responses)
}

fn rules(addr: std::net::SocketAddr, json: &str) -> RuleSet {
    RuleSet::from_json(&json.replace("ADDR", &addr.to_string())).unwrap()
}

fn spec(body: &str) -> MockSpec {
    MockSpec { status: 200, headers: Default::default(), body: body.into(), body_base64: None, latency_ms: 0 }
}

#[tokio::test]
async fn a_request_issued_without_a_controller_is_adopted_at_its_issue_instant() {
    let addr = support::serve();
    let (mut g, r) = launch("unbound", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"late","match":{"url":"http://ADDR/late"},"action":{"type":"mock","status":200,"body":"late","latency_ms":500}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let n = native(&endpoint).await;
    n.eval("globalThis.got=[];fetch('/late').then(r=>r.text()).then(t=>got.push(t+'@'+performance.now()));true", None).await.unwrap();
    n.client.close();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    let (requests, _) = records(&r, "/late");
    assert_eq!(requests.iter().map(|(_, t)| *t).collect::<Vec<_>>(), vec![VirtualTime(201_000)], "recorded once, at its issue instant");
    g.advance(VirtualDuration::from_ms(600)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["late@701"]), "its latency counts from its issue");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_resolution_dispatched_before_a_fault_is_recorded_once_from_its_receipt() {
    let addr = support::serve();
    let (mut g, r) = launch("dispatched", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.got=[];fetch('/held').then(r=>r.text()).then(t=>got.push(t));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let ordinal = g.held_ordinals().pop().unwrap();
    let endpoint = g.endpoint();
    g.dispatch_and_abandon(&ordinal, spec("answered")).await.unwrap();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert!(g.held_ordinals().is_empty());
    let (_, responses) = records(&r, "/held");
    assert_eq!(responses.iter().map(|(id, _, status, source)| (id.clone(), *status, *source)).collect::<Vec<_>>(),
        vec![(ordinal, 200, ResponseSource::Agent)], "the accepted answer is recorded once, from its receipt");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["answered"]));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_held_request_the_page_aborted_while_uncontrolled_is_failed_from_its_cancel_receipt() {
    let addr = support::serve();
    let (mut g, r) = launch("cancelled", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.got=[];globalThis.ac=new AbortController();fetch('/held',{signal:ac.signal}).then(()=>got.push('answered'),()=>got.push('aborted'));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let ordinal = g.held_ordinals().pop().unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let n = native(&endpoint).await;
    n.eval("ac.abort();true", None).await.unwrap();
    n.client.close();
    let g = reconnect(&r, endpoint).await.unwrap();
    assert!(g.held_ordinals().is_empty(), "the hold ended with its request");
    let (_, responses) = records(&r, "/held");
    assert_eq!(responses.iter().map(|(id, _, status, source)| (id.clone(), *status, *source)).collect::<Vec<_>>(),
        vec![(ordinal, 0, ResponseSource::Fail)]);
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["aborted"]));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_redirect_hop_is_bound_and_adopted_separately() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal};
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let response = match request.url() {
                "/redir" => tiny_http::Response::from_string("").with_status_code(302)
                    .with_header(tiny_http::Header::from_bytes("Location", "/held").unwrap()),
                _ => tiny_http::Response::from_string("<!doctype html><p>redirect</p>")
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()),
            };
            let _ = request.respond(response);
        }
    });
    let rules = rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}},
        {"id":"pass","match":{"url":"http://ADDR/redir"},"action":{"type":"passthrough"}}]"#);
    let runs = tempfile::tempdir().unwrap().keep();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "redirect", format!("http://{addr}/index.html"), rules);
    options.passthrough_origins = Some(vec![format!("http://{addr}")]);
    detached.apply(&mut options);
    let r = Run { runs: runs.clone(), run: "redirect", profile: options.profile.clone(), launch_id: frontcap_core::ids::LaunchId::new("redirect-launch").unwrap(), _detached: detached };
    options.launch_id = Some(r.launch_id.clone());
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.got=[];fetch('/redir').then(r=>r.text()).then(t=>got.push(t));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let held = g.held_ordinals();
    assert_eq!(held.len(), 1, "the second hop is held");
    let endpoint = g.endpoint();
    g.abandon();
    let bound: Vec<_> = BindingJournal::read(&runs.join("redirect").join("interceptions.jsonl")).unwrap().into_iter().filter_map(|e| match e {
        BindingEntry::Bound(b) if b.url.ends_with("/redir") || b.url.ends_with("/held") => Some((b.ordinal.clone(), b.fetch_id.clone())),
        _ => None,
    }).collect();
    assert_eq!(bound.len(), 2, "each hop is its own pause: {bound:?}");
    assert_ne!(bound[0].1, bound[1].1);
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert_eq!(g.held_ordinals(), held, "only the second hop is adopted, under its own ordinal");
    g.respond(&held[0], spec("hop two")).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["hop two"]));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_bound_request_gone_without_native_evidence_fails_recovery() {
    let addr = support::serve();
    let (mut g, r) = launch("no-evidence", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("fetch('/held').catch(()=>{});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let ordinal = g.held_ordinals().pop().unwrap();
    let endpoint = g.endpoint();
    g.dispatch_and_abandon(&ordinal, spec("answered")).await.unwrap();
    // Another controller adopts the interceptor and drops the receipt the next recorder would need.
    let n = native(&endpoint).await;
    // The abandoned connection holds the interception until the browser has seen it close.
    let enable = fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*".into()), request_stage: Some(fetch::RequestStage::Request), ..Default::default() }]), ..Default::default() };
    let wall = std::time::Instant::now();
    while let Err(e) = n.call(&enable).await {
        assert!(e.to_string().contains("already has a consumer") && wall.elapsed() < std::time::Duration::from_secs(2), "{e}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let receipts = n.call(&fetch::FrontcapReceiptsParams::default()).await.unwrap().receipts;
    n.call(&fetch::FrontcapForgetReceiptsParams::new(receipts.into_iter().map(|r| r.request_id).collect())).await.unwrap();
    n.client.close();
    let error = match reconnect(&r, endpoint.clone()).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("recovery must not invent an outcome for {ordinal}"),
    };
    assert!(error.contains(&ordinal) && error.contains("no native evidence"), "{error}");
    if let Ok(Some(identity)) = frontcap_core::process::ProcessIdentity::of(endpoint.browser_pid) {
        let _ = identity.kill();
    }
}

fn trace_kinds(r: &Run) -> Vec<(String, u64)> {
    BehaviorLog::read(&r.runs.join(r.run).join("trace.jsonl")).unwrap().into_iter()
        .map(|e| (serde_json::to_value(&e).unwrap()["kind"].as_str().unwrap().to_string(), e.t_us.0)).collect()
}

/// Records the recovery adds do not overtake observer reports the dead recorder never read: a report at
/// 400 ms and a request issued at 450 ms while no recorder ran are written in time order, and a later
/// recovery still works.
#[tokio::test]
async fn adopted_records_follow_reports_the_dead_recorder_never_read() {
    use frontcap_core::predicate::{Locator, Predicate};
    let addr = support::serve();
    let (mut g, r) = launch("report-order", format!("http://{addr}/adopt_order.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.watch_add(frontcap_core::ids::WatchName::new("late").unwrap(), Predicate::Attached { target: Locator::Css { selector: ".late".into() } }).await.unwrap();
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(200)).await.unwrap();
    support::shared_time::progress_without_recorder(&endpoint, VirtualTime(450_000), r.profile.initial_date_ms).await;
    let g = reconnect(&r, endpoint.clone()).await.expect("recovers with an unread report before the adopted request");
    assert_eq!(g.held_ordinals().len(), 1);
    let order: Vec<_> = trace_kinds(&r).into_iter()
        .filter(|(k, t)| (k == "watch" && *t == 400_000) || (k == "request" && *t == 450_000)).collect();
    assert_eq!(order, vec![("watch".into(), 400_000), ("request".into(), 450_000)]);
    g.abandon();
    reconnect(&r, endpoint).await.expect("a second recovery still works").stop().await.unwrap();
}

/// A delayed answer the page aborted while no recorder ran ends when the recovery finds it ended, not at
/// the deadline it never reached.
#[tokio::test]
async fn a_delayed_answer_the_page_aborted_while_uncontrolled_ends_now() {
    let addr = support::serve();
    let (mut g, r) = launch("aborted-delay", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"slow","match":{"url":"http://ADDR/slow"},"action":{"type":"mock","status":200,"body":"slow","latency_ms":5000}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.ac=new AbortController();fetch('/slow',{signal:ac.signal}).catch(()=>{});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let n = native(&endpoint).await;
    n.eval("ac.abort();true", None).await.unwrap();
    n.client.close();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    let (_, responses) = records(&r, "/slow");
    assert_eq!(responses.iter().map(|(_, t, status, _)| (*t, *status)).collect::<Vec<_>>(), vec![(VirtualTime(202_000), 0)]);
    g.advance(VirtualDuration::from_ms(17)).await.expect("time continues after the recovery");
    g.stop().await.unwrap();
}

/// Requests issued at one instant with no recorder are decided in the order the page issued them, as
/// with a recorder attached: the first takes the first sequence step even if the engine published it later.
#[tokio::test]
async fn unbound_requests_are_decided_in_issue_order() {
    let addr = support::serve();
    let (mut g, r) = launch("issue-order", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"seq","match":{"url":"http://ADDR/seq"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"first"},{"status":200,"body":"second"}]}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let n = native(&endpoint).await;
    n.eval("globalThis.got={};fetch('/seq',{method:'POST',body:new Blob(['x'.repeat(1<<20)])}).then(r=>r.text()).then(t=>got.big=t);fetch('/seq').then(r=>r.text()).then(t=>got.small=t);true", None).await.unwrap();
    n.client.close();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"big": "first", "small": "second"}));
    g.stop().await.unwrap();
}

/// A dispatched answer reconciles against every payload sent for that pause: a retry whose first,
/// ambiguous attempt took effect is recorded from the first one's receipt.
#[tokio::test]
async fn an_earlier_dispatched_answer_matching_the_receipt_is_recorded() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, Resolution};
    let addr = support::serve();
    let (mut g, r) = launch("retried-answer", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.got=[];fetch('/held').then(r=>r.text()).then(t=>got.push(t));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let ordinal = g.held_ordinals().pop().unwrap();
    let endpoint = g.endpoint();
    g.dispatch_and_abandon(&ordinal, spec("first")).await.unwrap();
    // The retry's payload was journaled after the first, which the engine had already accepted.
    let journal = r.runs.join(r.run).join("interceptions.jsonl");
    let fetch_id = BindingJournal::read(&journal).unwrap().into_iter().find_map(|e| match e {
        BindingEntry::Dispatched { ordinal: o, fetch_id, .. } if o == ordinal => Some(fetch_id),
        _ => None,
    }).unwrap();
    BindingJournal::open(&journal).unwrap().append(&BindingEntry::Dispatched {
        ordinal: ordinal.clone(), fetch_id, resolution: Resolution::Fulfill { status: 200, body_sha256: frontcap_core::canonical::sha256_bytes_hex(b"retry") },
    }).unwrap();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    let (_, responses) = records(&r, "/held");
    assert_eq!(responses.iter().map(|(id, _, status, _)| (id.clone(), *status)).collect::<Vec<_>>(), vec![(ordinal, 200)]);
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["first"]));
    g.stop().await.unwrap();
}

/// A bound pass-through under a `record` rule, adopted before it was continued, still writes its
/// cassette line once the network answers.
#[tokio::test]
async fn an_adopted_recorded_pass_through_keeps_its_cassette_line() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, Decision};
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let body = if request.url() == "/rec" { "recorded" } else { "<!doctype html><p>record</p>" };
            let _ = request.respond(tiny_http::Response::from_string(body)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
        }
    });
    let installed = rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/rec"},"action":{"type":"hold"}},
        {"id":"rec","match":{"url":"http://ADDR/never"},"action":{"type":"record","cassette":"rec"}}]"#);
    let runs = tempfile::tempdir().unwrap().keep();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "record-adopt", format!("http://{addr}/index.html"), installed);
    options.passthrough_origins = Some(vec![format!("http://{addr}")]);
    detached.apply(&mut options);
    let r = Run { runs: runs.clone(), run: "record-adopt", profile: options.profile.clone(), launch_id: frontcap_core::ids::LaunchId::new("record-adopt-launch").unwrap(), _detached: detached };
    options.launch_id = Some(r.launch_id.clone());
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.got=[];fetch('/rec').then(r=>r.text()).then(t=>got.push(t));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let ordinal = g.held_ordinals().pop().unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    // The dead recorder had decided it as the `record` rule's pass-through and not yet continued it.
    let journal = runs.join("record-adopt").join("interceptions.jsonl");
    let rewritten: Vec<BindingEntry> = BindingJournal::read(&journal).unwrap().into_iter().map(|e| match e {
        BindingEntry::Bound(mut b) if b.ordinal == ordinal => {
            b.rule = frontcap_core::ids::RuleId::new("rec").unwrap();
            b.decision = Decision::Passthrough;
            BindingEntry::Bound(b)
        }
        other => other,
    }).collect();
    std::fs::remove_file(&journal).unwrap();
    let mut j = BindingJournal::open(&journal).unwrap();
    for e in &rewritten { j.append(e).unwrap(); }
    let mut g = reconnect(&r, endpoint).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["recorded"]));
    let cassette = std::fs::read_to_string(runs.join("record-adopt").join("cassettes").join("rec.jsonl")).unwrap_or_default();
    assert_eq!(cassette.lines().filter(|l| l.contains("/rec")).count(), 1, "{cassette}");
    g.stop().await.unwrap();
}

/// A cross-origin request issued with no recorder is preflighted while uncontrolled: on reconnect the
/// preflight is recognized as one (answered as the mock's server would, never taking a step) at its
/// request's issue instant, and the request itself is then decided by its rule.
#[tokio::test]
async fn a_preflight_issued_without_a_controller_is_adopted_as_a_preflight() {
    let addr = support::serve();
    let api = format!("http://localhost:{}", addr.port());
    let (mut g, r) = launch("preflight-adopt", format!("http://{addr}/fetch.html"), RuleSet::from_json(&format!(
        r#"[{{"id":"api","match":{{"url":"{api}/api"}},"action":{{"type":"sequence","after_last":"fail","steps":[{{"status":200,"body":"ok"}}]}}}}]"#
    )).unwrap()).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let n = native(&endpoint).await;
    n.eval(&format!("globalThis.got=[];fetch('{api}/api',{{method:'POST',headers:{{'content-type':'application/json'}},body:'{{}}'}}).then(r=>r.text()).then(t=>got.push(t),e=>got.push('failed'));true"), None).await.unwrap();
    n.client.close();
    let mut g = reconnect(&r, endpoint).await.expect("a preflight paused with no recorder is adopted");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["ok"]), "the preflight took no sequence step");
    g.stop().await.unwrap();
}

/// A body that arrives `delay` after its headers, so a document commits before its load ends.
struct SlowBody { delay: std::time::Duration, sent: bool }
impl std::io::Read for SlowBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent { return Ok(0) }
        std::thread::sleep(self.delay);
        self.sent = true;
        let body = b"<!doctype html><title>slow</title><p id=done>done</p>";
        buf[..body.len()].copy_from_slice(body);
        Ok(body.len())
    }
}

/// A passed-through document still loading when the recorder dies finishes its load under the next one.
/// Its load ended while no recorder ran: the end is recorded once, from the engine's receipt.
#[tokio::test]
async fn a_document_whose_load_ended_while_uncontrolled_completes_and_is_recorded_once() {
    document_loading_across_a_fault("loading-doc-ended", 300, 600).await;
}

/// Its body still arriving when the next recorder takes over: the request is followed again and its end
/// is recorded when this session sees it.
#[tokio::test]
async fn a_document_still_receiving_its_body_after_recovery_completes_and_is_recorded_once() {
    document_loading_across_a_fault("loading-doc-arriving", 1500, 50).await;
}

async fn document_loading_across_a_fault(run: &'static str, body_delay_ms: u64, wait_ms: u64) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let header = tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap();
            let _ = if request.url() == "/slow.html" {
                request.respond(tiny_http::Response::new(200.into(), vec![header], SlowBody { delay: std::time::Duration::from_millis(body_delay_ms), sent: false }, None, None))
            } else {
                request.respond(tiny_http::Response::from_string("<!doctype html><p>first</p>").with_header(header))
            };
        }
    });
    let runs = tempfile::tempdir().unwrap().keep();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, run, format!("http://{addr}/index.html"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}")]);
    detached.apply(&mut options);
    let r = Run { runs: runs.clone(), run, profile: options.profile.clone(), launch_id: frontcap_core::ids::LaunchId::new(format!("{run}-launch")).unwrap(), _detached: detached };
    options.launch_id = Some(r.launch_id.clone());
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.navigate_action(&frontcap_core::ids::RequestId::new("navigate").unwrap(), &frontcap_core::action::Action::Navigate { url: format!("http://{addr}/slow.html") }).await.unwrap();
    assert_eq!(g.eval_main("document.readyState").await.unwrap(), serde_json::json!("loading"));
    let endpoint = g.endpoint();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    let mut g = reconnect(&r, endpoint).await.expect("a loading document is adopted");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("document.readyState+':'+document.getElementById('done').textContent").await.unwrap(), serde_json::json!("complete:done"));
    let (requests, responses) = records(&r, "/slow.html");
    assert_eq!(requests.len(), 1);
    assert_eq!(responses.iter().map(|(id, _, status, source)| (id.clone(), *status, *source)).collect::<Vec<_>>(),
        vec![(requests[0].0.clone(), 200, ResponseSource::Passthrough)], "the document's response is recorded once");
    g.stop().await.unwrap();
}

/// A navigation the dead recorder dispatched and the browser started, its document request paused when
/// the recorder died: the next recorder continues it as the same action, recorded once.
#[tokio::test]
async fn a_navigation_in_flight_across_a_fault_is_continued_as_its_action() {
    let addr = support::serve();
    let (mut g, r) = launch("nav-in-flight", format!("http://{addr}/fetch.html"), RuleSet::default()).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    let request = frontcap_core::ids::RequestId::new("navigate").unwrap();
    let action = frontcap_core::action::Action::Navigate { url: format!("http://{addr}/link.html") };
    g.navigate_dispatch_and_abandon(&action).await.unwrap();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert!(g.resume_navigation(&request, &action).await.unwrap(), "the paused document request is the navigation's");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), serde_json::json!("/link.html"));
    let actions = trace_kinds(&r).into_iter().filter(|(k, _)| k == "action").count();
    assert_eq!(actions, 1, "the navigation is one action");
    g.stop().await.unwrap();
}

/// With no paused document request the dispatched navigation never reached the browser.
#[tokio::test]
async fn a_navigation_that_never_left_has_nothing_to_continue() {
    let addr = support::serve();
    let (mut g, r) = launch("nav-not-sent", format!("http://{addr}/fetch.html"), RuleSet::default()).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    let action = frontcap_core::action::Action::Navigate { url: format!("http://{addr}/link.html") };
    assert!(!g.resume_navigation(&frontcap_core::ids::RequestId::new("navigate").unwrap(), &action).await.unwrap());
    g.stop().await.unwrap();
}

/// A navigation the page started while no recorder ran (its document request paused, unanswered)
/// commits under the next recorder at the next progress, recorded once with the page's issue instant.
#[tokio::test]
async fn a_page_navigation_started_without_a_recorder_commits_under_the_next_one() {
    let addr = support::serve();
    let (mut g, r) = launch("self-nav", format!("http://{addr}/self_navigate.html"), RuleSet::default()).await;
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(100)).await.unwrap();
    support::shared_time::progress_without_recorder(&endpoint, VirtualTime(250_000), r.profile.initial_date_ms).await;
    let mut g = reconnect(&r, endpoint).await.expect("recovers with the page's navigation in flight");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), serde_json::json!("/link.html"));
    let (requests, _) = records(&r, "/link.html");
    assert_eq!(requests.iter().map(|(_, t)| *t).collect::<Vec<_>>(), vec![VirtualTime(250_000)], "issued once, at the page's instant");
    let navigations = trace_kinds(&r).into_iter().filter(|(k, _)| k == "navigation").count();
    assert_eq!(navigations, 2, "the first document and the new one");
    g.stop().await.unwrap();
}

/// Receipts the trace no longer needs do not pile up in the engine: answered requests, passed-through
/// documents and the anchors of streams all have theirs forgotten.
#[tokio::test]
async fn no_receipt_outlives_its_recorded_outcome() {
    let addr = support::serve();
    let (mut g, _r) = launch("receipts-forgotten", format!("http://{addr}/stream.html"),
        rules(addr, r#"[{"id":"feed","match":{"url":"https://api.example.test/feed*"},"action":{"type":"stream_mock","messages":[{"at_ms":100,"data":"a"}]}},
            {"id":"chat","match":{"url":"wss://api.example.test/chat"},"action":{"type":"stream_mock","messages":[{"at_ms":50,"data":"ping"}],"close_at_ms":300}}]"#)).await;
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.engine_receipts().await.unwrap(), Vec::<String>::new());
    g.stop().await.unwrap();
}

/// Two bindings of one native pause contradict each other: recovery refuses rather than pick one.
#[tokio::test]
async fn conflicting_bindings_of_one_pause_fail_recovery() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal};
    let addr = support::serve();
    let (mut g, r) = launch("dup-binding", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"hold","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("fetch('/held').catch(()=>{});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    let journal = r.runs.join(r.run).join("interceptions.jsonl");
    let mut copy = BindingJournal::read(&journal).unwrap().into_iter().rev().find_map(|e| match e {
        BindingEntry::Bound(b) if b.url.ends_with("/held") => Some(b),
        _ => None,
    }).unwrap();
    copy.ordinal = "req-99".into();
    BindingJournal::open(&journal).unwrap().append(&BindingEntry::Bound(copy)).unwrap();
    let error = match reconnect(&r, endpoint.clone()).await { Err(e) => e.to_string(), Ok(_) => panic!("recovery must refuse") };
    assert!(error.contains("bound twice"), "{error}");
    if let Ok(Some(identity)) = frontcap_core::process::ProcessIdentity::of(endpoint.browser_pid) { let _ = identity.kill(); }
}

/// A browser receipt is the evidence after native answer but before a recorder writes its trace.
/// Adoption writes it once; a later reconnect must not repeat either the answer or the record.
#[tokio::test]
async fn answered_native_dialog_is_adopted_once_from_its_retained_receipt() {
    use frontcap_chromium::dialog_control::DialogControl;
    use cdp::protocol::{page, runtime};
    use std::time::Duration;
    let addr = support::serve();
    let (mut g, r) = launch("native-dialog-adopt", format!("http://{addr}/dialogs.html"), RuleSet::default()).await;
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    let endpoint = g.endpoint();
    drop(g);
    let session = native(&endpoint).await;
    let control = DialogControl::attach(&session).await.unwrap();
    session.call(&page::EnableParams::default()).await.unwrap();
    let frame_id = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id;
    let world = session.call(&page::CreateIsolatedWorldParams {
        world_name: Some("native-dialog-oracle".into()), ..page::CreateIsolatedWorldParams::new(frame_id)
    }).await.unwrap().execution_context_id;
    let effect = {
        let session = session.clone();
        tokio::spawn(async move {
            session.call(&runtime::EvaluateParams {
                context_id: Some(world), return_by_value: Some(true),
                ..runtime::EvaluateParams::new("document.body.dataset.nativeAnswer = String(confirm('unrecorded native')); document.body.dataset.pendingAnswer = String(confirm('retained pending'))".into())
            }).await.unwrap()
        })
    };
    let mut updates = control.subscribe();
    let dialog = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(dialog) = updates.borrow_and_update().clone().unwrap().dialogs.first() {
                break dialog.clone();
            }
            updates.changed().await.unwrap();
        }
    }).await.unwrap();
    control.answer(&page::FrontcapAnswerDialogParams::new(dialog.dialog_id.clone(), format!("native-auto-{}", frontcap_core::canonical::sha256_hex(&dialog.dialog_id)), true)).await.unwrap();
    let pending = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(dialog) = updates.borrow_and_update().clone().unwrap().dialogs.first()
                && dialog.message == "retained pending"
            {
                break dialog.clone();
            }
            updates.changed().await.unwrap();
        }
    }).await.unwrap();
    effect.abort();
    assert!(effect.await.unwrap_err().is_cancelled());
    assert_eq!(control.inspect().await.unwrap().receipts.len(), 1);
    drop(control);
    drop(session);
    let g = reconnect(&r, endpoint.clone()).await.unwrap();
    assert_eq!(g.eval_main("document.body.dataset.nativeAnswer").await.unwrap(), "true");
    assert_eq!(g.eval_main("document.body.dataset.pendingAnswer").await.unwrap(), "false");
    let native_records = |g: &Generation| BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
        EventKind::Dialog(d) if d.native.is_some() => Some((e.t_us, d)),
        _ => None,
    }).collect::<Vec<_>>();
    let records = native_records(&g);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, VirtualTime(10_000));
    assert_eq!(records[0].1.answer, "true");
    assert_eq!(records[0].1.native.as_ref().unwrap().source, frontcap_core::event::NativeDialogSource::Answered);
    assert_eq!(records[1].0, VirtualTime(10_000));
    assert_eq!(records[1].1.answer, "false");
    assert_eq!(records[0].1.native.as_ref().unwrap().id, "dialog-1");
    assert_eq!(records[1].1.native.as_ref().unwrap().id, "dialog-2");
    let bindings = std::fs::read_to_string(r.runs.join(r.run).join("dialog-bindings.jsonl")).unwrap();
    assert!(bindings.contains(&dialog.dialog_id) && bindings.contains(&pending.dialog_id));
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(!trace.contains(&dialog.dialog_id) && !trace.contains(&pending.dialog_id));
    assert_eq!(g.native_dialog_control().unwrap().inspect().await.unwrap().receipts.len(), 2,
        "both receipts must still be retained to test trace-written/forget-not-sent recovery");
    drop(g);
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert_eq!(native_records(&g), records);
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert!(g.native_dialog_control().unwrap().inspect().await.unwrap().receipts.is_empty());
}

#[tokio::test]
async fn a_selected_synchronous_prefix_keeps_its_deadline_and_cursor_on_adoption() {
    let addr = support::serve();
    let (mut g, r) = launch("sync-prefix", format!("http://{addr}/fetch.html"),
        rules(addr, r#"[{"id":"seq","match":{"url":"http://ADDR/seq"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"first","latency_ms":500},
            {"status":200,"body":"second","latency_ms":100},
            {"status":200,"body":"third"}]}}]"#)).await;
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let start = g.now();
    g.evaluate("window.got={};fetch('/seq').then(r=>r.text()).then(s=>got.async=s);const x=new XMLHttpRequest();x.open('GET','/seq',false);x.send();got.sync=x.responseText;true").await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"sync":"second"}));
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(100)));
    let endpoint = g.endpoint();
    g.abandon();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    g.evaluate("fetch('/seq').then(r=>r.text()).then(s=>got.after=s);true").await.unwrap();
    g.advance(VirtualDuration::from_ms(399)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"sync":"second","after":"third"}));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"async":"first","sync":"second","after":"third"}));
    let (requests, responses) = records(&r, "/seq");
    assert_eq!(requests.iter().map(|(_, t)| *t).collect::<Vec<_>>(), vec![start, start, start.saturating_add(VirtualDuration::from_ms(100))]);
    assert_eq!(responses.iter().find(|(id, ..)| id == &requests[0].0).unwrap().1, start.saturating_add(VirtualDuration::from_ms(500)));
    g.stop().await.unwrap();
    std::fs::remove_dir_all(&r.runs).unwrap();
}

#[tokio::test]
async fn worker_script_lineage_survives_adoption_and_matches_its_later_requests() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal};
    let addr = support::serve();
    let (mut g, r) = launch("worker-lineage", format!("http://{addr}/fetch.html"), rules(addr, r#"[
        {"id":"script","match":{"url":"http://ADDR/held.js"},"action":{"type":"hold"}},
        {"id":"owned","match":{"url":"http://ADDR/owned"},"action":{"type":"mock","status":200,"body":"ok"}}
    ]"#)).await;
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    g.eval_main("window.worker=new Worker('/held.js');true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let path = r.runs.join(r.run).join("interceptions.jsonl");
    let read = || BindingJournal::read(&path).unwrap().into_iter().filter_map(|entry| match entry {
        BindingEntry::Bound(b) if b.url.ends_with("/held.js") || b.url.ends_with("/owned") => Some(*b), _ => None,
    }).collect::<Vec<_>>();
    let original = read();
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].worker_lineage.as_ref().unwrap().constructors, vec![1]);
    let endpoint = g.endpoint();
    g.abandon();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert_eq!(read(), original, "adoption must not recalculate an already selected owner");
    g.respond(&original[0].ordinal, MockSpec {
        headers: [("content-type".into(), "text/javascript".into())].into(),
        ..spec("fetch('/owned');")
    }).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let after = read();
    assert_eq!(after.len(), 2);
    assert_eq!(after[1].worker_lineage, original[0].worker_lineage);
    g.stop().await.unwrap();
    std::fs::remove_dir_all(&r.runs).unwrap();
}

#[tokio::test]
async fn a_worker_request_owner_is_frozen_before_its_pause_and_restored_without_a_binding() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, WorkerLineage};
    for main_script in [false, true] {
        let addr = support::serve();
        let run = if main_script { "unbound-worker-script-owner" } else { "unbound-worker-owner" };
        let (mut g, r) = launch(run, format!("http://{addr}/frames_idle.html"), rules(addr, r#"[
            {"id":"owned","match":{"url":"http://ADDR/owned"},"action":{"type":"hold"}},
            {"id":"fast","match":{"url":"http://ADDR/fast"},"action":{"type":"hold"}}
        ]"#)).await;
        g.eval_main("window.got=[];true").await.unwrap();
        if !main_script {
            let script = format!("onmessage=()=>fetch('http://{addr}/owned').then(r=>r.text()).then(body=>postMessage(body+'@'+(Date.now()-946684800000)));postMessage('ready');");
            g.eval_main(&format!("window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=e=>got.push(e.data);true", serde_json::json!(script))).await.unwrap();
        }
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        assert_eq!(g.eval_main("got").await.unwrap(), if main_script { serde_json::json!([]) } else { serde_json::json!(["ready"]) });
        let endpoint = g.endpoint();
        let observer = native(&endpoint).await;
        g.delay_interceptions("/owned", std::time::Duration::from_millis(1000));
        g.eval_main(if main_script {
            "window.worker=new Worker('/owned');worker.onmessage=e=>got.push(e.data);fetch('/fast');true"
        } else { "worker.postMessage('go');fetch('/fast');true" }).await.unwrap();
        observer.call(&cdp::protocol::emulation::FrontcapDrainCurrentTasksParams::default()).await.unwrap();
        let info = observer.client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos
            .into_iter().find(|info| info.r#type == "worker").unwrap();
        let expected = WorkerLineage { creator_document: info.frontcap_creator_document_id.unwrap(), constructors: vec![1] };
        observer.client.close();
        let path = r.runs.join(r.run).join("interceptions.jsonl");
        // End the controller while collection owns a still-unselected request.
        let pending = tokio::time::timeout(std::time::Duration::from_millis(150), g.advance(VirtualDuration::from_ms(0))).await.is_err();
        let before = BindingJournal::read(&path).unwrap();
        let frozen: Vec<_> = before.iter().filter_map(|entry| match entry {
            BindingEntry::WorkerRequestOwner { network_id, lineage, .. } => Some((network_id.clone(), lineage.clone())), _ => None,
        }).collect();
        let selected_early = before.iter().any(|entry| matches!(entry, BindingEntry::Bound(b) if b.url.ends_with("/owned")));
        g.abandon();
        let mut g = reconnect(&r, endpoint).await.unwrap();
        let after = BindingJournal::read(&path).unwrap();
        let bound = after.iter().find_map(|entry| match entry {
            BindingEntry::Bound(binding) if binding.url.ends_with("/owned") => Some(binding.clone()), _ => None,
        }).expect("the retained unbound pause is adopted");
        let owners_after: Vec<_> = after.iter().filter_map(|entry| match entry {
            BindingEntry::WorkerRequestOwner { network_id, lineage, .. } => Some((network_id.clone(), lineage.clone())), _ => None,
        }).collect();
        let answer = if main_script { MockSpec {
            headers: [("content-type".into(), "text/javascript".into())].into(), ..spec("postMessage('restored@'+(Date.now()-946684800000))")
        } } else { spec("restored") };
        g.respond(&bound.ordinal, answer).await.unwrap();
        g.advance(VirtualDuration::from_ms(0)).await.unwrap();
        let got = g.eval_main("got").await.unwrap();
        g.stop().await.unwrap();
        assert!(pending && !selected_early, "ownership must be frozen before response selection");
        assert_eq!(frozen, vec![(bound.network_id.clone().unwrap(), expected.clone())]);
        assert_eq!(owners_after, frozen, "recovery does not rewrite or duplicate frozen ownership");
        assert_eq!(bound.worker_lineage, Some(expected));
        assert_eq!(bound.issued_us, 1_000_000);
        assert_eq!(got, if main_script { serde_json::json!(["restored@1000"]) } else { serde_json::json!(["ready", "restored@1000"]) });
        std::fs::remove_dir_all(&r.runs).unwrap();
    }
}

/// Requests a controller collected but never selected are adopted in the declared issuer order, from
/// the owners and creator-document places frozen before it ended: the constructor order, not the
/// order the workers issued them in.
#[tokio::test]
async fn unselected_worker_requests_are_adopted_in_their_constructor_order() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, DocumentPlace};
    let page = "https://adopt-order.example.test/";
    let (mut g, r) = launch("adopted-order", page.into(), RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":page},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>adopt</p>"}},
        {"id":"sequence","match":{"url":"https://adopt-order.example.test/sequence*"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"1"},{"status":200,"body":"2"}]}}
    ]).to_string()).unwrap()).await;
    // `two` issues first; only then does the page tell `one` to issue.
    let worker = "onmessage=e=>{fetch(e.data+'/sequence?'+name,{method:'POST'}).then(r=>r.text()).then(b=>postMessage({name,b}));postMessage({issued:name});};";
    g.eval_main(&format!("window.got={{}};const url=URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}}));\
        window.workers={{}};for(const name of ['one','two']){{const w=new Worker(url,{{name}});w.onmessage=e=>{{\
        if(e.data.issued==='two')workers.one.postMessage(location.origin);else if(e.data.b)got[e.data.name]=e.data.b;}};workers[name]=w;}};true", serde_json::json!(worker))).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    // The controller ends while collection still waits for `one`'s interception.
    g.delay_interceptions("sequence?one", std::time::Duration::from_millis(1000));
    g.eval_main("workers.two.postMessage(location.origin);true").await.unwrap();
    let pending = tokio::time::timeout(std::time::Duration::from_millis(150), g.advance(VirtualDuration::from_ms(0))).await.is_err();
    let path = r.runs.join(r.run).join("interceptions.jsonl");
    let before = BindingJournal::read(&path).unwrap();
    let places: Vec<_> = before.iter().filter_map(|entry| match entry {
        BindingEntry::WorkerRequestOwner { lineage, document, .. } => Some((lineage.constructors.clone(), document.clone())), _ => None,
    }).collect();
    let selected_early = before.iter().any(|entry| matches!(entry, BindingEntry::Bound(b) if b.url.contains("/sequence")));
    let endpoint = g.endpoint();
    g.abandon();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let got = g.eval_main("got").await.unwrap();
    g.stop().await.unwrap();
    assert!(pending && !selected_early, "both requests must still be unselected when the controller ends");
    let top = Some(DocumentPlace::Current { window: 0, frames: vec![] });
    assert_eq!(places.len(), 2, "{places:?}");
    assert!(places.iter().all(|(_, document)| *document == top), "{places:?}");
    assert_eq!(got, serde_json::json!({"one":"1","two":"2"}));
    std::fs::remove_dir_all(&r.runs).unwrap();
}

/// Requests workers and the page issue at one instant of a grant whose recorder ended before reading
/// anything have no frozen owners: each paused request names its worker, so adoption still decides
/// them in the declared order (the page's thread, then workers by constructor), whatever order they
/// were issued in.
#[tokio::test]
async fn requests_no_recorder_read_are_adopted_in_their_issuers_order() {
    let page = "https://adopt-unseen.example.test/";
    let (mut g, r) = launch("adopted-unseen", page.into(), RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":page},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>adopt</p>"}},
        {"id":"sequence","match":{"url":"https://adopt-unseen.example.test/sequence*"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"1"},{"status":200,"body":"2"},{"status":200,"body":"3"}]}}
    ]).to_string()).unwrap()).await;
    // `two` issues first, then the page, then `one`: the reverse of the declared order.
    let worker = "onmessage=e=>{fetch(e.data+'/sequence?'+name,{method:'POST'}).then(r=>r.text()).then(b=>postMessage({name,b}));postMessage({issued:name});};";
    g.eval_main(&format!("window.got={{}};window.issued=[];const url=URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}}));\
        window.workers={{}};for(const name of ['one','two']){{const w=new Worker(url,{{name}});w.onmessage=e=>{{\
        if(e.data.issued){{issued.push(e.data.issued);if(e.data.issued==='two'){{fetch('/sequence?page',{{method:'POST'}}).then(r=>r.text()).then(b=>got.page=b);issued.push('page');workers.one.postMessage(location.origin);}}}}\
        else if(e.data.b)got[e.data.name]=e.data.b;}};workers[name]=w;}};true", serde_json::json!(worker))).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    g.eval_main("workers.two.postMessage(location.origin);true").await.unwrap();
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(0)).await.unwrap();
    let n = native(&endpoint).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while n.eval("issued.length", None).await.unwrap() != serde_json::json!(3) {
        assert!(std::time::Instant::now() < deadline, "the page and its workers issue in the abandoned grant");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    n.client.close();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let got = g.eval_main("got").await.unwrap();
    g.stop().await.unwrap();
    let (requests, _) = records(&r, "");
    let sequence: Vec<_> = requests.iter().filter(|(id, _)| id != "req-1").map(|(_, t)| *t).collect();
    assert_eq!(sequence, vec![VirtualTime(1_000_000); 3], "issued at the stopped instant");
    // Recovery froze both workers' owners, in the launch page's top document.
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, DocumentPlace};
    let owners: Vec<_> = BindingJournal::read(&r.runs.join(r.run).join("interceptions.jsonl")).unwrap().into_iter().filter_map(|entry| match entry {
        BindingEntry::WorkerRequestOwner { lineage, document, issued_us, .. } => Some((lineage.constructors, document, issued_us)), _ => None,
    }).collect();
    let top = Some(DocumentPlace::Current { window: 0, frames: vec![] });
    assert_eq!(owners.len(), 2, "{owners:?}");
    assert!(owners.iter().all(|(_, document, issued)| *document == top && *issued == 1_000_000), "{owners:?}");
    assert_eq!(got, serde_json::json!({"page":"1","one":"2","two":"3"}));
    std::fs::remove_dir_all(&r.runs).unwrap();
}

/// A worker blocked in a synchronous load whose answer the dead recorder held: the next recorder
/// adopts the hold and the worker's other loads, and once answered the worker goes on.
#[tokio::test]
async fn a_worker_blocked_in_a_held_synchronous_load_continues_under_the_next_recorder() {
    blocked_worker_load_is_adopted("worker-sync-adopt", "importScripts(o+'/held.js');", "globalThis.loaded='yes';").await;
}

#[tokio::test]
async fn a_worker_blocked_in_a_held_synchronous_xhr_continues_under_the_next_recorder() {
    blocked_worker_load_is_adopted("worker-xhr-adopt",
        "const x=new XMLHttpRequest();x.open('GET',o+'/held.js',false);x.send();globalThis.loaded=x.responseText;", "yes").await;
}

async fn blocked_worker_load_is_adopted(run: &'static str, load: &str, body: &str) {
    let addr = support::serve();
    let (mut g, r) = launch(run, format!("http://{addr}/fetch.html"), rules(addr, r#"[
        {"id":"script","match":{"url":"http://ADDR/held.js"},"action":{"type":"hold"}},
        {"id":"before","match":{"url":"http://ADDR/before"},"action":{"type":"mock","status":200,"body":"before","latency_ms":50}},
        {"id":"after","match":{"url":"http://ADDR/after"},"action":{"type":"mock","status":200,"body":"after"}}
    ]"#)).await;
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    let worker = format!("onmessage=e=>{{const o=e.data;fetch(o+'/before').then(r=>r.text()).then(b=>postMessage(b));\
        {load}fetch(o+'/after').then(r=>r.text()).then(b=>postMessage(b+':'+globalThis.loaded));}};");
    g.eval_main(&format!("window.got=[];window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));\
        worker.onmessage=e=>got.push(e.data);worker.postMessage(location.origin);true", serde_json::json!(worker))).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let held = g.held();
    assert_eq!(held.len(), 1, "the worker waits in its synchronous load");
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!([]), "a blocked worker handles no answer");
    let endpoint = g.endpoint();
    g.abandon();
    let mut g = reconnect(&r, endpoint).await.unwrap();
    assert_eq!(g.held(), held, "the hold is adopted");
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!([]), "still blocked under the next recorder");
    g.respond(&held[0], MockSpec { headers: [("content-type".into(), "text/javascript".into())].into(), ..spec(body) }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["before", "after:yes"]));
    let (requests, _) = records(&r, "");
    let urls: Vec<_> = requests.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(urls.len(), 4, "page, before, held script and after, each recorded once: {requests:?}");
    g.stop().await.unwrap();
    std::fs::remove_dir_all(&r.runs).unwrap();
}

#[tokio::test]
async fn a_workers_unread_synchronous_load_is_adopted_while_it_waits() {
    use cdp::protocol::emulation;
    for (run, load, body) in [
        ("unread-import", "importScripts(o+'/held');postMessage(globalThis.loaded);", "globalThis.loaded='answer';"),
        ("unread-xhr", "const x=new XMLHttpRequest();x.open('GET',o+'/held',false);x.send();postMessage(x.responseText);", "answer"),
    ] {
        let addr = support::serve();
        let (mut g, r) = launch(run, format!("http://{addr}/fetch.html"), rules(addr,
            r#"[{"id":"held","match":{"url":"http://ADDR/held"},"action":{"type":"hold"}}]"#)).await;
        g.advance(VirtualDuration::from_ms(10)).await.unwrap();
        let worker = format!("onmessage=e=>{{const o=e.data;{load}}};");
        g.eval_main(&format!("window.got=[];window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));\
            worker.onmessage=e=>got.push(e.data);true", serde_json::json!(worker))).await.unwrap();
        g.advance(VirtualDuration::from_ms(0)).await.unwrap();
        let issued = g.now();
        g.eval_main("worker.postMessage(location.origin);true").await.unwrap();
        let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(0)).await.unwrap();
        let n = native(&endpoint).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let workers = n.call(&emulation::FrontcapGetWorkersParams::default()).await.unwrap().workers;
            if workers.iter().any(|w| w.thread_started && w.synchronous_wait_id.is_some()) { break; }
            assert!(std::time::Instant::now() < deadline, "{run}: worker must actually enter its synchronous wait");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(records(&r, "/held").0.is_empty(), "the old controller never recorded the request");
        n.client.close();
        let mut g = reconnect(&r, endpoint).await.unwrap();
        let held = g.held();
        assert_eq!(held.len(), 1, "{run}: adoption binds the unread hold");
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!([]));
        g.respond(&held[0], MockSpec {
            headers: [("content-type".into(), "text/javascript".into())].into(), ..spec(body)
        }).await.unwrap();
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!(["answer"]));
        let (requests, responses) = records(&r, "/held");
        assert_eq!(requests, vec![(held[0].clone(), issued)], "one request at its original instant");
        assert_eq!(responses.len(), 1, "one response after recovery");
        g.stop().await.unwrap();
        std::fs::remove_dir_all(&r.runs).unwrap();
    }
}
