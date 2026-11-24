//! Launch-window names are construction inputs, not post-start mutations.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn initial_window_name_precedes_scripts_and_preserves_literal_text() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://window-name.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":url},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>window.first=name</script>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "initial-window-name", url.into(), rules);
    let name = "root';window.injected=true;//\n한글";
    options.profile.initial_window_name = Some(name.into());
    options.profile = serde_json::from_slice(&serde_json::to_vec(&options.profile).unwrap()).unwrap();
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(g.now().0, 0);
    assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined");
    assert_eq!(g.eval_main("name").await.unwrap(), name);
    assert_eq!(g.eval_main("typeof injected").await.unwrap(), "undefined");
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("first").await.unwrap(), name);
    g.stop().await.unwrap();
}
