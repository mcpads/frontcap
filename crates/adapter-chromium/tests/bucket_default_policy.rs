//! Authored local default-bucket policies survive initialization before page code runs.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::rules::RuleSet;
use serde_json::json;

#[tokio::test]
async fn expired_local_default_policy_preserves_initial_cache_and_indexed_db() {
    let origin="https://default-policy.test";
    let rules=RuleSet::from_json(&json!([{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let runs=tempfile::tempdir().unwrap();
    let mut options=support::options(runs.path(),"expired-local-default",format!("{origin}/start"),rules);
    options.profile.initial_buckets=serde_json::from_value(json!([{"origin":origin,"expiration":"12591158399999000","quota":"1048576","persistent":true,"durability":"relaxed"}])).unwrap();
    options.profile.initial_caches=serde_json::from_value(json!([{"origin":origin,"name":"kept","entries":[{"request_url":format!("{origin}/item"),"status":200,"body":"saved"}]}])).unwrap();
    options.profile.initial_indexed_db=serde_json::from_value(json!([{"origin":origin,"name":"records","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"key"},"value":"saved"}]}]}])).unwrap();
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(frontcap_core::time::VirtualDuration::from_ms(1)).await.unwrap();
    let live=g.live_state().await.unwrap();
    assert_eq!(live.caches.len(),1);
    assert_eq!(live.caches[0].entries[0].body_bytes().unwrap(),b"saved");
    assert_eq!(live.indexed_db.len(),1);
    assert_eq!(live.indexed_db[0].stores[0].records[0].value,"saved");
    assert_eq!(g.now().0,1_000);
    g.stop().await.unwrap();
}
