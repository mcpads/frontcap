//! A zero-length progress includes what a dedicated worker does with a response it receives at that
//! instant: its callbacks run, and what they post reaches the page, before the progress returns. A
//! worker that cannot run them now (blocked in an atomic wait) is not waited for.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

/// Fetches the URL it is sent and posts the body back; with a shared buffer, waits on its first word
/// right after issuing, until the page notifies it.
const WORKER: &str = r#"
onmessage=e=>{
  fetch(e.data.url).then(r=>r.text()).then(b=>postMessage(b));
  if(e.data.buffer)Atomics.wait(new Int32Array(e.data.buffer),0,0);
};
"#;

/// Serves a cross-origin isolated page at `/`, and `late` at `/slow` after `delay` of wall-clock time.
fn serve_slow(delay: Duration) -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            std::thread::spawn(move || {
                let header = |name: &str, value: &str| tiny_http::Header::from_bytes(name, value).unwrap();
                let response = if req.url().starts_with("/slow") {
                    std::thread::sleep(delay);
                    tiny_http::Response::from_string("late").with_header(header("Content-Type", "text/plain"))
                } else {
                    tiny_http::Response::from_string("<!doctype html><p>worker responses</p>")
                        .with_header(header("Content-Type", "text/html"))
                        .with_header(header("Cross-Origin-Opener-Policy", "same-origin"))
                        .with_header(header("Cross-Origin-Embedder-Policy", "require-corp"))
                };
                let _ = req.respond(response);
            });
        }
    });
    addr
}

/// A page with one started worker whose messages land in `got`.
async fn launch(runs: &std::path::Path, run: &str, addr: std::net::SocketAddr) -> Generation {
    let mut options = support::options(runs, run, format!("http://{addr}/"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}")]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    g.eval_main(&format!("window.got=[];window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));\
        worker.onmessage=e=>got.push(e.data);true", json!(WORKER))).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g
}

/// The response takes wall-clock time to arrive, after every task of the instant has run.
#[tokio::test]
async fn a_zero_length_progress_includes_a_workers_callbacks_for_its_response() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let addr = serve_slow(Duration::from_millis(500));
    let runs = tempfile::tempdir().unwrap();
    let mut g = launch(runs.path(), "worker-response", addr).await;
    let at = g.now();
    g.eval_main("worker.postMessage({url:location.origin+'/slow'});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.now(), at);
    assert_eq!(g.eval_main("got").await.unwrap(), json!(["late"]), "the worker's answer reaches the page at its instant");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_worker_in_an_atomic_wait_does_not_hold_a_zero_length_progress() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let addr = serve_slow(Duration::from_millis(500));
    let runs = tempfile::tempdir().unwrap();
    let mut g = launch(runs.path(), "worker-response-waiting", addr).await;
    let at = g.now();
    g.eval_main("window.words=new Int32Array(new SharedArrayBuffer(4));worker.postMessage({url:location.origin+'/slow',buffer:words.buffer});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.now(), at);
    assert_eq!(g.eval_main("got").await.unwrap(), json!([]), "a waiting worker cannot handle its answer");
    // Woken, it handles the answer at the same instant.
    g.eval_main("Atomics.store(words,0,1);Atomics.notify(words,0);true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.now(), at);
    assert_eq!(g.eval_main("got").await.unwrap(), json!(["late"]));
    g.stop().await.unwrap();
}
