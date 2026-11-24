#![allow(dead_code)]
pub mod shared_time;
use frontcap_chromium::generation::LaunchOptions;
use frontcap_core::ids::RunId;
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::rules::RuleSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Serves `tests/fixtures` on 127.0.0.1 in a background thread.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("gif") => "image/gif",
        _ => "text/html; charset=utf-8",
    }
}

pub fn serve() -> std::net::SocketAddr {
    serve_on("127.0.0.1:0")
}

/// Serves `tests/fixtures` on `bind` (`[::]:0` takes IPv6 and, mapped, IPv4).
pub fn serve_on(bind: &str) -> std::net::SocketAddr {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let server = tiny_http::Server::http(bind).unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let rel = req.url().trim_start_matches('/').split('?').next().unwrap_or("").to_string();
            // `?oac` asks for an origin-keyed agent cluster (its own process under isolation).
            let oac = req.url().split_once('?').is_some_and(|(_, q)| q.split('&').any(|p| p == "oac"));
            let resp = match std::fs::read(dir.join(&rel)) {
                Ok(bytes) => {
                    let r = tiny_http::Response::from_data(bytes).with_header(tiny_http::Header::from_bytes("Content-Type", content_type(&rel)).unwrap());
                    if oac { r.with_header(tiny_http::Header::from_bytes("Origin-Agent-Cluster", "?1").unwrap()).boxed() } else { r.boxed() }
                }
                Err(_) => tiny_http::Response::from_string("not found").with_status_code(404).boxed(),
            };
            let _ = req.respond(resp);
        }
    });
    addr
}

pub fn options(runs: &Path, run: &str, url: String, rules: RuleSet) -> LaunchOptions {
    LaunchOptions {
        repo_root: repo_root(),
        runs_root: runs.to_path_buf(),
        run: RunId::new(run).unwrap(),
        url,
        profile: DeterminismProfile { host_class: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), ..Default::default() },
        rules,
        passthrough_origins: None,
        watchdog: Duration::from_secs(10),
        browser_profile: None,
        launch_id: None,
        on_browser: None,
        proxy: None,
        upstream: None,
        cell: None,
    }
}

/// Require the engine built from the current inputs. Explicit baseline runs instead
/// verify that the trace records all missing engine changes before the caller returns.
pub async fn controlled_engine_or_check_baseline() -> bool {
    let root = repo_root();
    if browser_pin::engine::patched_binary(&root, browser_pin::current_platform().unwrap()).unwrap().is_some() {
        return true;
    }
    let id = browser_pin::engine::patchset_id(&root).unwrap();
    assert!(
        std::env::var("FRONTCAP_ALLOW_BASELINE").is_ok_and(|v| v == "1"),
        "no patched engine for patch set {id}: `browser-pin engine build --src <chromium/src>`, or FRONTCAP_ALLOW_BASELINE=1"
    );
    let addr = serve();
    let runs = tempfile::tempdir().unwrap();
    let g = frontcap_chromium::generation::Generation::launch(options(runs.path(), "baseline", format!("http://{addr}/clock_jitter.html"), RuleSet::default())).await.unwrap();
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    g.stop().await.unwrap();
    let missing = browser_pin::engine::patches(&root).unwrap().into_iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect::<Vec<_>>().join(", ");
    assert!(!missing.is_empty(), "the test requires controlled engine changes");
    assert!(trace.contains("\"engine_baseline\"") &&
        trace.contains(&format!("the baseline lacks {missing};")), "{trace}");
    false
}

/// A browser profile directory for a detached browser, which outlives its generation by design (a
/// recorder that dies leaves it running): dropped, it kills that browser — however the test ended — and
/// then removes the directory.
pub struct DetachedProfile {
    dir: tempfile::TempDir,
    browser: std::sync::Arc<std::sync::Mutex<Option<frontcap_core::process::ProcessIdentity>>>,
}

impl DetachedProfile {
    pub fn new() -> DetachedProfile {
        DetachedProfile { dir: tempfile::tempdir().unwrap(), browser: Default::default() }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Launches with this profile and remembers the browser.
    pub fn apply(&self, opts: &mut LaunchOptions) {
        opts.browser_profile = Some(self.dir.path().to_path_buf());
        let slot = self.browser.clone();
        opts.on_browser = Some(Box::new(move |id| *slot.lock().unwrap() = Some(id.clone())));
    }
}

impl Drop for DetachedProfile {
    fn drop(&mut self) {
        if let Some(id) = self.browser.lock().unwrap().take() {
            let _ = id.kill();
        }
    }
}
