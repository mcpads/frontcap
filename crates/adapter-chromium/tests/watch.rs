mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::{ClockOutcome, EventKind, PointKind, SampleValue};
use frontcap_core::ids::WatchName;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

fn late() -> Predicate {
    Predicate::Attached { target: Locator::Css { selector: ".late".into() } }
}

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    (runs, g)
}

#[tokio::test]
async fn until_stops_at_the_exact_instant() {
    let (_runs, mut g) = launched("watch-1", "appear.html").await;
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    let name = WatchName::new("late").unwrap();
    assert_eq!(g.watch_add(name.clone(), late()).await.unwrap(), SampleValue::False);
    let outcome = g.until(&name, VirtualDuration::from_ms(10_000)).await.unwrap();
    assert_eq!(outcome, ClockOutcome::ConditionMet { watch: name.clone() });
    assert_eq!(g.now(), VirtualTime(1_234_000));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let first_true = events.iter().find_map(|e| match &e.kind {
        EventKind::Watch(s) if s.value == SampleValue::True => Some((e.t_us, s.point)),
        _ => None,
    });
    assert_eq!(first_true, Some((VirtualTime(1_234_000), PointKind::Mutation)));
}

#[tokio::test]
async fn until_times_out_without_the_condition() {
    let (_runs, mut g) = launched("watch-2", "appear.html").await;
    let name = WatchName::new("late").unwrap();
    g.watch_add(name.clone(), late()).await.unwrap();
    let outcome = g.until(&name, VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(outcome, ClockOutcome::ConditionTimedOut { watch: name });
    assert_eq!(g.now(), VirtualTime(1_000_000));
}

#[tokio::test]
async fn every_sample_is_recorded_with_initial_and_final() {
    let (_runs, mut g) = launched("watch-3", "appear.html").await;
    let name = WatchName::new("late").unwrap();
    g.watch_add(name.clone(), late()).await.unwrap();
    g.advance(VirtualDuration::from_ms(2000)).await.unwrap();
    assert_eq!(g.watch_remove(&name).await.unwrap(), SampleValue::True);
    let points: Vec<PointKind> = BehaviorLog::read(&g.trace_path())
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind { EventKind::Watch(s) => Some(s.point), _ => None })
        .collect();
    assert_eq!(points.first(), Some(&PointKind::Initial));
    assert_eq!(points.last(), Some(&PointKind::Final));
}

#[tokio::test]
async fn page_debugger_statement_does_not_stop_until() {
    let (_runs, mut g) = launched("watch-4", "debugger.html").await;
    let name = WatchName::new("never").unwrap();
    g.watch_add(name.clone(), Predicate::Attached { target: Locator::Css { selector: "#never".into() } }).await.unwrap();
    let outcome = g.until(&name, VirtualDuration::from_ms(500)).await.unwrap();
    assert_eq!(outcome, ClockOutcome::ConditionTimedOut { watch: name });
    assert_eq!(g.eval_main("document.title").await.unwrap(), "after");
}

#[tokio::test]
async fn role_predicates_follow_native_names_replacement_and_ambiguity_without_rendering() {
    let (_runs, mut g) = launched("watch-roles", "roles.html").await;
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let role=|name:&str| Locator::Role {role:"button".into(),name:Some(name.into())};
    let before=g.observation_boundary().await.unwrap();
    let image=g.screenshot().await.unwrap();
    for (name,target) in [
        ("save",role("Save")),("shadow",role("Shadow")),
        ("search",Locator::Role {role:"textbox".into(),name:Some("Search".into())}),
        ("child",Locator::Frame {frame:Box::new(Locator::Role {role:"iframe".into(),name:Some("Child controls".into())}),target:Box::new(role("Child"))}),
    ] {
        g.query(&target,&[frontcap_chromium::generation::Prop::Text]).await.unwrap();
        assert_eq!(g.watch_add(WatchName::new(name).unwrap(),Predicate::Attached {target}).await.unwrap(),SampleValue::True,"{name}");
    }
    assert_eq!(g.observation_boundary().await.unwrap(),before,"native AX matching must not move the logical boundary");
    let after=g.screenshot().await.unwrap();
    assert_eq!(std::fs::read(image.path).unwrap(),std::fs::read(after.path).unwrap(),"role sampling must not paint");
    assert_eq!(g.eval_main("typeof __frontcapQueryRole").await.unwrap(),"undefined","observer native hook is not exposed to page script");
    let submit=WatchName::new("submit").unwrap();
    assert_eq!(g.watch_add(submit.clone(),Predicate::Attached {target:role("Submit")}).await.unwrap(),SampleValue::False);
    assert_eq!(g.until(&submit,VirtualDuration::from_ms(1000)).await.unwrap(),ClockOutcome::ConditionMet {watch:submit});
    assert_eq!(g.now(),VirtualTime(100_000));
    assert_eq!(g.watch_remove(&WatchName::new("save").unwrap()).await.unwrap(),SampleValue::False,"replacement is resolved dynamically");
    let ready=WatchName::new("ready").unwrap();
    g.watch_add(ready.clone(),Predicate::Attached {target:role("Ready")}).await.unwrap();
    assert_eq!(g.until(&ready,VirtualDuration::from_ms(1000)).await.unwrap(),ClockOutcome::ConditionMet {watch:ready.clone()});
    assert_eq!(g.now(),VirtualTime(200_000));
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.watch_remove(&ready).await.unwrap(),SampleValue::Unknown,"duplicate accessible matches remain ambiguous");
    assert_eq!(g.watch_add(WatchName::new("two").unwrap(),Predicate::CountAtLeast {target:role("Ready"),n:2}).await.unwrap(),SampleValue::True);
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.watch_add(ready,Predicate::Attached {target:role("Ready")}).await.unwrap(),SampleValue::True,"hidden accessible objects do not match");
}
