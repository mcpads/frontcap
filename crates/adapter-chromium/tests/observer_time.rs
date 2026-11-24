//! Observer reports retain the instant of the callback, including fractional milliseconds.
mod support;

use frontcap_chromium::Generation;
use frontcap_core::{
    event::{EventKind, SampleValue}, ids::WatchName, log::BehaviorLog,
    predicate::{Locator, Predicate}, rules::RuleSet, time::{VirtualDuration, VirtualTime},
};

#[tokio::test]
async fn held_requests_keep_their_fractional_issue_time() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/fractional-request");
    let rules = RuleSet::from_json(&format!(r#"[{{"id":"hold","match":{{"url":"{url}"}},"action":{{"type":"hold"}}}}]"#)).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "request-time", format!("http://{addr}/presentation_recovery.html"), rules)).await.unwrap();
    g.advance(VirtualDuration(17_333)).await.unwrap();
    g.eval_main("fetch('/fractional-request');true").await.unwrap();
    g.advance(VirtualDuration(1_000)).await.unwrap();
    assert_eq!(g.held_ordinals().len(), 1);
    let trace = g.trace_path();
    g.stop().await.unwrap();
    let events = BehaviorLog::read(&trace).unwrap();
    let request = events.iter().find(|event| matches!(&event.kind, EventKind::Request(r) if r.url == url)).expect("the held request must be recorded");
    assert_eq!(request.t_us, VirtualTime(17_333));
}

#[tokio::test]
async fn console_and_exception_events_keep_fractional_native_timestamps() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "console-time", format!("http://{addr}/presentation_recovery.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration(17_333)).await.unwrap();
    g.evaluate("console.log('fractional console');setTimeout(()=>{throw new Error('fractional exception')},10);true").await.unwrap();
    g.advance(VirtualDuration(10_000)).await.unwrap();
    let trace = g.trace_path();
    g.stop().await.unwrap();
    let events = BehaviorLog::read(&trace).unwrap();
    let console = events.iter().find(|event| matches!(&event.kind, EventKind::Console(c) if c.text == "fractional console")).expect("the console event must be recorded");
    assert_eq!(console.t_us, VirtualTime(17_333));
    let exception = events.iter().find(|event| matches!(&event.kind, EventKind::Exception(e) if e.text.contains("fractional exception"))).expect("the exception must be recorded");
    assert_eq!(exception.t_us, VirtualTime(27_333));
}

#[tokio::test]
async fn watch_registration_and_timer_samples_keep_their_native_microsecond_instant() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "observer-time", format!("http://{addr}/presentation_recovery.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration(17_333)).await.unwrap();
    assert_eq!(g.eval_main("typeof globalThis.__frontcapNativeTime").await.unwrap(), "undefined", "the native observation clock is confined to the inspector world");
    let name = WatchName::new("ready").unwrap();
    assert_eq!(g.watch_add(name.clone(), Predicate::TextEquals {
        target: Locator::Css { selector: "#change".into() }, text: "ready".into(),
    }).await.unwrap(), SampleValue::False);
    assert_eq!(g.now(), VirtualTime(17_333), "registering a watch must not move time");
    g.evaluate("setTimeout(()=>document.querySelector('#change').textContent='ready',10);true").await.unwrap();
    g.until(&name, VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.now(), VirtualTime(27_333));
    let trace = g.trace_path();
    g.stop().await.unwrap();
    let samples: Vec<_> = BehaviorLog::read(&trace).unwrap().into_iter().filter_map(|event| match event.kind {
        EventKind::Watch(sample) if sample.name == name => Some((event.t_us, sample.value)),
        _ => None,
    }).collect();
    assert_eq!(samples.first(), Some(&(VirtualTime(17_333), SampleValue::False)));
    assert_eq!(samples.iter().find(|(_, value)| *value == SampleValue::True), Some(&(VirtualTime(27_333), SampleValue::True)));
}
