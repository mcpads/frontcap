//! Child frames (Plan 24): locators, input, watches, snapshots, and references reach into the frames of a
//! page — an embedded widget, an editor in a frame — through the top frame's observer.
mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::action::Action;
use frontcap_core::event::{ClockOutcome, EventKind, SampleValue};
use frontcap_core::ids::{RequestId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    (runs, g)
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

fn role(r: &str, name: &str) -> Locator {
    Locator::Role { role: r.into(), name: Some(name.into()) }
}

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

/// An expression of the widget's window, evaluated in the page's main world.
async fn widget(g: &Generation, expr: &str) -> serde_json::Value {
    g.eval_main(&format!("document.getElementById('widget').contentWindow.eval({})", serde_json::to_string(expr).unwrap())).await.unwrap()
}

#[tokio::test]
async fn a_control_in_a_child_frame_is_clicked_and_its_result_waited_for() {
    let (_runs, mut g) = launched("cf-1", "child_frames.html").await;
    let start = g.now();
    g.act_input(rid("c-1"), &Action::Click { target: role("button", "Upgrade") }).await.unwrap();
    assert_eq!(widget(&g, "JSON.stringify(log)").await, r#"["pointerdown:go","click:go"]"#);
    let done = WatchName::new("upgraded").unwrap();
    g.watch_add(done.clone(), Predicate::TextEquals { target: css("#out"), text: "upgraded".into() }).await.unwrap();
    assert_eq!(g.until(&done, VirtualDuration::from_ms(1000)).await.unwrap(), ClockOutcome::ConditionMet { watch: done.clone() });
    assert_eq!(g.now().0 - start.0, 100_000, "the widget's 100 ms timer, exactly");
    // Its message reaches the page, and a CSS locator finds the frame's element as well.
    let status = WatchName::new("status").unwrap();
    g.watch_add(status.clone(), Predicate::TextEquals { target: css("#status"), text: "upgraded".into() }).await.unwrap();
    assert_eq!(g.until(&status, VirtualDuration::from_ms(100)).await.unwrap(), ClockOutcome::ConditionMet { watch: status });
    g.act_input(rid("c-2"), &Action::Click { target: css("#go") }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 4);
}

/// A widget an `embed` shows (which has no `contentDocument`) is one page with the rest, as an `object`'s
/// is: locators, watches and focus reach into it, as the snapshot already showed it.
#[tokio::test]
async fn an_embedded_widget_is_located_watched_and_operated() {
    let (_runs, mut g) = launched("cf-embed", "child_frame_embedded.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    assert_eq!(s.nodes.iter().filter(|n| n.role == "button" && n.name == "Upgrade").count(), 3, "{:?}", s.nodes);
    let scoped = |frame: &str| Locator::Frame { frame: Box::new(css(frame)), target: Box::new(css("#go")) };
    // One in a component's shadow root too, which `window.frames` does not list.
    for frame in ["#widget", "#legacy", "#shadowed"] {
        let q = g.query(&scoped(frame), &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
        assert_eq!(q.values["text"], "Upgrade", "{frame}");
    }
    assert!(matches!(g.query(&css("#go"), &[]).await, Err(ChromiumError::Ambiguous { count: 3, .. })), "every widget's button is found");
    g.act_input(rid("c-1"), &Action::Click { target: scoped("#widget") }).await.unwrap();
    let done = WatchName::new("upgraded").unwrap();
    g.watch_add(done.clone(), Predicate::TextEquals { target: Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(css("#out")) }, text: "upgraded".into() }).await.unwrap();
    assert_eq!(g.until(&done, VirtualDuration::from_ms(1000)).await.unwrap(), ClockOutcome::ConditionMet { watch: done });
    g.act_input(rid("f-1"), &Action::Fill { target: Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(css("#coupon")) }, value: "SAVE10".into() }).await.unwrap();
    // Keys without a target go where focus is: inside the embedded widget.
    g.act_input(rid("p-1"), &Action::Press { target: None, key: "Enter".into() }).await.unwrap();
    let q = g.query(&Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(css("#out")) }, &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
    assert_eq!(q.values["text"], "coupon SAVE10");
}

/// Every kind of document a frame shows is one page with the rest for snapshots and locators alike: a
/// `data:` document, one a `sandbox` header confines, an SVG image, plain text, `srcdoc`, an `object`'s
/// and an `embed`'s. A frame whose document was refused (`X-Frame-Options`) shows the browser's empty
/// error page, which has nothing to find.
#[tokio::test]
async fn every_kind_of_frame_document_is_seen_alike_by_snapshots_and_locators() {
    let page = r#"<!doctype html>
<iframe srcdoc="<button id=srcdoc>In srcdoc</button>"></iframe>
<iframe src="data:text/html,<button id=data>In data</button>"></iframe>
<iframe src="https://csp.example.test/"></iframe>
<iframe id=image src="https://img.example.test/x.svg"></iframe>
<iframe src="https://txt.example.test/x.txt"></iframe>
<object data="https://obj.example.test/" type="text/html"></object>
<embed src="https://emb.example.test/" type="text/html">
<iframe id=denied src="https://deny.example.test/"></iframe>"#;
    let mock = |id: &str, url: &str, ctype: &str, extra: serde_json::Value, body: &str| {
        let mut headers = serde_json::json!({"content-type": ctype});
        headers.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": headers, "body": body}})
    };
    let none = serde_json::json!({});
    let rules = RuleSet::from_json(&serde_json::json!([
        mock("app", "https://app.example.test/", "text/html", none.clone(), page),
        mock("csp", "https://csp.example.test/", "text/html", serde_json::json!({"content-security-policy": "sandbox"}), "<button id=csp>In CSP sandbox</button>"),
        mock("img", "https://img.example.test/x.svg", "image/svg+xml", none.clone(), "<svg xmlns='http://www.w3.org/2000/svg' width='80' height='40'><text id='svg' y='20'>In SVG</text></svg>"),
        mock("txt", "https://txt.example.test/x.txt", "text/plain", none.clone(), "In plain text"),
        mock("obj", "https://obj.example.test/", "text/html", none.clone(), "<button id=obj>In object</button>"),
        mock("emb", "https://emb.example.test/", "text/html", none.clone(), "<button id=emb>In embed</button>"),
        mock("deny", "https://deny.example.test/", "text/html", serde_json::json!({"x-frame-options": "DENY"}), "<button id=refused>Denied</button>"),
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "cf-kinds", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let s = g.snapshot(None, None, 400).await.unwrap();
    let names: Vec<&str> = s.nodes.iter().map(|n| n.name.as_str()).collect();
    for name in ["In srcdoc", "In data", "In CSP sandbox", "In SVG", "In plain text", "In object", "In embed"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    assert!(!names.contains(&"Denied"), "{names:?}");
    for (id, text) in [("srcdoc", "In srcdoc"), ("data", "In data"), ("csp", "In CSP sandbox"), ("svg", "In SVG"), ("obj", "In object"), ("emb", "In embed")] {
        let q = g.query(&css(&format!("#{id}")), &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
        assert_eq!(q.values["text"], text, "#{id}");
    }
    assert!(matches!(g.query(&css("#refused"), &[]).await, Err(ChromiumError::NotFound(_))));
    // The refused frame is there, showing nothing.
    let scoped = g.snapshot(Some(&css("#denied")), None, 400).await.unwrap();
    assert_eq!(scoped.nodes.iter().map(|n| (n.role.as_str(), n.depth)).collect::<Vec<_>>(), [("iframe", 0)], "{:?}", scoped.nodes);
    assert_eq!(scoped.nodes[0].value.as_deref(), Some("chrome-error://chromewebdata/"), "the browser's error page");
    // Each frame element's value is the address of its document.
    let frames: Vec<&str> = s.nodes.iter().filter(|n| n.role == "iframe").filter_map(|n| n.value.as_deref()).collect();
    for url in ["about:srcdoc", "https://csp.example.test/", "https://img.example.test/x.svg", "https://txt.example.test/x.txt"] {
        assert!(frames.contains(&url), "{url} in {frames:?}");
    }
}

#[tokio::test]
async fn keys_and_choices_reach_a_child_frame() {
    let (_runs, mut g) = launched("cf-2", "child_frames.html").await;
    g.act_input(rid("f-1"), &Action::Fill { target: role("textbox", "Coupon"), value: "SAVE10".into() }).await.unwrap();
    // Keys without a target go where focus is: inside the frame.
    g.act_input(rid("p-1"), &Action::Press { target: None, key: "Enter".into() }).await.unwrap();
    assert_eq!(widget(&g, "out.textContent").await, "coupon SAVE10");
    g.act_input(rid("s-1"), &Action::Select { target: role("combobox", "Plan"), values: vec!["Pro".into()] }).await.unwrap();
    assert_eq!(widget(&g, "out.textContent").await, "plan pro");
    // An editor that is a frame's editable body.
    g.act_input(rid("t-1"), &Action::Type { target: css("#doc"), text: " and more".into() }).await.unwrap();
    let text = g.eval_main("document.getElementById('editor').contentDocument.body.textContent").await.unwrap();
    assert_eq!(text, "start and more");
}

#[tokio::test]
async fn snapshots_show_child_frames_in_place_and_their_refs_act() {
    let (_runs, mut g) = launched("cf-3", "child_frames.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let lines: Vec<String> = s.nodes.iter().map(|n| format!("{} {}", n.role, n.name)).collect();
    let at = |l: &str| lines.iter().position(|x| x == l).unwrap_or_else(|| panic!("{l} in {lines:#?}"));
    // The widget's controls between the page's button and the editor's text, as a reader meets them.
    assert!(at("button Top") < at("button Upgrade") && at("button Upgrade") < at("combobox Plan") && at("combobox Plan") < at("text start"), "{lines:#?}");
    let upgrade = s.nodes.iter().find(|n| n.name == "Upgrade").unwrap().r#ref.clone();
    g.act_input(rid("c-1"), &Action::Click { target: Locator::Ref { id: upgrade.clone() } }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 2);
    // A frame's box is in the page's viewport: the frame's margin, border and padding, then the button's.
    let q = g.query(&Locator::Ref { id: upgrade.clone() }, &[frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    let inner = widget(&g, "JSON.stringify(document.getElementById('go').getBoundingClientRect())").await;
    let inner: serde_json::Value = serde_json::from_str(inner.as_str().unwrap()).unwrap();
    let frame = g.eval_main("JSON.stringify(document.getElementById('widget').getBoundingClientRect())").await.unwrap();
    let frame: serde_json::Value = serde_json::from_str(frame.as_str().unwrap()).unwrap();
    let bbox = &q.values["bbox"];
    assert_eq!(bbox["x"].as_f64().unwrap(), frame["x"].as_f64().unwrap() + 7.0 + inner["x"].as_f64().unwrap(), "{bbox} {frame} {inner}");
    assert_eq!(bbox["y"].as_f64().unwrap(), frame["y"].as_f64().unwrap() + 7.0 + inner["y"].as_f64().unwrap(), "{bbox} {frame} {inner}");
    // A re-execution record of the reference names the frame and binds to the same node.
    let resolution = g.resolution_of(&upgrade).await.unwrap().expect("an element path");
    assert_eq!(resolution.frames.len(), 1, "{resolution:?}");
    g.bind_ref("e900", &resolution).await.unwrap();
    g.act_input(rid("c-2"), &Action::Click { target: Locator::Ref { id: "e900".into() } }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 4);
}

#[tokio::test]
async fn a_locator_counts_matches_in_every_frame() {
    let (_runs, mut g) = launched("cf-4", "child_frames.html").await;
    let err = g.act_input(rid("c-1"), &Action::Click { target: css("button") }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::Ambiguous { count: 2, .. }), "{err:?}");
    let err = g.act_input(rid("c-2"), &Action::Click { target: Locator::Role { role: "button".into(), name: None } }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::Ambiguous { count: 2, .. }), "{err:?}");
    // Clicking the frame element itself lands inside it, which the frame element accepts.
    g.act_input(rid("c-3"), &Action::Click { target: css("#widget") }).await.unwrap();
}

#[tokio::test]
async fn watches_see_child_frames() {
    let (_runs, mut g) = launched("cf-5", "iframe.html").await;
    let name = WatchName::new("target").unwrap();
    g.watch_add(name.clone(), Predicate::Attached { target: css(".target") }).await.unwrap();
    assert_eq!(g.until(&name, VirtualDuration::from_ms(1000)).await.unwrap(), ClockOutcome::ConditionMet { watch: name.clone() });
    // `iframe.html` adds the frame at 200 ms; its document is parsed at that instant.
    assert_eq!(g.now().0, 200_000);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Watch(s) if s.value == SampleValue::True)), "{events:?}");
}

/// A child frame's mutations are the page's activity: settling waits for them.
#[tokio::test]
async fn settling_waits_for_a_child_frames_activity() {
    let (_runs, mut g) = launched("cf-6", "child_frames.html").await;
    let before = g.dom_mutations();
    // A change every 50 ms from 100 to 350 ms.
    widget(&g, "let k = 0; const t = setInterval(() => { out.textContent = 'tick ' + ++k; if (k === 6) clearInterval(t); }, 50)").await;
    let r = g.settle(VirtualDuration::from_ms(100), VirtualDuration::from_ms(2000)).await.unwrap();
    assert!(g.dom_mutations() >= before + 6, "{r:?}");
    assert_eq!(r, frontcap_core::clock::SettleOutcome::Quiescent { t: frontcap_core::time::VirtualTime(450_000), next_timer_t: None }, "the frame's last change and the quiet window after it");
}

/// Cross-document observation: a widget of another origin (another port) is reached like one of the page's own — its
/// controls clicked, filled and pressed, its text waited for, its box read in the page's viewport.
#[tokio::test]
async fn a_frame_of_another_origin_is_observed_on_the_patched_engine() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let child = format!("http://localhost:{}/child_frame_widget.html", addr.port());
    let mut o = support::options(runs.path(), "cf-7", format!("http://{addr}/child_frames.html?child={child}"), RuleSet::default());
    o.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let start = g.now();
    g.act_input(rid("c-1"), &Action::Click { target: role("button", "Upgrade") }).await.unwrap();
    let done = WatchName::new("upgraded").unwrap();
    g.watch_add(done.clone(), Predicate::TextEquals { target: css("#out"), text: "upgraded".into() }).await.unwrap();
    assert_eq!(g.until(&done, VirtualDuration::from_ms(1000)).await.unwrap(), ClockOutcome::ConditionMet { watch: done.clone() });
    assert_eq!(g.now().0 - start.0, 100_000);
    assert_eq!(g.eval_main("document.getElementById('status').textContent").await.unwrap(), "upgraded");
    g.act_input(rid("f-1"), &Action::Fill { target: css("#coupon"), value: "SAVE10".into() }).await.unwrap();
    g.act_input(rid("p-1"), &Action::Press { target: None, key: "Enter".into() }).await.unwrap();
    let q = g.query(&css("#out"), &[frontcap_chromium::generation::Prop::Text, frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    assert_eq!(q.values["text"], "coupon SAVE10");
    // The frame starts at 10 px margin, 2 px border, 5 px padding inside the page.
    assert!(q.values["bbox"]["x"].as_f64().unwrap() >= 17.0, "{:?}", q.values);
    let s = g.snapshot(None, None, 400).await.unwrap();
    assert!(s.nodes.iter().any(|n| n.role == "button" && n.name == "Upgrade"), "{:?}", s.nodes);
}

/// Requests issued at one instant along different paths — a frame's navigation goes through the browser's
/// navigation before interception, a fetch does not — are handled in the page's order: their records,
/// their ordinals, and the steps of a `sequence` rule they share follow it in every run.
#[tokio::test]
async fn requests_issued_at_one_instant_are_handled_in_the_pages_order() {
    let rules = RuleSet::from_json(
        r#"[{"id":"frames","match":{"url":"https://w.test/*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p>frame</p>"}},
            {"id":"seq","match":{"url":"https://api.test/seq"},"action":{"type":"sequence","after_last":"repeat_last","steps":[
              {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"one"},
              {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"two"},
              {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"three"},
              {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"four"}]}}]"#,
    )
    .unwrap();
    let addr = support::serve();
    for run in 0..6 {
        let runs = tempfile::tempdir().unwrap();
        let mut g = Generation::launch(support::options(runs.path(), &format!("cf-8-{run}"), format!("http://{addr}/same_instant.html"), rules.clone())).await.unwrap();
        g.advance(VirtualDuration::from_ms(300)).await.unwrap();
        let requests: Vec<String> = BehaviorLog::read(&g.trace_path())
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Request(r) if !r.url.contains("same_instant") => Some(format!("{} {} {}", r.id, e.t_us.0 / 1000, r.url)),
                _ => None,
            })
            .collect();
        assert_eq!(
            requests,
            vec![
                "req-2 0 https://w.test/markup",
                "req-3 0 https://api.test/seq",
                "req-4 100 https://w.test/a",
                "req-5 100 https://api.test/seq",
                "req-6 100 https://w.test/b",
                "req-7 100 https://api.test/seq",
                "req-8 100 https://w.test/c2",
                "req-9 100 https://api.test/seq",
            ],
            "run {run}"
        );
        let got = g.eval_main("JSON.stringify(got)").await.unwrap();
        assert_eq!(got, r#"["load:one","first:two","second:three","third:four"]"#, "run {run}");
    }
}

/// Cross-document observation: `document.open()` erases a window's listeners, the page's; the observer's survive, so
/// input into an editor the page writes into a frame (and into a page that rewrites itself) is received.
#[tokio::test]
async fn input_reaches_documents_the_page_wrote_with_document_open() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let (_runs, mut g) = launched("cf-9", "child_frame_written.html").await;
    g.act_input(rid("t-1"), &Action::Type { target: css("#doc"), text: "x".into() }).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('editor').contentDocument.body.textContent").await.unwrap(), "startx");
    g.eval_main("rewrite()").await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.act_input(rid("c-1"), &Action::Click { target: css("#again") }).await.unwrap();
    assert_eq!(g.eval_main("window.clicks").await.unwrap(), 1);
}

/// A modified activation in a frame opens a new window: its navigation is the frame's to request, not to
/// load, so nothing waits for its interception.
#[tokio::test]
async fn a_link_opened_elsewhere_from_a_frame_is_not_waited_for() {
    let (_runs, mut g) = launched("cf-10", "child_frame_links.html").await;
    let wall = std::time::Instant::now();
    g.act_input(rid("p-1"), &Action::Press { target: Some(css("#terms")), key: "Shift+Enter".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    assert!(wall.elapsed() < std::time::Duration::from_secs(1), "{:?}", wall.elapsed());
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(!trace.contains("unintercepted_request"), "{trace}");
}

/// A frame scope narrows a locator to the frames one frame element shows: for actions, role locators, and
/// watches (a role one included, by the engine's native observer query); a scope that names something
/// other than a frame is not found.
#[tokio::test]
async fn frame_scopes_narrow_a_locator_to_one_frame() {
    let (_runs, mut g) = launched("cf-11", "child_frames.html").await;
    let in_widget = |target: Locator| Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(target) };
    g.act_input(rid("c-1"), &Action::Click { target: in_widget(css("button")) }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 2);
    g.act_input(rid("c-2"), &Action::Click { target: in_widget(Locator::Role { role: "button".into(), name: None }) }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 4);
    // The frame named by its accessibility role and title.
    let by_role = Locator::Frame { frame: Box::new(role("Iframe", "Widget")), target: Box::new(css("#go")) };
    g.act_input(rid("c-3"), &Action::Click { target: by_role }).await.unwrap();
    assert_eq!(widget(&g, "log.length").await, 6);
    // The editor's frame has no button.
    let err = g.act_input(rid("c-4"), &Action::Click { target: Locator::Frame { frame: Box::new(css("#editor")), target: Box::new(css("button")) } }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotFound(_)), "{err:?}");
    let err = g.act_input(rid("c-5"), &Action::Click { target: Locator::Frame { frame: Box::new(css("#status")), target: Box::new(css("button")) } }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotFound(ref m) if m.contains("a frame")), "{err:?}");
    let done = WatchName::new("upgraded").unwrap();
    g.watch_add(done.clone(), Predicate::TextEquals { target: in_widget(css("#out")), text: "upgraded".into() }).await.unwrap();
    assert_eq!(g.until(&done, VirtualDuration::from_ms(1000)).await.unwrap(), ClockOutcome::ConditionMet { watch: done });
    let by_role = g.watch_add(WatchName::new("by-role").unwrap(), Predicate::Attached { target: in_widget(role("button", "Upgrade")) }).await.unwrap();
    assert_eq!(by_role, frontcap_core::event::SampleValue::True);
}

/// A reference into a frame the page removed is stale, as a detached node's is.
#[tokio::test]
async fn a_reference_into_a_removed_frame_is_stale() {
    let (_runs, mut g) = launched("cf-12", "child_frames.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let upgrade = s.nodes.iter().find(|n| n.name == "Upgrade").unwrap().r#ref.clone();
    g.eval_main("document.getElementById('widget').remove()").await.unwrap();
    let err = g.act_input(rid("c-1"), &Action::Click { target: Locator::Ref { id: upgrade } }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::StaleRef(_)), "{err:?}");
}

/// Cross-document observation: a frame of the page's origin inside one of another origin is reached, placed, and
/// re-bound through both frames.
#[tokio::test]
async fn a_frame_inside_a_frame_of_another_origin_is_observed() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let middle = format!("http://localhost:{}/nested_middle.html", addr.port());
    let leaf = format!("http://{addr}/nested_leaf.html");
    let mut o = support::options(runs.path(), "cf-13", format!("http://{addr}/nested_frames.html?middle={middle}&leaf={leaf}"), RuleSet::default());
    o.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.act_input(rid("c-1"), &Action::Click { target: role("button", "Deep") }).await.unwrap();
    let q = g.query(&css("#said"), &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
    assert_eq!(q.values["text"], "pressed");
    // The button's box: 20 px into the page, 10 px into the middle frame (after its paragraph), 5 px margin.
    let b = g.query(&css("#deep"), &[frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    assert_eq!(b.values["bbox"]["x"].as_f64().unwrap(), 35.0, "{:?}", b.values);
    let s = g.snapshot(None, None, 400).await.unwrap();
    let deep = s.nodes.iter().find(|n| n.name == "Deep").unwrap().r#ref.clone();
    let resolution = g.resolution_of(&deep).await.unwrap().expect("an element path");
    assert_eq!(resolution.frames.len(), 2, "{resolution:?}");
    g.bind_ref("e901", &resolution).await.unwrap();
    g.act_input(rid("c-2"), &Action::Click { target: Locator::Ref { id: "e901".into() } }).await.unwrap();
}

/// A frame whose initial empty document the page touched before its own committed: the frame's document
/// reuses that window, and its input is received.
#[tokio::test]
async fn input_reaches_a_frame_that_reused_its_initial_window() {
    let (_runs, mut g) = launched("cf-14", "child_frames.html?touch").await;
    g.act_input(rid("t-1"), &Action::Type { target: css("#doc"), text: " ok".into() }).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('editor').contentDocument.body.textContent").await.unwrap(), "start ok");
}

/// A frame sandboxed without scripts (a message a webmail page renders): the observer reaches it from
/// the top frame, and its input is received.
#[tokio::test]
async fn a_sandboxed_frame_without_scripts_is_observed() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let (_runs, mut g) = launched("cf-15", "child_frame_sandboxed.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    assert!(s.nodes.iter().any(|n| n.role == "button" && n.name == "Send"), "{:?}", s.nodes);
    g.act_input(rid("f-1"), &Action::Fill { target: css("#reply"), value: "thanks".into() }).await.unwrap();
    let q = g.query(&css("#reply"), &[frontcap_chromium::generation::Prop::Value]).await.unwrap();
    assert_eq!(q.values["value"], "thanks");
}

/// The page's own navigation takes its place among the requests the page issued before it: a link's
/// click handler that reports before the page leaves (a keepalive fetch, a beacon, a fetch) has them
/// recorded first, in every run.
#[tokio::test]
async fn a_navigation_follows_the_requests_issued_before_it() {
    let rules = RuleSet::from_json(
        r#"[{"id":"next","match":{"url":"https://next.test/*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p>next</p>"}},
            {"id":"api","match":{"url":"https://api.test/*"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"ok"}}]"#,
    )
    .unwrap();
    let addr = support::serve();
    for run in 0..8 {
        let runs = tempfile::tempdir().unwrap();
        let mut g = Generation::launch(support::options(runs.path(), &format!("cf-16-{run}"), format!("http://{addr}/beacon_link.html"), rules.clone())).await.unwrap();
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        g.act_input(rid("c-1"), &Action::Click { target: css("#out") }).await.unwrap();
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        let requests: Vec<String> = BehaviorLog::read(&g.trace_path())
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Request(r) if !r.url.contains("beacon_link") => Some(format!("{} {}", r.id, r.url)),
                _ => None,
            })
            .collect();
        assert_eq!(requests, vec!["req-2 https://api.test/k", "req-3 https://api.test/b", "req-4 https://api.test/p", "req-5 https://next.test/page"], "run {run}");
    }
}

/// The navigation's pause can reach the platform before its `Page.frameRequestedNavigation`, which
/// places it among the page's announcements: it still waits for the requests the page issued first,
/// and its `navigate` report, which waits behind them, is read once they are recorded.
#[tokio::test]
async fn a_navigation_paused_before_earlier_requests_still_follows_them() {
    let rules = RuleSet::from_json(
        r#"[{"id":"next","match":{"url":"https://next.test/*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p>next</p>"}},
            {"id":"api","match":{"url":"https://api.test/*"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"ok"}}]"#,
    )
    .unwrap();
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "cf-20", format!("http://{addr}/beacon_link.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.delay_navigation_requests("https://next.test/", std::time::Duration::from_millis(500));
    g.act_input(rid("c-1"), &Action::Click { target: css("#out") }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let requests: Vec<String> = BehaviorLog::read(&g.trace_path())
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Request(r) if !r.url.contains("beacon_link") => Some(format!("{} {}", r.id, r.url)),
            _ => None,
        })
        .collect();
    assert_eq!(requests, vec!["req-2 https://api.test/k", "req-3 https://api.test/b", "req-4 https://api.test/p", "req-5 https://next.test/page"]);
}

/// A frame scope in a watch means what it means for an action: one frame element, or the watch cannot say;
/// an element that is no frame holds nothing.
#[tokio::test]
async fn a_frame_scope_in_a_watch_names_one_frame() {
    let (_runs, mut g) = launched("cf-17", "child_frames.html").await;
    let scoped = |frame: &str| Locator::Frame { frame: Box::new(css(frame)), target: Box::new(css("button")) };
    let both = WatchName::new("both").unwrap();
    assert_eq!(g.watch_add(both, Predicate::Attached { target: scoped("iframe") }).await.unwrap(), SampleValue::Unknown);
    let err = g.act_input(rid("c-1"), &Action::Click { target: scoped("iframe") }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::Ambiguous { count: 2, .. }), "{err:?}");
    let none = WatchName::new("none").unwrap();
    assert_eq!(g.watch_add(none, Predicate::Attached { target: scoped("#status") }).await.unwrap(), SampleValue::False);
    let one = WatchName::new("one").unwrap();
    assert_eq!(g.watch_add(one, Predicate::Attached { target: scoped("#widget") }).await.unwrap(), SampleValue::True);
    // A reference names its node already: a scope around it is refused rather than ignored.
    let s = g.snapshot(None, None, 400).await.unwrap();
    let top = s.nodes.iter().find(|n| n.name == "Top").unwrap().r#ref.clone();
    let err = g.act_input(rid("c-2"), &Action::Click { target: Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(Locator::Ref { id: top }) } }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::InvalidArgument(_)), "{err:?}");
}

/// A node's box is placed on the page with every transform applied: inside a frame scaled by 2, a 100 by 40
/// button at (20, 30) covers 200 by 80 at (40, 60); rotated a quarter turn, its corners' bounds.
#[tokio::test]
async fn a_box_in_a_transformed_frame_is_where_the_page_shows_it() {
    let (_runs, mut g) = launched("cf-18", "child_frames.html").await;
    let frame = |transform: &str| format!("document.body.innerHTML = `<iframe id='t' style='position:absolute;left:0;top:0;border:0;padding:0;margin:0;width:400px;height:300px;transform:{transform};transform-origin:0 0' srcdoc=\"<button id='b' style='position:absolute;left:20px;top:30px;width:100px;height:40px;margin:0'>B</button>\"></iframe>`; 0");
    let bbox = |v: &frontcap_chromium::generation::QueryResult| -> Vec<f64> { ["x", "y", "width", "height"].iter().map(|k| v.values["bbox"][*k].as_f64().unwrap()).collect() };
    g.eval_main(&frame("scale(2)")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let q = g.query(&css("#b"), &[frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    assert_eq!(bbox(&q), vec![40.0, 60.0, 200.0, 80.0]);
    // A quarter turn about the frame's corner: (x, y) goes to (-y, x), then the frame is moved into view.
    g.eval_main(&frame("translate(300px, 0) rotate(90deg)")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let q = g.query(&css("#b"), &[frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    assert_eq!(bbox(&q).iter().map(|v| v.round()).collect::<Vec<_>>(), vec![230.0, 20.0, 40.0, 100.0]);
}

/// A node the engine laid out no box for (not displayed) has the observer's empty box, not an error.
#[tokio::test]
async fn a_node_without_a_box_has_an_empty_one() {
    let (_runs, mut g) = launched("cf-19", "child_frames.html").await;
    g.eval_main("document.getElementById('status').style.display = 'none'; 0").await.unwrap();
    let q = g.query(&css("#status"), &[frontcap_chromium::generation::Prop::Bbox]).await.unwrap();
    assert_eq!(q.values["bbox"], serde_json::json!({"x": 0, "y": 0, "width": 0, "height": 0}));
}

/// A dialog pauses the page and resumes it once answered. A sibling document that committed meanwhile
/// still waits for its turn: until the dialog's document has finished arriving.
#[tokio::test]
async fn a_dialog_does_not_admit_a_sibling_waiting_for_its_turn() {
    use std::io::Write;
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let port = addr.port();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            std::thread::spawn(move || {
                // `a`, another site's document so that its alert is the engine's own, arrives in three parts:
                // `c` starts once `a` is in, and `a` is still arriving when its alert is answered.
                let parts: Vec<&str> = match request.url() {
                    "/a" => vec!["<!doctype html><p>a</p><script>parent.postMessage('start','*')</script>",
                        "<script>alert('a')</script>", "<script>parent.postMessage('a','*')</script>"],
                    "/b" => vec!["<!doctype html><script>parent.postMessage('b','*')</script>"],
                    "/c" => vec!["<!doctype html><script>parent.postMessage('c','*')</script>"],
                    _ => vec!["<!doctype html><output id=status></output><script>const order=[];addEventListener('message',e=>{\
                        if(e.data==='start'){document.body.insertAdjacentHTML('beforeend','<iframe src=/c></iframe>');return}\
                        order.push(e.data);document.querySelector('#status').textContent=order.join(',')});\
                        setTimeout(()=>document.body.insertAdjacentHTML('beforeend','<iframe src=//localhost:PORT/a></iframe><iframe src=/b></iframe>'),10)</script>"],
                };
                let parts: Vec<String> = parts.iter().map(|p| p.replace("PORT", &port.to_string())).collect();
                let length: usize = parts.iter().map(|p| p.len()).sum();
                let mut out = request.into_writer();
                let _ = write!(out, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {length}\r\n\r\n");
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    let _ = out.write_all(part.as_bytes());
                    let _ = out.flush();
                }
            });
        }
    });
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "dialog-admission", format!("http://{addr}/"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{port}")]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(), "b,a,c");
}
