//! A delivery at a frame tick must reach its continuation before that tick's frame.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

async fn run(addr: std::net::SocketAddr, case: usize) -> (tempfile::TempDir, serde_json::Value) {
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[
        {"id":"go","match":{"url":"https://api.example.test/go"},"action":{"type":"mock","status":200,"body":"g","latency_ms":50}},
        {"id":"held","match":{"url":"https://api.example.test/never"},"action":{"type":"hold"}}
    ]"#).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), &format!("response-{case}"), format!("http://{addr}/response_frame.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    let held = case & 2 != 0;
    let read_body = case & 4 != 0;
    g.eval_main(&format!("start({held}, {read_body}); 'started'")).await.unwrap();
    if case & 1 != 0 {
        for _ in 0..400 { g.advance(VirtualDuration::from_ms(1)).await.unwrap(); }
    } else {
        g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    }
    let evidence = g.eval_main("evidence()").await.unwrap();
    assert_eq!(g.held().len(), usize::from(held), "an unrelated deliberate hold must not block rendering");
    assert_eq!(g.now().0, 417_000);
    g.stop().await.unwrap();
    (runs, evidence)
}

#[tokio::test]
async fn response_continuation_precedes_its_frame_for_different_slicing_and_concurrent_worlds() {
    let addr = support::serve();
    let mut tasks = Vec::new();
    for i in 0..8 {
        tasks.push(tokio::spawn(run(addr, i)));
    }
    let expected = serde_json::json!([["request",17],["response",67],["frame1",67],["frame2",84],["transitionrun",101],["transitionend",284]]);
    let mut failures = Vec::new();
    for task in tasks {
        let (runs, observed) = task.await.unwrap();
        if observed != expected {
            let kept = runs.keep();
            std::fs::write(kept.join("observed.json"), serde_json::to_vec_pretty(&observed).unwrap()).unwrap();
            failures.push(format!("{}: {observed}", kept.display()));
        }
    }
    assert!(failures.is_empty(), "response/frame ordering differs; retained evidence:\n{}", failures.join("\n"));
}
