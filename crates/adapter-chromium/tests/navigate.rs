//! Navigation actions and `wait_actionable` (Plan 7).
mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::action::Action;
use frontcap_core::clock::WaitOutcome;
use frontcap_core::event::EventKind;
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation, std::net::SocketAddr) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    (runs, g, addr)
}

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

fn navigations(g: &Generation) -> Vec<(u64, String)> {
    BehaviorLog::read(&g.trace_path())
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Navigation(n) => Some((e.t_us.0, n.url.rsplit('/').next().unwrap_or_default().to_string())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn navigate_replaces_the_document_and_invalidates_references() {
    let (_runs, mut g, addr) = launched("nav-a", "input.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let go = s.nodes.iter().find(|n| n.role == "button" && n.name == "Go").unwrap();
    let old = Locator::Ref { id: go.r#ref.clone() };
    g.navigate_action(&rid("n-1"), &Action::Navigate { url: format!("http://{addr}/nav2.html") }).await.unwrap();
    assert_eq!(navigations(&g).last().unwrap(), &(50_000, "nav2.html".to_string()));
    assert!(matches!(g.click(rid("c-1"), &old).await, Err(ChromiumError::StaleRef(_))));
    // The new document has committed but is not parsed, and runs no script, before an advance.
    assert_eq!(g.eval_main("document.getElementById('second') === null && location.pathname").await.unwrap(), "/nav2.html");
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('second') !== null").await.unwrap(), true);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Action(a) if a.action == "navigate" && a.args.as_ref().is_some_and(|v| v["url"].as_str().unwrap().ends_with("nav2.html")))));
}

#[tokio::test]
async fn back_returns_to_the_previous_document_and_reload_restarts_it() {
    let (_runs, mut g, addr) = launched("nav-b", "input.html").await;
    g.navigate_action(&rid("n-1"), &Action::Navigate { url: format!("http://{addr}/nav2.html") }).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.navigate_action(&rid("b-1"), &Action::Back {}).await.unwrap();
    assert_eq!(navigations(&g).last().unwrap().1, "input.html");
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    g.eval_main("window.marker = 1").await.unwrap();
    g.navigate_action(&rid("r-1"), &Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    assert_eq!(g.eval_main("typeof window.marker").await.unwrap(), "undefined", "a fresh document");
    let names: Vec<String> = navigations(&g).into_iter().map(|(_, n)| n).collect();
    assert_eq!(names, vec!["input.html", "nav2.html", "input.html", "input.html"]);
    // Every new document gets its observer: a watch samples it.
    let w = frontcap_core::ids::WatchName::new("go").unwrap();
    g.watch_add(w.clone(), frontcap_core::predicate::Predicate::Attached { target: css("#btn") }).await.unwrap();
    assert_eq!(g.until(&w, VirtualDuration::from_ms(100)).await.unwrap(), frontcap_core::event::ClockOutcome::ConditionMet { watch: w });
}

/// The renderer reports a new document before the browser may have finished committing it. A
/// navigation returns only once the browser has, so `back` right after it reads a history whose
/// current entry is the new document: neither refused nor one entry behind.
#[tokio::test]
async fn back_right_after_a_navigation_reads_the_committed_history() {
    let (_runs, mut g, addr) = launched("nav-committed", "input.html").await;
    for round in 0..15 {
        g.navigate_action(&rid(&format!("n-{round}")), &Action::Navigate { url: format!("http://{addr}/nav2.html?round={round}") })
            .await.unwrap();
        g.navigate_action(&rid(&format!("b-{round}")), &Action::Back {}).await
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert_eq!(navigations(&g).last().unwrap().1, "input.html", "round {round}");
    }
}

#[tokio::test]
async fn back_without_history_is_refused() {
    let (_runs, mut g, _) = launched("nav-c", "input.html").await;
    let before = navigations(&g).len();
    let err = g.navigate_action(&rid("b-1"), &Action::Back {}).await.unwrap_err();
    assert!(matches!(err, ChromiumError::InvalidArgument(ref m) if m.contains("history")), "{err:?}");
    assert_eq!(navigations(&g).len(), before);
}

#[tokio::test]
async fn wait_actionable_returns_when_the_target_becomes_actionable() {
    let (_runs, mut g, _) = launched("nav-d", "appear.html").await;
    let out = g.wait_actionable(&css(".late"), VirtualDuration::from_ms(2000)).await.unwrap();
    let WaitOutcome::Actionable { t } = out else { panic!("{out:?}") };
    assert!((1_234_000..=1_234_000 + 16_667).contains(&t.0), "{t:?}");
    let out = g.wait_actionable(&css(".never"), VirtualDuration::from_ms(500)).await.unwrap();
    assert!(matches!(out, WaitOutcome::TimedOut { ref reason, .. } if reason.contains("no node")), "{out:?}");
    assert_eq!(g.now(), VirtualTime(t.0 + 500_000), "timed out at exactly max");
}

/// A committed navigation has not parsed its document while frozen. Waiting is itself a progress
/// operation: it must not require a separate advance to install the new document's observer.
#[tokio::test]
async fn wait_actionable_waits_for_a_document_that_has_not_started() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "nav-wait-document", format!("http://{addr}/appear.html"), RuleSet::default())).await.unwrap();
    let start = g.now();
    let out = g.wait_actionable(&css(".late"), VirtualDuration::from_ms(0)).await.unwrap();
    assert!(matches!(out, WaitOutcome::TimedOut { t, .. } if t == start), "{out:?}");
    let out = g.wait_actionable(&css(".late"), VirtualDuration::from_ms(2000)).await.unwrap();
    let WaitOutcome::Actionable { t } = out else { panic!("{out:?}") };
    assert!((1_234_000..=1_234_000 + 16_667).contains(&t.since(start).0), "{t:?}");
}

/// A document left before it finished loading (navigated away while frozen) does not keep a settle
/// from becoming quiescent.
#[tokio::test]
async fn documents_left_while_frozen_do_not_block_quiescence() {
    let (_runs, mut g, addr) = launched("nav-e", "input.html").await;
    g.navigate_action(&rid("n-1"), &Action::Navigate { url: format!("http://{addr}/nav2.html") }).await.unwrap();
    g.navigate_action(&rid("b-1"), &Action::Back {}).await.unwrap();
    g.navigate_action(&rid("r-1"), &Action::Reload {}).await.unwrap();
    let out = g.settle(VirtualDuration::from_ms(100), VirtualDuration::from_ms(1000)).await.unwrap();
    assert!(matches!(out, frontcap_core::clock::SettleOutcome::Quiescent { .. }), "{out:?}");
}

/// A same-document history entry (`pushState`, a fragment) is a navigation within the document: it
/// commits without a new document, is recorded, and keeps the document's references.
#[tokio::test]
async fn same_document_navigations_commit_without_a_new_document() {
    let (_runs, mut g, addr) = launched("nav-f", "input.html").await;
    g.eval_main("history.pushState({}, '', '#two')").await.unwrap();
    let wall = std::time::Instant::now();
    g.navigate_action(&rid("b-1"), &Action::Back {}).await.unwrap();
    g.navigate_action(&rid("n-1"), &Action::Navigate { url: format!("http://{addr}/input.html#frag") }).await.unwrap();
    assert!(wall.elapsed() < std::time::Duration::from_secs(2), "{:?}", wall.elapsed());
    assert_eq!(g.eval_main("location.hash").await.unwrap(), "#frag");
    let urls: Vec<String> = navigations(&g).into_iter().map(|(_, u)| u).collect();
    // The page's own pushState is a navigation within the document too.
    assert_eq!(urls, vec!["input.html", "input.html#two", "input.html", "input.html#frag"]);
    g.click(rid("c-1"), &css("#btn")).await.unwrap();
}

/// A click on a link navigates synchronously from the input's handler: the click answers, and the new
/// document is the next one.
#[tokio::test]
async fn a_click_on_a_link_navigates() {
    let (_runs, mut g, _addr) = launched("nav-link", "link.html").await;
    g.click(rid("c-1"), &css("#l")).await.unwrap();
    g.advance(frontcap_core::time::VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), "/nav2.html");
}

/// The same from a `pointerdown` handler, before the release is sent.
#[tokio::test]
async fn a_press_that_navigates_still_releases() {
    let (_runs, mut g, _addr) = launched("nav-down", "link.html").await;
    g.click(rid("c-1"), &css("#down")).await.unwrap();
    g.advance(frontcap_core::time::VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), "/nav2.html");
}

#[tokio::test]
async fn an_input_barrier_services_an_already_queued_navigation() {
    let (_runs, mut g, addr) = launched("nav-queued", "link.html").await;
    let at = g.now();
    g.queued_navigation_round_trip(&format!("http://{addr}/nav2.html")).await.unwrap();
    assert_eq!(g.now(), at, "servicing the navigation grants no virtual time");
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("location.pathname").await.unwrap(), "/nav2.html");
}
