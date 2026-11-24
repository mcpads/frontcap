mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn cookie_clock_diagnostic() {
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"page","match":{"url":"http://cookie.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>cookie clock</p>"}}]"#).unwrap();
    let mut options = support::options(runs.path(), "cookie-clock-diagnostic", "http://cookie.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.eval_main("document.cookie='virtual=kept; Max-Age=10; Path=/';true").await.unwrap();
    let before = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    g.advance(VirtualDuration(9_999_999)).await.unwrap();
    let before_expiry = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    g.advance(VirtualDuration(1)).await.unwrap();
    let after_virtual = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    g.eval_main("document.cookie='wall=kept; Max-Age=1; Path=/';true").await.unwrap();
    let warm = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    let stopped = g.now();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let after_wall = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    assert_eq!(g.now(), stopped);
    g.advance(VirtualDuration(1_000_000)).await.unwrap();
    let after_second_expiry = g.eval_main("({cookies:document.cookie,date:Date.now()})").await.unwrap();
    println!("COOKIE_CLOCK_PROBE {}", serde_json::json!({"before":before,"just_before_expiry":before_expiry,"after_virtual_10s":after_virtual,"warm":warm,"after_host_1500ms":after_wall,"stopped_us":stopped.0,"after_second_expiry":after_second_expiry}));
    g.stop().await.unwrap();
}
