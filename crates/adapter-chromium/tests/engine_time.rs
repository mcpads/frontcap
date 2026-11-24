//! Seeded clock coarsening: the page's clock is coarsened from the run's seed and relative to its time origin, so
//! the same run reads the same times every time (S4 found them random per run on the baseline engine).
//! These tests need the patched engine; without it they fail, unless `FRONTCAP_ALLOW_BASELINE=1`, which
//! checks instead that a baseline run says what it lacks.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launch(run: &str, seed: u64) -> (Generation, tempfile::TempDir) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), run, format!("http://{addr}/clock_jitter.html"), RuleSet::default());
    o.profile.random_seed = seed;
    (Generation::launch(o).await.unwrap(), runs)
}

async fn reads(run: &str, seed: u64) -> Vec<serde_json::Value> {
    let (mut g, _runs) = launch(run, seed).await;
    assert_eq!(g.evaluate("typeof window.reads").await.unwrap(), "undefined");
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert_eq!(g.evaluate("window.scriptStartedAt").await.unwrap(), serde_json::json!(0));
    let v: Vec<serde_json::Value> = serde_json::from_str(g.eval_main("JSON.stringify(reads)").await.unwrap().as_str().unwrap()).unwrap();
    assert_eq!(v.len(), 80, "60 timers and 20 frames: {v:?}");
    g.stop().await.unwrap();
    v
}

#[tokio::test]
async fn the_page_reads_virtual_time_exactly_and_the_same_in_every_run() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let a = reads("jit-a", 7).await;
    // The initial script runs at zero on the first explicit grant. Each timer
    // retains its own deadline; whole milliseconds remain exact under coarsening.
    let timer_reads: Vec<&serde_json::Value> = a.iter().filter(|r| r.as_array().unwrap().len() == 2).collect();
    let timers: Vec<i64> = timer_reads.iter().map(|r| r[0].as_i64().unwrap()).collect();
    let expected: Vec<i64> = (1..=60).map(|i: i64| i * 1000).collect();
    assert_eq!(timers, expected, "{a:?}");
    assert!(timer_reads.iter().all(|r| r[0] == r[1]), "an event's timeStamp is the same clock: {a:?}");
    assert_eq!(a, reads("jit-b", 7).await, "the same seed, the same times, animation frames included");
}

/// Instants off the 100 µs grid: the page reads them coarsened to 100 µs, rounding the way the seed
/// decides — the same for the same seed, differently for another.
async fn off_grid(run: &str, seed: u64) -> Vec<i64> {
    let (mut g, _runs) = launch(run, seed).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    let mut out = Vec::new();
    for step in [1_337u64, 2_789, 4_013, 1_551, 3_666, 2_222, 1_909, 4_444, 3_131, 2_575, 1_717, 3_939] {
        g.advance(VirtualDuration(step)).await.unwrap();
        let t = g.eval_main("Math.round(performance.now() * 1000)").await.unwrap().as_i64().unwrap();
        out.push(t);
    }
    g.stop().await.unwrap();
    out
}

#[tokio::test]
async fn off_grid_instants_are_coarsened_from_the_seed() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let a = off_grid("jit-o1", 7).await;
    assert!(a.iter().all(|t| t % 100 == 0), "coarsened to 100 µs: {a:?}");
    assert_eq!(a, off_grid("jit-o2", 7).await, "the same seed, the same rounding");
    assert_ne!(a, off_grid("jit-o3", 8).await, "another seed rounds otherwise somewhere");
}

#[tokio::test]
async fn native_clock_inventory_preserves_identity_and_does_not_advance_time() {
    use cdp::protocol::{emulation, target};
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let (mut generation, _runs) = launch("clock-inventory", 7).await;
    generation.advance(VirtualDuration::from_ms(20)).await.unwrap();
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true),
        ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    let session = Some(attached.session_id.0.as_str());
    let before = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert!(before.enabled && !before.renderer_id.is_empty());
    let initial: i128 = before.unix_time_us.as_ref().unwrap().parse().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let held = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_eq!(held.renderer_id, before.renderer_id);
    assert_eq!(held.unix_time_us, before.unix_time_us);
    generation.advance(VirtualDuration::from_micros(1_337)).await.unwrap();
    let advanced = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_eq!(advanced.renderer_id, before.renderer_id);
    assert_eq!(advanced.unix_time_us.unwrap().parse::<i128>().unwrap(), initial + 1_337);
    client.call(None, &target::DetachFromTargetParams {
        session_id: Some(attached.session_id), ..Default::default()
    }).await.unwrap();
    generation.stop().await.unwrap();
}
