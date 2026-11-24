mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::action::Action;
use frontcap_core::event::EventKind;
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched(run: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/cover.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    (runs, g)
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

#[tokio::test]
async fn click_on_nested_span_is_accepted() {
    let (_runs, mut g) = launched("click-1").await;
    g.click(RequestId::new("c-1").unwrap(), &css("#ok span")).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[\"ok\"]");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Action(a) if a.action == "click")));
}

/// A date field's segments are in its user-agent shadow tree: the snapshot names them, and a click by
/// their reference lands on the segment, its receipt at the field (where the page sees it).
#[tokio::test]
async fn a_date_fields_segment_is_clicked_by_its_reference() {
    let rules = RuleSet::from_json(&serde_json::json!([{"id": "app", "match": {"url": "https://app.example.test/"}, "action": {"type": "mock", "status": 200,
        "headers": {"content-type": "text/html"}, "body": "<!doctype html><label>Due <input id=due type=date></label><script>window.clicks = []; due.addEventListener('click', () => clicks.push(document.activeElement.id));</script>"}}]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "click-date", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let s = g.snapshot(None, None, 400).await.unwrap();
    // The engine names a labelled field's segments "Month Month", "Day Day".
    let segment = |name: &str| Locator::Ref { id: s.nodes.iter().find(|n| n.role == "spinbutton" && n.name.starts_with(name)).unwrap_or_else(|| panic!("{name} in {:?}", s.nodes)).r#ref.clone() };
    g.click(RequestId::new("c-1").unwrap(), &segment("Month")).await.unwrap();
    g.act_input(RequestId::new("t-1").unwrap(), &Action::Press { target: None, key: "ArrowUp".into() }).await.unwrap();
    g.click(RequestId::new("c-2").unwrap(), &segment("Day")).await.unwrap();
    g.act_input(RequestId::new("t-2").unwrap(), &Action::Press { target: None, key: "ArrowUp".into() }).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(clicks)").await.unwrap(), r#"["due","due"]"#);
    // Month and day stepped up from empty; the year is still unset, so the value is.
    let q = g.query(&segment("Day"), &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
    assert_eq!(q.values["text"], "01");
    let q = g.query(&segment("Month"), &[frontcap_chromium::generation::Prop::Text]).await.unwrap();
    assert_eq!(q.values["text"], "01");
    // A watch takes the segment's reference too.
    let month = frontcap_core::ids::WatchName::new("month").unwrap();
    let sample = g.watch_add(month, frontcap_core::predicate::Predicate::TextEquals { target: segment("Month"), text: "01".into() }).await.unwrap();
    assert_eq!(sample, frontcap_core::event::SampleValue::True);
}

#[tokio::test]
async fn covered_button_is_not_actionable() {
    let (_runs, mut g) = launched("click-2").await;
    let err = g.click(RequestId::new("c-2").unwrap(), &css("#hidden-btn")).await.unwrap_err();
    match err {
        ChromiumError::NotActionable(why) => assert!(why.contains("covered_by"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[]");
}

#[tokio::test]
async fn ambiguous_and_missing_locators_are_refused() {
    let (_runs, mut g) = launched("click-3").await;
    assert!(matches!(g.click(RequestId::new("c-3").unwrap(), &css(".dup")).await, Err(ChromiumError::Ambiguous { count: 2, .. })));
    assert!(matches!(g.click(RequestId::new("c-4").unwrap(), &css("#nope")).await, Err(ChromiumError::NotFound(_))));
}

#[tokio::test]
async fn role_locator_clicks_by_accessible_name() {
    let (_runs, mut g) = launched("click-4").await;
    g.click(RequestId::new("c-5").unwrap(), &Locator::Role { role: "button".into(), name: Some("nested label".into()) }).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[\"ok\"]");
}

#[tokio::test]
async fn click_is_atomic_against_a_frame_mutator() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "click-5", format!("http://{addr}/frame-mutator.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    for i in 0..20 {
        let before = g.eval_main("window.__gen()").await.unwrap();
        // Wall time passes while frozen; platform-owned frames must not run.
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(g.eval_main("window.__gen()").await.unwrap(), before, "a frame callback ran while frozen");
        g.click(RequestId::new(format!("m-{i}")).unwrap(), &css("button")).await.unwrap();
        let last = g.eval_main("window.__clicks[window.__clicks.length - 1]").await.unwrap();
        assert_eq!(last, before, "click {i} reached another generation");
        g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    }
}

#[tokio::test]
async fn disabled_button_is_not_actionable() {
    let (_runs, mut g) = launched("click-6").await;
    let err = g.click(RequestId::new("c-6").unwrap(), &css("#dis")).await.unwrap_err();
    match err {
        ChromiumError::NotActionable(why) => assert_eq!(why, "disabled"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn offscreen_button_is_not_actionable() {
    let (_runs, mut g) = launched("click-7").await;
    let err = g.click(RequestId::new("c-7").unwrap(), &css("#off")).await.unwrap_err();
    match err {
        // The button sits at top 5000px, below the viewport: centering it scrolls down.
        ChromiumError::Offscreen { delta_x: 0, delta_y } => assert!(delta_y > 0, "{delta_y}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[]");
}

#[tokio::test]
async fn click_substeps_leave_receipts_per_step() {
    let (_runs, mut g) = launched("click-8").await;
    let id = RequestId::new("c-8").unwrap();
    let plan = g.input_plan(&Action::Click { target: css("#ok") }).await.unwrap();
    assert_eq!(plan.steps.len(), 3, "a move to the button, a press and a release");
    assert_eq!(g.input_receipts(&id).await.unwrap(), Some(vec![]));
    g.input_substep(&id, &plan, 0).await.unwrap();
    assert_eq!(g.input_receipts(&id).await.unwrap(), Some(vec![0]));
    g.input_substep(&id, &plan, 1).await.unwrap();
    assert_eq!(g.input_receipts(&id).await.unwrap(), Some(vec![0, 1]));
    g.input_substep(&id, &plan, 2).await.unwrap();
    g.input_finish(&id, &plan).await.unwrap();
    assert_eq!(g.input_receipts(&id).await.unwrap(), Some(vec![0, 1, 2]));
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[\"ok\"]");
}

#[tokio::test]
async fn a_page_that_cancels_pointer_events_still_leaves_receipts() {
    let (_runs, mut g) = launched("click-9").await;
    g.eval_main("window.addEventListener('pointerdown', (e) => { e.preventDefault(); e.stopImmediatePropagation(); }, true)").await.unwrap();
    let id = RequestId::new("c-9").unwrap();
    let plan = g.input_plan(&Action::Click { target: css("#ok") }).await.unwrap();
    // The move to the button, then the press the page cancels.
    g.input_substep(&id, &plan, 0).await.unwrap();
    g.input_substep(&id, &plan, 1).await.unwrap();
    assert_eq!(g.input_receipts(&id).await.unwrap(), Some(vec![0, 1]));
}

/// A user's mouse moves to what it clicks: a click where the mouse is not first moves it there, so the
/// page's move handlers run at the click's point; a click where it already is sends no move (scenario 3
/// consumer run: a slider following the pointer saved the value of the last hover, not of the click).
#[tokio::test]
async fn a_click_elsewhere_moves_the_mouse_first() {
    let (_runs, mut g) = launched("click-move").await;
    g.eval_main("window.__moves = []; document.addEventListener('pointermove', e => __moves.push([e.clientX, e.clientY, e.target.id])); document.addEventListener('pointerdown', e => __moves.push(['down', e.target.id])); true").await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &css("#ok")).await.unwrap();
    g.click(RequestId::new("c-2").unwrap(), &css("#ok")).await.unwrap();
    let log: Vec<serde_json::Value> = serde_json::from_str(g.eval_main("JSON.stringify(__moves)").await.unwrap().as_str().unwrap()).unwrap();
    assert_eq!(log.len(), 3, "one move, then a press for each click: {log:?}");
    assert!(log[0][0].is_number() && log[1][0] == "down" && log[2][0] == "down", "{log:?}");
}
