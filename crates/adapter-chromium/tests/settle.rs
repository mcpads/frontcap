mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::clock::SettleOutcome;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    (runs, g)
}

#[tokio::test]
async fn quiet_page_settles_before_a_distant_timer() {
    let (_runs, mut g) = launched("settle-1", "timer.html").await;
    let r = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(2000)).await.unwrap();
    let SettleOutcome::Quiescent { t, next_timer_t: Some(due) } = r else { panic!("{r:?}") };
    // Set when the script ran, 60 s ahead of then.
    assert!(due.0 >= 60_000_000 && due.0 <= t.0 + 60_000_000, "{r:?}");
    assert_eq!(g.eval_main("document.title").await.unwrap(), "timer");
}

#[tokio::test]
async fn the_controllers_own_timers_give_no_hint() {
    let (_runs, mut g) = launched("settle-10", "quiet.html").await;
    // A watch samples on frame ticks from the controller's own world.
    let name = frontcap_core::ids::WatchName::new("shown").unwrap();
    let selector = frontcap_core::predicate::Locator::Css { selector: "#p".into() };
    g.watch_add(name, frontcap_core::predicate::Predicate::Visible { target: selector }).await.unwrap();
    let r = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(2000)).await.unwrap();
    assert!(matches!(r, SettleOutcome::Quiescent { next_timer_t: None, .. }), "{r:?}");
}

#[tokio::test]
async fn endless_animation_exhausts_budget() {
    let (_runs, mut g) = launched("settle-2", "spinner.html").await;
    let r = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(1000)).await.unwrap();
    match r {
        SettleOutcome::BudgetExhausted { still_active, .. } => assert!(still_active.contains(&"animation".to_string())),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_zero_idle_window_is_refused() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = frontcap_chromium::generation::Generation::launch(support::options(runs.path(), "settle-9", format!("http://{addr}/spinner.html"), frontcap_core::rules::RuleSet::default())).await.unwrap();
    g.advance(frontcap_core::time::VirtualDuration::from_ms(50)).await.unwrap();
    let before = g.now();
    let wall = std::time::Instant::now();
    let err = g.settle(frontcap_core::time::VirtualDuration(0), frontcap_core::time::VirtualDuration::from_ms(100)).await.unwrap_err();
    assert!(matches!(err, frontcap_chromium::ChromiumError::InvalidArgument(_)), "{err:?}");
    assert!(wall.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(g.now(), before);
    g.advance(frontcap_core::time::VirtualDuration::from_ms(1)).await.expect("the generation stays usable");
}

async fn next_timer(g: &mut Generation) -> Option<u64> {
    match g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(2000)).await.unwrap() {
        SettleOutcome::Quiescent { next_timer_t, .. } => next_timer_t.map(|t| t.0),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn settle_reports_worker_timer_changes_and_termination() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mock = |id: &str, url: &str, mime: &str, body: &str| serde_json::json!({"id":id,"match":{"url":url},
        "action":{"type":"mock","status":200,"headers":{"content-type":mime},"body":body}});
    let page = r#"<!doctype html><output id=events></output><script>
window.received=[];
window.startWorker=()=>{
  window.worker=new Worker('/worker.js');
  worker.onmessage=e=>{received.push(e.data);document.querySelector('#events').textContent=e.data;};
};
startWorker();
</script>"#;
    let worker = r#"const nested=new Worker('/nested.js');
nested.onmessage=e=>postMessage(['nested',e.data,Date.now()]);
nested.onerror=e=>postMessage(['error',e.message]);
const interval=setInterval(()=>postMessage('interval'),15000);
setTimeout(()=>postMessage('timeout'),60000);
onmessage=e=>{
  if(e.data==='cancelInterval')clearInterval(interval);
  if(e.data==='cancelNested')nested.terminate();
  postMessage(e.data);
};"#;
    let rules = RuleSet::from_json(&serde_json::json!([
        mock("page","https://app.example.test/","text/html",page),
        mock("worker","https://app.example.test/worker.js","text/javascript",worker),
        mock("nested","https://app.example.test/nested.js","text/javascript","postMessage('started');setTimeout(()=>postMessage('nested'),45000)")
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(),"worker-timer-hint","https://app.example.test/".into(),rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(next_timer(&mut g).await,Some(15_000_000));
    g.advance(VirtualDuration(15_000_000-g.now().0)).await.unwrap();
    assert_eq!(next_timer(&mut g).await,Some(30_000_000),"a repeated timer publishes its next firing");
    g.eval_main("worker.postMessage('cancelInterval')").await.unwrap();
    let hint=next_timer(&mut g).await;
    assert_eq!(hint,Some(45_000_000),"the nested worker becomes earliest: {}",g.eval_main("JSON.stringify(received)").await.unwrap());
    g.eval_main("worker.postMessage('cancelNested')").await.unwrap();
    assert_eq!(next_timer(&mut g).await,Some(60_000_000),"termination removes the nested timer");
    g.advance(VirtualDuration(60_000_000-g.now().0)).await.unwrap();
    assert_eq!(next_timer(&mut g).await,None,"a fired one-shot timer leaves no hint");
    let started=g.now().0;
    g.eval_main("worker.terminate();startWorker()").await.unwrap();
    assert_eq!(next_timer(&mut g).await,Some(started+15_000_000));
    g.eval_main("worker.terminate()").await.unwrap();
    assert_eq!(next_timer(&mut g).await,None,"termination removes all remaining worker timers");
    g.stop().await.unwrap();
}
