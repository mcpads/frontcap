//! Input actions other than click: plans, receipts, and recipients (Plan 7).
mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched(run: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/input.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    (runs, g)
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

async fn log(g: &Generation) -> Vec<String> {
    serde_json::from_str(g.eval_main("JSON.stringify(log.splice(0))").await.unwrap().as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn dblclick_dispatches_two_clicks_and_a_dblclick() {
    let (_runs, mut g) = launched("in-1").await;
    g.act_input(rid("d-1"), &Action::Dblclick { target: css("#btn") }).await.unwrap();
    // Pressing at a point the pointer has not visited also moves it there (`pointerover`), as in a browser.
    assert_eq!(log(&g).await, vec!["pointerover", "click", "click", "dblclick"]);
}

#[tokio::test]
async fn hover_moves_the_pointer_over_the_target() {
    let (_runs, mut g) = launched("in-2").await;
    g.act_input(rid("h-1"), &Action::Hover { target: css("#btn") }).await.unwrap();
    assert_eq!(log(&g).await, vec!["pointerover"]);
}

#[tokio::test]
async fn every_substep_of_a_dblclick_leaves_a_receipt() {
    let (_runs, mut g) = launched("in-3").await;
    let plan = g.input_plan(&Action::Dblclick { target: css("#btn") }).await.unwrap();
    assert_eq!(plan.steps.len(), 5, "a move to the button, then two presses and releases");
    for step in 0..4 {
        g.input_substep(&rid("d-2"), &plan, step).await.unwrap();
    }
    assert_eq!(g.input_receipts(&rid("d-2")).await.unwrap(), Some(vec![0, 1, 2, 3]));
}

#[tokio::test]
async fn the_action_record_carries_name_target_and_arguments() {
    let (_runs, mut g) = launched("in-4").await;
    g.act_input(rid("h-2"), &Action::Hover { target: css("#btn") }).await.unwrap();
    let events = frontcap_core::log::BehaviorLog::read(&g.trace_path()).unwrap();
    let a = events.iter().find_map(|e| match &e.kind { frontcap_core::event::EventKind::Action(a) => Some(a.clone()), _ => None }).unwrap();
    assert_eq!((a.action.as_str(), a.args.is_none(), a.delivered_to.is_none()), ("hover", true, true));
}

async fn value(g: &Generation, selector: &str) -> String {
    let expr = format!("(() => {{ const e = document.querySelector({}); return 'value' in e ? e.value : e.textContent; }})()", serde_json::to_string(selector).unwrap());
    g.eval_main(&expr).await.unwrap().as_str().unwrap().to_string()
}

#[tokio::test]
async fn type_enters_every_character_once() {
    let (_runs, mut g) = launched("in-5").await;
    g.act_input(rid("f-1"), &Action::Fill { target: css("#name"), value: String::new() }).await.unwrap();
    log(&g).await;
    g.act_input(rid("t-1"), &Action::Type { target: css("#name"), text: "hi 한글!".into() }).await.unwrap();
    assert_eq!(value(&g, "#name").await, "hi 한글!");
    // `!` is typed with Shift held, as on a keyboard.
    assert_eq!(log(&g).await, vec!["keydown:h", "keydown:i", "keydown: ", "keydown:Shift", "keydown:!"]);
}

#[tokio::test]
async fn fill_replaces_the_value_of_inputs_textareas_and_editing_hosts() {
    let (_runs, mut g) = launched("in-6").await;
    for (i, sel) in ["#name", "#area", "#editable"].into_iter().enumerate() {
        g.act_input(rid(&format!("f-{i}")), &Action::Fill { target: css(sel), value: "new".into() }).await.unwrap();
        assert_eq!(value(&g, sel).await, "new", "{sel}");
    }
}

#[tokio::test]
async fn press_enter_submits_and_press_without_target_goes_to_the_focused_element() {
    let (_runs, mut g) = launched("in-7").await;
    g.act_input(rid("p-1"), &Action::Press { target: Some(css("#name")), key: "Enter".into() }).await.unwrap();
    assert_eq!(log(&g).await, vec!["keydown:Enter", "submit:old"]);
    g.eval_main("document.activeElement.blur()").await.unwrap();
    g.act_input(rid("p-2"), &Action::Press { target: None, key: "Escape".into() }).await.unwrap();
    assert_eq!(log(&g).await, vec!["escape:BODY"]);
    let err = g.act_input(rid("p-3"), &Action::Press { target: None, key: "Ctrl+A".into() }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::InvalidArgument(ref m) if m.contains("Ctrl")), "{err:?}");
}

#[tokio::test]
async fn keyboard_actions_refuse_targets_that_are_not_editable() {
    let (_runs, mut g) = launched("in-8").await;
    for (sel, reason) in [("#btn", "not editable"), ("#readonly", "not editable"), ("#disabled", "disabled")] {
        let err = g.act_input(rid(&format!("x-{}", &sel[1..])), &Action::Fill { target: css(sel), value: "v".into() }).await.unwrap_err();
        assert!(matches!(err, ChromiumError::NotActionable(ref m) if m.contains(reason)), "{sel}: {err:?}");
    }
    assert_eq!(value(&g, "#readonly").await, "ro");
    assert!(log(&g).await.is_empty());
}

/// A page that moves focus while typing (an auto-advancing code field) gets the later keys where the
/// focus went, as in a browser; only the first event is the platform's to check.
#[tokio::test]
async fn keys_follow_focus_moved_by_the_page() {
    let (_runs, mut g) = launched("in-9").await;
    g.act_input(rid("f-9"), &Action::Fill { target: css("#name"), value: String::new() }).await.unwrap();
    g.act_input(rid("t-9"), &Action::Type { target: css("#mover"), text: "abcd".into() }).await.unwrap();
    // The second key's handler moves focus before its text is inserted, so "b" already lands in #name.
    assert_eq!((value(&g, "#mover").await, value(&g, "#name").await), ("a".to_string(), "bcd".to_string()));
}

#[tokio::test]
async fn select_chooses_options_by_value_or_label_and_reports_the_change() {
    let (_runs, mut g) = launched("in-10").await;
    g.act_input(rid("s-1"), &Action::Select { target: css("#pick"), values: vec!["b".into()] }).await.unwrap();
    assert_eq!((value(&g, "#pick").await, log(&g).await), ("b".to_string(), vec!["input".to_string(), "change".to_string()]));
    g.act_input(rid("s-2"), &Action::Select { target: css("#pick"), values: vec!["Option C".into()] }).await.unwrap();
    assert_eq!(value(&g, "#pick").await, "c");
    let err = g.act_input(rid("s-3"), &Action::Select { target: css("#pick"), values: vec!["z".into()] }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotFound(ref m) if m.contains("z")), "{err:?}");
    let err = g.act_input(rid("s-4"), &Action::Select { target: css("#pick"), values: vec!["a".into(), "b".into()] }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::InvalidArgument(_)), "{err:?}");
    g.act_input(rid("s-5"), &Action::Select { target: css("#many"), values: vec!["x".into(), "y".into()] }).await.unwrap();
    let err = g.act_input(rid("s-6"), &Action::Select { target: css("#name"), values: vec!["a".into()] }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotActionable(ref m) if m.contains("not a select")), "{err:?}");
    assert_eq!(value(&g, "#pick").await, "c", "a refused select changes nothing");
}

/// `scroll` is a native wheel: frozen, the page hears it at once and scrolls at the next frames.
#[tokio::test]
async fn scroll_wheels_the_page_and_it_scrolls_at_the_next_frame() {
    let (_runs, mut g) = launched("in-11").await;
    g.eval_main("window.wheels = []; addEventListener('wheel', e => wheels.push(e.deltaY)); 'listening'").await.unwrap();
    g.act_input(rid("w-1"), &Action::Scroll { target: None, delta_x: 0, delta_y: 400 }).await.unwrap();
    assert_eq!(g.eval_main("[window.scrollY, wheels]").await.unwrap(), serde_json::json!([0, [400]]));
    g.advance(VirtualDuration::from_ms(34)).await.unwrap();
    assert_eq!(g.eval_main("window.scrollY").await.unwrap(), 400);
    // A target scrolls its nearest scrollable ancestor: here the document, even with the target out of view.
    g.act_input(rid("w-2"), &Action::Scroll { target: Some(css("#btn")), delta_x: 0, delta_y: -150 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(34)).await.unwrap();
    assert_eq!(g.eval_main("window.scrollY").await.unwrap(), 250);
    let err = g.act_input(rid("w-0"), &Action::Scroll { target: None, delta_x: 0, delta_y: 0 }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::InvalidArgument(_)), "{err:?}");
}

/// After a click and a move at one instant the wheel is still handled while frozen (engine patch 6).
#[tokio::test]
async fn scroll_after_a_click_and_a_move_at_one_instant_still_scrolls() {
    let (_runs, mut g) = launched("in-12").await;
    g.act_input(rid("c-1"), &Action::Click { target: css("#btn") }).await.unwrap();
    g.act_input(rid("h-1"), &Action::Hover { target: css("#name") }).await.unwrap();
    g.act_input(rid("w-3"), &Action::Scroll { target: None, delta_x: 0, delta_y: 400 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(34)).await.unwrap();
    assert_eq!(g.eval_main("window.scrollY").await.unwrap(), 400);
}

/// The element that takes focus is what the keys reach: a descendant of an editing host types into the
/// host; a target that cannot take focus, or whose focus a handler moves away, is refused before any key.
#[tokio::test]
async fn keyboard_actions_type_where_focus_actually_lands() {
    let (_runs, mut g) = launched("in-13").await;
    g.act_input(rid("t-1"), &Action::Type { target: css("#inner"), text: "Z".into() }).await.unwrap();
    assert!(value(&g, "#host").await.contains('Z'), "{}", value(&g, "#host").await);
    let err = g.act_input(rid("p-1"), &Action::Press { target: Some(css("#plain")), key: "Escape".into() }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotActionable(ref m) if m.contains("not focusable")), "{err:?}");
    let before = value(&g, "#name").await;
    let err = g.act_input(rid("t-2"), &Action::Type { target: css("#stealer"), text: "x".into() }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotActionable(ref m) if m.contains("focus")), "{err:?}");
    assert_eq!((value(&g, "#name").await, value(&g, "#stealer").await), (before, String::new()), "no key was sent");
}

/// A child frame's input is observed: a point inside one is accepted by its frame element, and keys go to
/// the element focused inside it.
#[tokio::test]
async fn input_into_a_child_frame_is_received() {
    let (_runs, mut g) = launched("in-14").await;
    g.act_input(rid("c-1"), &Action::Click { target: css("#frame") }).await.unwrap();
    g.eval_main("document.getElementById('frame').contentDocument.getElementById('inframe').focus()").await.unwrap();
    g.act_input(rid("p-1"), &Action::Press { target: None, key: "a".into() }).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('frame').contentDocument.getElementById('inframe').value").await.unwrap(), "a");
}

/// A role locator resolves right after keyboard input at the same instant (the renderer's frames are
/// rationed while frozen, S14; the accessibility tree must not wait for one).
#[tokio::test]
async fn a_role_locator_resolves_after_typing_at_the_same_instant() {
    let (_runs, mut g) = launched("in-15").await;
    g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(2000)).await.unwrap();
    g.snapshot(None, None, 400).await.unwrap();
    g.act_input(rid("f-1"), &Action::Fill { target: css("#name"), value: "Grace".into() }).await.unwrap();
    let wall = std::time::Instant::now();
    g.act_input(rid("c-1"), &Action::Click { target: Locator::Role { role: "button".into(), name: Some("Go".into()) } }).await.unwrap();
    assert!(wall.elapsed() < std::time::Duration::from_secs(2), "{:?}", wall.elapsed());
    assert_eq!(log(&g).await, vec!["pointerover", "click"]);
}


/// Typed text appends to what the field holds (fields without a text selection included); a line
/// break is the Enter key, and a tab is inserted as text (moving focus is `press Tab`).
#[tokio::test]
async fn type_appends_and_types_line_breaks_as_keys() {
    let (_runs, mut g) = launched("in-16").await;
    g.act_input(rid("t-1"), &Action::Type { target: css("#area"), text: "\r\nAB".into() }).await.unwrap();
    assert_eq!(value(&g, "#area").await, "old\nAB");
    g.eval_main("document.getElementById('name').setAttribute('type', 'TEXT')").await.unwrap();
    g.act_input(rid("t-2"), &Action::Type { target: css("#name"), text: "!".into() }).await.unwrap();
    assert_eq!(value(&g, "#name").await, "old!");
    g.act_input(rid("t-3"), &Action::Type { target: css("#mail"), text: ".org".into() }).await.unwrap();
    assert_eq!(value(&g, "#mail").await, "a@b.org");
    g.act_input(rid("t-4"), &Action::Type { target: css("#num"), text: "3".into() }).await.unwrap();
    assert_eq!(value(&g, "#num").await, "123");
    g.act_input(rid("t-5"), &Action::Type { target: css("#area"), text: "x\ty".into() }).await.unwrap();
    assert_eq!(value(&g, "#area").await, "old\nABx\ty");
}

/// Options are checked before anything is dispatched; a disabled option cannot be chosen, and choosing
/// what is already selected changes nothing and fires nothing, as in a browser.
#[tokio::test]
async fn select_checks_options_before_dispatch() {
    let (_runs, mut g) = launched("in-17").await;
    let plan = g.input_plan(&Action::Select { target: css("#pick"), values: vec!["z".into()] }).await;
    assert!(matches!(plan, Err(ChromiumError::NotFound(_))), "refused while planning: {plan:?}");
    let err = g.act_input(rid("s-1"), &Action::Select { target: css("#pick"), values: vec!["d".into()] }).await.unwrap_err();
    assert!(matches!(err, ChromiumError::NotActionable(ref m) if m.contains("disabled")), "{err:?}");
    g.act_input(rid("s-2"), &Action::Select { target: css("#pick"), values: vec!["a".into()] }).await.unwrap();
    assert_eq!(log(&g).await, Vec::<String>::new(), "a is already selected");
}

/// Tabbing through components' fields: each field's focus and blur is recorded with the field itself,
/// inside an open root and inside a closed one alike, including the moves within one component, which
/// nothing outside its shadow tree sees (there, the move's target and related target are the same host).
#[tokio::test]
async fn focus_moves_inside_shadow_trees_are_recorded_with_their_fields() {
    let page = r#"<!doctype html><input id=outer placeholder=outer><x-open></x-open><x-closed></x-closed><script>
for (const [tag, mode] of [['x-open', 'open'], ['x-closed', 'closed']]) customElements.define(tag, class extends HTMLElement {
  constructor() { super(); this.attachShadow({ mode }).innerHTML = `<input placeholder="${mode} a"><input placeholder="${mode} b">`; }
});</script>"#;
    let rules = RuleSet::from_json(&serde_json::json!([{"id": "app", "match": {"url": "https://app.example.test/"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": page}}]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "in-focus", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.click(rid("c-1"), &css("#outer")).await.unwrap();
    for i in 0..4 {
        g.act_input(rid(&format!("t-{i}")), &Action::Press { target: None, key: "Tab".into() }).await.unwrap();
        g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    }
    let focus: Vec<String> = frontcap_core::log::BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
        frontcap_core::event::EventKind::Focus(f) => Some(format!("{} {}", if f.focused { "focus" } else { "blur" }, f.target.split('"').nth(1).unwrap_or(&f.target))),
        _ => None,
    }).collect();
    assert_eq!(focus, [
        "focus outer", "blur outer", "focus open a", "blur open a", "focus open b", "blur open b",
        "focus closed a", "blur closed a", "focus closed b",
    ]);
}
