//! Windows the page opens under the `controlled` popup policy: each is a context of the generation,
//! enrolled before it runs anything, on the generation's clock, rules and evidence.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::{ContextEvent, EventKind, ResponseSource, TaintSource};
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::Locator;
use frontcap_core::profile::PopupPolicy;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;
use serde_json::json;

const OPENER: &str = r#"<!doctype html><button id=open onclick="openIt()">sign in</button>
<button id=later onclick="setTimeout(openIt, 100)">later</button>
<button id=second onclick="window.open('https://auth.example.test/login', 'second')">second</button>
<button id=noopener onclick="window.log.push(['noopener', window.open('https://auth.example.test/login', '_blank', 'noopener') === null])">noopener</button>
<script>
window.log = [];
addEventListener('message', e => log.push(['message', e.data, e.origin, Date.now()]));
function openIt() { window.w = window.open('https://auth.example.test/login', 'auth'); log.push(['opened', w !== null, Date.now()]); }
</script>"#;

const SIGN_IN: &str = r#"<!doctype html><label>name <input id=name></label><button id=go onclick="opener.postMessage('signed in as ' + document.getElementById('name').value, '*'); window.close()">continue</button>"#;

const POPUP: &str = r#"<!doctype html><p>signing in</p><script>
console.log('popup ready');
setTimeout(() => { throw new Error('popup boom'); }, 100);
opener.postMessage('loaded ' + Date.now() + ' ' + Math.random(), '*');
setTimeout(() => { opener.postMessage('later ' + Date.now(), '*'); if (location.hash !== '#stay') window.close(); }, 200);
</script>"#;

fn rules(popup: &str) -> RuleSet {
    let mock = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    RuleSet::from_json(&json!([mock("app", "https://app.example.test/", OPENER), mock("login", "https://auth.example.test/login", popup),
        {"id": "token", "match": {"url": "https://auth.example.test/token"}, "action": {"type": "mock", "status": 200, "body": "t-42"}}]).to_string()).unwrap()
}

async fn launched(run: &str, popup: &str) -> Option<(tempfile::TempDir, Generation)> {
    if !support::controlled_engine_or_check_baseline().await {
        return None;
    }
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), run, "https://app.example.test/".into(), rules(popup));
    options.passthrough_origins = Some(vec![]);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    Some((runs, g))
}

async fn page_log(g: &Generation) -> Vec<serde_json::Value> {
    serde_json::from_str(g.eval_main("JSON.stringify(window.log)").await.unwrap().as_str().unwrap()).unwrap()
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

#[tokio::test]
async fn a_popup_runs_on_the_generation_clock_and_rules() {
    let Some((_runs, mut g)) = launched("popup-1", POPUP).await else { return };
    let opened_at = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    let log = page_log(&g).await;
    let date = |t: frontcap_core::time::VirtualTime| 946_684_800_000 + t.0 / 1000;
    assert_eq!(log[0], json!(["opened", true, date(opened_at)]), "{log:?}");
    let loaded = &log[1];
    assert_eq!(loaded[2], "https://auth.example.test");
    let text = loaded[1].as_str().unwrap();
    let popup_now: f64 = text.split(' ').nth(1).unwrap().parse().unwrap();
    assert_eq!(popup_now, loaded[3].as_f64().unwrap(), "the popup and its opener read one clock: {log:?}");
    // The popup's timer ran 200 ms later by the same clock.
    assert_eq!(log[2][1], format!("later {}", popup_now + 200.0), "{log:?}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let opened = events.iter().find_map(|e| match &e.kind {
        EventKind::Context(c) if c.event == ContextEvent::Opened => Some((e.t_us, c.clone())),
        _ => None,
    }).expect("the popup is a context");
    assert_eq!(opened.1.context, "popup-1");
    assert_eq!(opened.1.opener.as_deref(), Some("page"));
    assert_eq!(opened.0, opened_at, "opened at the click's instant");
    let order: Vec<&str> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Context(_) => Some("context"),
        EventKind::Request(r) if r.context.is_some() => Some("request"),
        _ => None,
    }).collect();
    assert_eq!(&order[..2], ["context", "request"], "a popup's opening precedes its requests");
    let request = events.iter().find_map(|e| match &e.kind {
        EventKind::Request(r) if r.url == "https://auth.example.test/login" => Some(r.clone()),
        _ => None,
    }).expect("the popup's document request is the generation's");
    assert_eq!(request.context.as_deref(), Some("popup-1"));
    assert_eq!(request.rule.as_ref().map(|r| r.as_str()), Some("login"));
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Response(r) if r.id == request.id && r.source == ResponseSource::Mock)));
    let console = events.iter().find_map(|e| match &e.kind {
        EventKind::Console(c) if c.text == "popup ready" => Some((e.t_us, c.context.clone())),
        _ => None,
    });
    assert_eq!(console, Some((opened_at, Some("popup-1".into()))), "{events:?}");
    let thrown = events.iter().find_map(|e| match &e.kind {
        EventKind::Exception(x) if x.text.contains("popup boom") => Some((e.t_us, x.context.clone())),
        _ => None,
    });
    assert_eq!(thrown, Some((opened_at.saturating_add(VirtualDuration::from_ms(100)), Some("popup-1".into()))), "{events:?}");
    let closed = events.iter().any(|e| matches!(&e.kind, EventKind::Context(c) if c.context == "popup-1" && c.event == ContextEvent::Closed));
    assert!(closed, "the popup closed itself");
    g.stop().await.unwrap();
}

/// A window's first document commits before the input that opened it completes, however long its
/// network takes: its navigation is read at that input's boundary, ahead of anything later at the same
/// instant, never at whichever later boundary wall time brings it to (where a replay would order it
/// differently).
#[tokio::test]
async fn a_popup_commits_before_the_input_that_opened_it_completes() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            std::thread::spawn(move || {
                let slow = req.url().starts_with("/slow");
                let body = if slow { "<!doctype html><p>slow sign-in</p>" } else { r#"<!doctype html><button id=open onclick="window.open('/slow', 'auth')">sign in</button>"# };
                if slow { std::thread::sleep(std::time::Duration::from_millis(300)); }
                let _ = req.respond(tiny_http::Response::from_string(body).with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
            });
        }
    });
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "popup-slow", format!("http://127.0.0.1:{port}/"), RuleSet::default());
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let clicked = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let navigation = events.iter().find(|e| matches!(&e.kind, EventKind::Navigation(n) if n.context.as_deref() == Some("popup-1")));
    assert_eq!(navigation.map(|e| e.t_us), Some(clicked), "committed at the click's instant before it completed: {events:?}");
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.evaluate("document.body.textContent").await.unwrap(), "slow sign-in");
    g.leave_context();
    g.stop().await.unwrap();
}

/// A window opened without an opener (a `target=_blank` link, `window.open(…, 'noopener')`) does not
/// stop its opener: it opens at the native creation instant, its document by the rules, committed before the
/// clock moves, its scripts on the generation's clock from the next grant.
#[tokio::test]
async fn windows_without_an_opener_open_at_the_native_creation_boundary() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let opener = r#"<!doctype html><a id=help href="https://auth.example.test/help?link" target=_blank>Help</a>
<button id=later onclick="setTimeout(() => window.log.push(['opened', window.open('https://auth.example.test/help?timer', '_blank', 'noopener')]), 30)">later</button>
<script>window.log = [];</script>"#;
    let help = r#"<!doctype html><p>Help</p><script>window.loadedAt = Date.now(); window.hasOpener = window.opener !== null; setTimeout(() => window.later = Date.now(), 200);</script>"#;
    let mock = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = RuleSet::from_json(&json!([mock("app", "https://app.example.test/", opener), mock("help", "https://auth.example.test/help*", help)]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "popup-noopener", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let date = |t: frontcap_core::time::VirtualTime| 946_684_800_000 + t.0 / 1000;

    let clicked = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#help")).await.unwrap();
    g.advance(VirtualDuration::from_ms(250)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    let seen = g.evaluate("[window.loadedAt, window.hasOpener, window.later, location.search]").await.unwrap();
    g.leave_context();
    assert_eq!(seen, json!([date(clicked), false, date(clicked) + 200, "?link"]), "opened at the click, on the clock");

    let pressed = g.now();
    g.click(RequestId::new("c-2").unwrap(), &css("#later")).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(page_log(&g).await, vec![json!(["opened", null])], "the page's window.open returns null at once");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let opened: Vec<(u64, String, bool)> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Context(c) if c.event == ContextEvent::Opened => Some((e.t_us.0, c.context.clone(), c.no_opener)),
        _ => None,
    }).collect();
    assert_eq!(opened[0], (clicked.0, "popup-1".to_string(), true), "{opened:?}");
    let (timer_at, ref id, no_opener) = opened[1];
    assert_eq!((id.as_str(), no_opener), ("popup-2", true));
    assert_eq!(timer_at, pressed.0 + 30_000, "native admission stops at creation");
    g.enter_context("popup-2").unwrap();
    let loaded = g.evaluate("window.loadedAt").await.unwrap();
    g.leave_context();
    assert_eq!(loaded, json!(timer_at / 1000 + 946_684_800_000));
    // Its document request is recorded after its opening, in its context, under the rules.
    let help_requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Request(r) if r.url.starts_with("https://auth.example.test/help") => Some((e.t_us.0, r.context.clone())),
        _ => None,
    }).collect();
    assert_eq!(help_requests, vec![(clicked.0, Some("popup-1".into())), (timer_at, Some("popup-2".into()))]);
    assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Taint(t) if t.source == TaintSource::NewWindow)), "{events:?}");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn commands_address_a_popup_by_context_and_by_reference() {
    let Some((_runs, mut g)) = launched("popup-commands", SIGN_IN).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert!(matches!(g.enter_context("popup-2"), Err(frontcap_chromium::ChromiumError::NoSuchContext(_))));
    g.enter_context("popup-1").unwrap();
    let snapshot = g.snapshot(None, None, 100).await.unwrap();
    let button = snapshot.nodes.iter().find(|n| n.name == "continue").expect("the popup button").r#ref.clone();
    // The launch page's snapshot does not have it; its reference names the popup.
    g.leave_context();
    let page = g.snapshot(None, None, 100).await.unwrap();
    assert!(page.nodes.iter().all(|n| n.name != "continue"));
    let refused = g.click(RequestId::new("c-2").unwrap(), &Locator::Ref { id: button.clone() }).await;
    assert!(matches!(refused, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))), "{refused:?}");
    g.enter_context("popup-1").unwrap();
    g.act_input(RequestId::new("c-3").unwrap(), &frontcap_core::action::Action::Type { target: css("#name"), text: "ada".into() }).await.unwrap();
    g.click(RequestId::new("c-4").unwrap(), &Locator::Ref { id: button }).await.unwrap();
    g.leave_context();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!(log.last().unwrap()[1], "signed in as ada", "{log:?}");
    assert!(matches!(g.enter_context("popup-1"), Err(frontcap_chromium::ChromiumError::ContextClosed(_))));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let actions: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Action(a) => Some(a.request_id.as_str().to_string()),
        _ => None,
    }).collect();
    assert!(actions.contains(&"c-4".to_string()), "{actions:?}");
    assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Taint(t) if t.source == TaintSource::UninterceptedRequest)), "{events:?}");
    g.stop().await.unwrap();
}

const TOKEN: &str = r#"<!doctype html><button id=go onclick="fetch('https://auth.example.test/token').then(r => r.text()).then(t => opener.postMessage('token ' + t, '*'))">continue</button>"#;

#[tokio::test]
async fn a_relaunched_recorder_takes_back_open_popups() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let runs = tempfile::tempdir().unwrap();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(runs.path(), "popup-recovery", "https://app.example.test/".into(), rules(TOKEN));
    options.passthrough_origins = Some(vec![]);
    options.profile.popups = PopupPolicy::Controlled;
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let mut g = Generation::launch(options).await.unwrap();
    let launch_id = g.launch_id().clone();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    let original_document = g.snapshot(None, None, 100).await.unwrap().document;
    g.evaluate("history.pushState({}, '', '#first');history.pushState({}, '', '#second');true").await.unwrap();
    g.leave_context();
    let endpoint = g.endpoint();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (mut g, _) = Generation::reconnect(frontcap_chromium::generation::ReconnectOptions {
        repo_root: support::repo_root(), runs_root: runs.path().into(), run: frontcap_core::ids::RunId::new("popup-recovery").unwrap(),
        launch_id, profile, endpoint, watches: Default::default(),
        watchdog: std::time::Duration::from_secs(10), granted_until: None, dialog_rules: Default::default(), upstream: None,
    }).await.unwrap();
    // The open popup is the generation's again: commands reach it, and its requests follow the rules.
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.snapshot(None, None, 100).await.unwrap().document, original_document, "history entries must not become new documents on adoption");
    g.click(RequestId::new("c-2").unwrap(), &css("#go")).await.unwrap();
    g.leave_context();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(page_log(&g).await.last().unwrap()[1], "token t-42");
    // A window opened after the relaunch is enrolled with the next number.
    g.click(RequestId::new("c-3").unwrap(), &css("#second")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let contexts = g.contexts().await.unwrap();
    assert_eq!(contexts.iter().map(|c| (c.id.as_str(), c.open)).collect::<Vec<_>>(), [("page", true), ("popup-1", true), ("popup-2", true)]);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let token = events.iter().find_map(|e| match &e.kind {
        EventKind::Request(r) if r.url.ends_with("/token") => Some(r.clone()),
        _ => None,
    }).expect("the popup's request after the relaunch is the generation's");
    assert_eq!((token.context.as_deref(), token.rule.as_ref().map(|r| r.as_str())), (Some("popup-1"), Some("token")));
    let opened: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Context(c) if c.event == ContextEvent::Opened => Some(c.context.clone()),
        _ => None,
    }).collect();
    assert_eq!(opened, ["popup-1", "popup-2"]);
    let navigated: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Navigation(n) if n.context.as_deref() == Some("popup-1") && n.rseq != 0 => Some((n.url.rsplit('#').next().unwrap(), n.document)),
        _ => None,
    }).collect();
    assert_eq!(navigated.iter().map(|(url, _)| *url).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(navigated[0].1, navigated[1].1, "history entries keep the same document");
    assert_eq!(navigated[1].1, original_document);
    assert!(g.engine_receipts().await.unwrap().is_empty(), "recorded popup receipts must be forgotten in their owning sessions");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_next_event_wait_stops_at_a_popup_before_its_document() {
    let Some((_runs, mut g)) = launched("popup-next", POPUP).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#later")).await.unwrap();
    let clicked = g.now();
    let arm = g.arm_event(frontcap_core::event::ProgressEvent::Popup).await.unwrap();
    let outcome = g.next_event(frontcap_core::event::ProgressEvent::Popup, &arm, clicked.saturating_add(VirtualDuration::from_ms(500))).await.unwrap();
    let frontcap_core::event::ClockOutcome::Event { triggers, .. } = outcome else { panic!("{outcome:?}") };
    assert!(matches!(&triggers[..], [frontcap_core::event::ProgressTrigger::Popup { context, opener, .. }] if context == "popup-1" && opener == "page"), "{triggers:?}");
    assert_eq!(g.now(), clicked.saturating_add(VirtualDuration::from_ms(100)), "stopped at the instant it was opened");
    // Stopped before its document: the window is open, its document request held.
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.evaluate("location.href").await.unwrap(), "about:blank");
    g.leave_context();
    assert_eq!(page_log(&g).await.len(), 1, "the popup has not run");
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let log = page_log(&g).await;
    assert!(log[1][1].as_str().unwrap().starts_with("loaded "), "{log:?}");
    g.stop().await.unwrap();
}

/// A popup stop at a window without an opener: at the boundary it opened, its first document committed
/// (it has no frame to answer before), its scripts not yet run.
#[tokio::test]
async fn a_next_event_wait_stops_where_a_window_without_an_opener_opened() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let opener = r#"<!doctype html><button id=later onclick="setTimeout(() => window.open('https://auth.example.test/help', '_blank', 'noopener'), 30)">later</button>"#;
    let help = r#"<!doctype html><p>Help</p><script>window.loadedAt = Date.now();</script>"#;
    let mock = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = RuleSet::from_json(&json!([mock("app", "https://app.example.test/", opener), mock("help", "https://auth.example.test/help", help)]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "popup-noopener-next", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &css("#later")).await.unwrap();
    let clicked = g.now();
    let arm = g.arm_event(frontcap_core::event::ProgressEvent::Popup).await.unwrap();
    let outcome = g.next_event(frontcap_core::event::ProgressEvent::Popup, &arm, clicked.saturating_add(VirtualDuration::from_ms(500))).await.unwrap();
    let frontcap_core::event::ClockOutcome::Event { triggers, .. } = outcome else { panic!("{outcome:?}") };
    assert!(matches!(&triggers[..], [frontcap_core::event::ProgressTrigger::Popup { context, .. }] if context == "popup-1"), "{triggers:?}");
    let stopped = g.now();
    assert_eq!(stopped.0, clicked.0 + 30_000, "native admission stops at creation");
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.evaluate("[location.href, typeof window.loadedAt]").await.unwrap(), json!(["https://auth.example.test/help", "undefined"]));
    g.leave_context();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.evaluate("window.loadedAt").await.unwrap(), json!(946_684_800_000 + stopped.0 / 1000));
    g.leave_context();
    g.stop().await.unwrap();
}

const STREAMING: &str = r#"<!doctype html><script>
const s = new EventSource('https://auth.example.test/events');
s.onerror = () => { s.close(); opener.postMessage('stream failed', '*'); };
</script>"#;

/// A popup's stream follows the rules as the launch page's does: with no rule letting it through, it is
/// refused as a connection that could not be made, and nothing marks it unsupported.
#[tokio::test]
async fn a_popup_stream_follows_the_rules() {
    let Some((_runs, mut g)) = launched("popup-stream", STREAMING).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(page_log(&g).await.last().unwrap()[1], "stream failed");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Taint(t) if t.source == TaintSource::UnsupportedStream)), "{events:?}");
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Stream(s) if s.context.as_deref() == Some("popup-1"))), "{events:?}");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn input_reaches_a_popup_of_the_opener_origin() {
    // Its first document reuses the window it opened with (same origin): the observer adopts it again.
    let opener = r#"<!doctype html><button id=open onclick="window.w = window.open('/login', 'auth')">go</button>"#;
    let popup = r#"<!doctype html><p>Sign in</p><button id=allow onclick="this.textContent = 'clicked'">Allow</button>"#;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mock = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = RuleSet::from_json(&json!([mock("app", "https://app.example.test/", opener), mock("login", "https://app.example.test/login", popup)]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "popup-same-origin", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(250)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    g.click(RequestId::new("c-2").unwrap(), &css("#allow")).await.unwrap();
    assert_eq!(g.evaluate("document.getElementById('allow').textContent").await.unwrap(), "clicked");
    g.leave_context();
    g.stop().await.unwrap();
}

const CLOSABLE: &str = r#"<!doctype html><label>draft <input id=draft></label><script>
addEventListener('beforeunload', e => { if (document.getElementById('draft').value) { e.preventDefault(); e.returnValue = ''; } });
addEventListener('pagehide', () => opener.postMessage('closing ' + Date.now(), '*'));
</script>"#;

/// The user closes a popup: its prompt asks first (staying keeps the window), its unload handlers run
/// at the action's instant, and its opener sees it closed on its next look.
#[tokio::test]
async fn a_user_closes_a_popup_after_its_prompt() {
    use frontcap_core::dialog::{DialogAnswer, DialogRule, DialogRules};
    use frontcap_core::event::DialogKind;
    let Some((_runs, mut g)) = launched("popup-close", CLOSABLE).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.eval_main("setInterval(() => { if (w.closed && !window.seen) { window.seen = true; log.push(['closed', Date.now()]); } }, 100); true").await.unwrap();
    let stay = DialogRule { kind: Some(DialogKind::Beforeunload), message: None, answer: DialogAnswer::Dismiss, text: None };
    g.set_dialog_rules(DialogRules(vec![stay])).await.unwrap();
    g.enter_context("popup-1").unwrap();
    g.act_input(RequestId::new("c-2").unwrap(), &frontcap_core::action::Action::Type { target: css("#draft"), text: "unsent".into() }).await.unwrap();
    assert!(!g.close_window(&RequestId::new("x-1").unwrap()).await.unwrap(), "the user stayed");
    g.leave_context();
    g.set_dialog_rules(DialogRules::default()).await.unwrap();
    g.enter_context("popup-1").expect("still open");
    let at = g.now();
    assert!(g.close_window(&RequestId::new("x-2").unwrap()).await.unwrap());
    g.leave_context();
    assert!(matches!(g.enter_context("popup-1"), Err(frontcap_chromium::ChromiumError::ContextClosed(_))));
    assert!(matches!(g.close_window(&RequestId::new("x-3").unwrap()).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))), "the launch page is not closed");
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let date = |t: frontcap_core::time::VirtualTime| 946_684_800_000 + t.0 / 1000;
    let log = page_log(&g).await;
    let closing = log.iter().find(|e| e[0] == "message" && e[1].as_str().is_some_and(|m| m.starts_with("closing"))).expect("pagehide ran");
    assert_eq!(closing[1], format!("closing {}", date(at)), "{log:?}");
    let seen = log.iter().find(|e| e[0] == "closed").expect("the opener saw it closed");
    assert!(seen[1].as_u64().unwrap() > date(at) && seen[1].as_u64().unwrap() <= date(at) + 100, "{log:?}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let story: Vec<String> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Action(a) if a.action == "close" => Some(format!("{} {} {:?}", e.t_us.0, a.request_id.as_str(), a.context)),
        EventKind::Dialog(d) if d.kind == DialogKind::Beforeunload => Some(format!("{} {} {:?}", e.t_us.0, d.answer, d.context)),
        EventKind::Context(c) if c.event == ContextEvent::Closed => Some(format!("{} {} closed", e.t_us.0, c.context)),
        _ => None,
    }).collect();
    let (t, popup) = (at.0, Some("popup-1"));
    assert_eq!(story, vec![
        format!("{t} x-1 {popup:?}"), format!("{t} dismissed {popup:?}"),
        format!("{t} x-2 {popup:?}"), format!("{t} accepted {popup:?}"), format!("{t} popup-1 closed"),
    ], "{events:?}");
    g.stop().await.unwrap();
}

/// The native input reply can be lost when its handler closes the target. The
/// completion must retain the action and closure once, without advancing time.
#[tokio::test]
async fn a_closing_click_completes_when_detach_precedes_its_input_reply() {
    let Some((_runs, mut g)) = launched("popup-closing-reply", SIGN_IN).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let at = g.now();
    g.enter_context("popup-1").unwrap();
    g.omit_mouse_release_answer();
    g.click(RequestId::new("c-2").unwrap(), &css("#go")).await.unwrap();
    g.leave_context();
    assert_eq!(g.now(), at);
    assert!(matches!(g.enter_context("popup-1"), Err(frontcap_chromium::ChromiumError::ContextClosed(_))));
    let trace = g.trace_path();
    g.stop().await.unwrap();
    let events = BehaviorLog::read(&trace).unwrap();
    let actions: Vec<_> = events.iter().filter(|e| matches!(&e.kind,
        EventKind::Action(a) if a.request_id.as_str() == "c-2" && a.context.as_deref() == Some("popup-1"))).collect();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].t_us, at);
    let closes: Vec<_> = events.iter().filter(|e| matches!(&e.kind,
        EventKind::Context(c) if c.context == "popup-1" && c.event == ContextEvent::Closed)).collect();
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0].t_us, at);
}
