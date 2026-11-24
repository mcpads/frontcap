mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::{DialogKind, EventKind};
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

async fn launched(run: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/dialogs.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    (runs, g)
}

#[tokio::test]
async fn alert_confirm_prompt_are_answered_and_recorded() {
    let (_runs, mut g) = launched("dlg-1").await;
    g.eval_main("timers()").await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let log = g.eval_main("JSON.stringify(log)").await.unwrap();
    assert_eq!(log, r#"[["before-alert",101],["after-alert",101],["confirm",false,201],["prompt",null,301],["timer-400",401]]"#);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let dialogs: Vec<(u64, DialogKind, String, String)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Dialog(d) => Some((e.t_us.0, d.kind, d.message.clone(), d.answer.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        dialogs,
        vec![
            (101_000, DialogKind::Alert, "hi".to_string(), "returned".to_string()),
            (201_000, DialogKind::Confirm, "sure?".to_string(), "false".to_string()),
            (301_000, DialogKind::Prompt, "name?".to_string(), "null".to_string()),
        ]
    );
}

#[tokio::test]
async fn confirm_is_dismissed_and_recorded() {
    let (_runs, mut g) = launched("dlg-2").await;
    g.click(RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#del".into() }).await.unwrap();
    assert_eq!(g.eval_main("!!document.getElementById('item')").await.unwrap(), true, "the guarded action did not happen");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Dialog(d) if d.kind == DialogKind::Confirm && d.message == "Delete?")));
}

#[tokio::test]
async fn uncaught_exceptions_are_recorded() {
    let (_runs, mut g) = launched("dlg-3").await;
    g.eval_main("boom()").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let ex = events.iter().find(|e| matches!(&e.kind, EventKind::Exception(_))).expect("an exception record");
    assert_eq!(ex.t_us, VirtualTime(51_000));
    let EventKind::Exception(r) = &ex.kind else { unreachable!() };
    assert!(r.text.contains("boom"), "{r:?}");
}

#[tokio::test]
async fn beforeunload_is_accepted_and_the_navigation_proceeds() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "dlg-4", format!("http://{addr}/leave.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    // The click gives the page the user activation that beforeunload dialogs require.
    g.click(RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#go".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert!(g.eval_main("location.pathname").await.unwrap().as_str().unwrap().ends_with("nav2.html"));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let dialog = events.iter().position(|e| matches!(&e.kind, EventKind::Dialog(d) if d.kind == DialogKind::Beforeunload && d.answer == "accepted")).expect("recorded");
    let navigation = events.iter().rposition(|e| matches!(&e.kind, EventKind::Navigation(_))).unwrap();
    assert!(dialog < navigation, "the dialog precedes the navigation it let through: {events:?}");
    assert_eq!(events[dialog].t_us, events[navigation].t_us, "stamped at the instant the page left");
    if g.native_dialog_control().is_some() {
        let EventKind::Dialog(d) = &events[dialog].kind else { unreachable!() };
        assert_eq!(d.native.as_ref().unwrap().source, frontcap_core::event::NativeDialogSource::Answered);
    }
}

fn dialogs_of(g: &Generation) -> Vec<(u64, String)> {
    BehaviorLog::read(&g.trace_path())
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Dialog(d) => Some((e.t_us.0, d.message.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn page_script_cannot_forge_or_hide_dialog_records() {
    let (_runs, mut g) = launched("dlg-5").await;
    // A page that knows the old event name forges a record and breaks the platform's helpers.
    g.eval_main(r#"document.dispatchEvent(new CustomEvent("frontcap-dialog", {detail: JSON.stringify({kind: "confirm", message: "forged", answer: "true"})}));
        document.dispatchEvent(new CustomEvent("frontcap-dialog", {detail: JSON.stringify({kind: "nope", message: "x", answer: "y"})}));
        EventTarget.prototype.dispatchEvent = function () { return true; };
        window.CustomEvent = function () {};
        JSON.stringify = () => "{}";
        window.addEventListener("click", (e) => e.stopImmediatePropagation(), true);
        setTimeout(() => confirm("real"), 10);"#).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.expect("page content never fails a clock step");
    assert_eq!(dialogs_of(&g), vec![(11_000, "real".to_string())]);
}

#[tokio::test]
async fn child_frame_dialogs_are_recorded() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "dlg-6", format!("http://{addr}/frames-dialog.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(5)).await.unwrap();
    g.eval_main("childConfirm(); borrowed()").await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(log)").await.unwrap(), r#"[["child",false],["borrowed",false]]"#);
    let recorded: Vec<String> = dialogs_of(&g).into_iter().map(|(_, m)| m).collect();
    assert_eq!(recorded, vec!["from child".to_string(), "borrowed".to_string()]);
}

#[tokio::test]
async fn evaluate_that_never_settles_is_a_script_failure_within_the_watchdog() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut opts = support::options(runs.path(), "dlg-7", format!("http://{addr}/dialogs.html"), RuleSet::default());
    opts.watchdog = std::time::Duration::from_millis(1000);
    let mut g = Generation::launch(opts).await.unwrap();
    let wall = std::time::Instant::now();
    let err = g.evaluate("new Promise(() => {})").await.unwrap_err();
    assert!(matches!(err, frontcap_chromium::ChromiumError::Script(ref m) if m.contains("did not settle")), "{err:?}");
    assert!(wall.elapsed() < std::time::Duration::from_secs(5), "{:?}", wall.elapsed());
    g.advance(VirtualDuration::from_ms(10)).await.expect("the page still answers");
    let err = g.evaluate("while (true) {}").await.unwrap_err();
    assert!(matches!(err, frontcap_chromium::ChromiumError::Unresponsive { .. }), "{err:?}");
}

fn dialog_rules(json: &str) -> frontcap_core::dialog::DialogRules {
    serde_json::from_str(json).unwrap()
}

/// Rules the agent declares answer the page's dialogs: an accepted confirm lets the guarded action run,
/// an accepted prompt receives the rule's text, and each answer is what the record states.
#[tokio::test]
async fn dialog_rules_answer_as_declared_and_are_recorded() {
    let (_runs, mut g) = launched("dlg-rules").await;
    g.set_dialog_rules(dialog_rules(r#"[{"kind":"confirm","message":"sure*","answer":"accept"},{"kind":"prompt","answer":"accept","text":"Ada"}]"#)).await.unwrap();
    g.eval_main("timers()").await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(log)").await.unwrap(),
        r#"[["before-alert",101],["after-alert",101],["confirm",true,201],["prompt","Ada",301],["timer-400",401]]"#);
    let answers: Vec<String> = BehaviorLog::read(&g.trace_path()).unwrap().iter().filter_map(|e| match &e.kind {
        EventKind::Dialog(d) => Some(d.answer.clone()),
        _ => None,
    }).collect();
    assert_eq!(answers, vec!["returned", "true", "\"Ada\""]);
}

/// Rules reach documents loaded after they were set, and a cleared rule set dismisses again.
#[tokio::test]
async fn dialog_rules_follow_new_documents_and_can_be_cleared() {
    let (_runs, mut g) = launched("dlg-rules-nav").await;
    g.set_dialog_rules(dialog_rules(r#"[{"kind":"confirm","answer":"accept"}]"#)).await.unwrap();
    g.navigate_action(&RequestId::new("reload").unwrap(), &frontcap_core::action::Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#del".into() }).await.unwrap();
    assert_eq!(g.eval_main("!!document.getElementById('item')").await.unwrap(), false, "accepted in the reloaded document");
    g.navigate_action(&RequestId::new("reload-2").unwrap(), &frontcap_core::action::Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    g.set_dialog_rules(dialog_rules("[]")).await.unwrap();
    g.click(RequestId::new("c-2").unwrap(), &Locator::Css { selector: "#del".into() }).await.unwrap();
    assert_eq!(g.eval_main("!!document.getElementById('item')").await.unwrap(), true, "dismissed again");
}

/// A native dialog (beforeunload) follows the rules too: dismissed, the page stays.
#[tokio::test]
async fn a_dismissed_beforeunload_keeps_the_page() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "dlg-stay", format!("http://{addr}/leave.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.set_dialog_rules(dialog_rules(r#"[{"kind":"beforeunload","answer":"dismiss"}]"#)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#go".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert!(g.eval_main("location.pathname").await.unwrap().as_str().unwrap().ends_with("leave.html"));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Dialog(d) if d.kind == DialogKind::Beforeunload && d.answer == "dismissed")));
}

fn new_windows(g: &Generation) -> Vec<String> {
    BehaviorLog::read(&g.trace_path()).unwrap().iter().filter_map(|e| match &e.kind {
        EventKind::Taint(t) if t.source == frontcap_core::event::TaintSource::NewWindow => Some(t.detail.clone()),
        _ => None,
    }).collect()
}

/// A window the page opens would run outside the platform's clock, rules and evidence: `window.open` is
/// refused as a popup blocker refuses it, and recorded.
#[tokio::test]
async fn window_open_is_blocked_and_recorded() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "popup-open", format!("http://{addr}/popup.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    assert_eq!(g.eval_main("opened()").await.unwrap(), "blocked");
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let windows = new_windows(&g);
    assert_eq!(windows.len(), 1, "{windows:?}");
    assert!(windows[0].contains("link.html"), "{windows:?}");
}

/// A link that would open a new window opens none, and is recorded; the page stays where it was.
#[tokio::test]
async fn a_target_blank_window_is_closed_and_recorded() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "popup-blank", format!("http://{addr}/popup.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    g.click(RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#blank".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert!(g.eval_main("location.pathname").await.unwrap().as_str().unwrap().ends_with("popup.html"));
    assert_eq!(g.page_targets().await.unwrap(), 1, "no other page is left open");
    assert_eq!(new_windows(&g).len(), 1);
}
