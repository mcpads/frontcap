#![allow(dead_code)]
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::rules::RuleSet;
use frontcap_recorder::launcher::Workspace;
use frontcap_recorder::runtime::{self, LaunchSpec, RuntimeState};
use frontcap_recorder_protocol::client::RecorderClient;
use frontcap_recorder_protocol::{Call, Op, Reply};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

pub fn recorder_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_frontcap-recorder"))
}

/// Writes an executable through a child process. A child that another test thread spawns inherits
/// this process's open files until it executes its own program, and Linux refuses to execute a
/// file that any process holds open for writing (`ETXTBSY`). So this process never opens it for
/// writing.
pub fn write_program(path: &Path, text: &str) {
    use std::io::Write as _;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
}

/// Serves `tests/fixtures` on 127.0.0.1 in a background thread.
pub fn serve() -> std::net::SocketAddr {
    serve_dir("tests/fixtures")
}

/// Serves `rel` (relative to the crate) on 127.0.0.1 in a background thread.
pub fn serve_dir(rel: &str) -> std::net::SocketAddr {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let rel = req.url().trim_start_matches('/').split('?').next().unwrap_or("").to_string();
            let resp = match std::fs::read(dir.join(&rel)) {
                Ok(bytes) => tiny_http::Response::from_data(bytes)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
                    .boxed(),
                Err(_) => tiny_http::Response::from_string("not found").with_status_code(404).boxed(),
            };
            let _ = req.respond(resp);
        }
    });
    addr
}

pub fn workspace() -> (tempfile::TempDir, Workspace) {
    let mut tmp = tempfile::tempdir().unwrap();
    if std::env::var("FRONTCAP_KEEP_WORKSPACE").is_ok() {
        tmp.disable_cleanup(true);
        eprintln!("workspace kept at {}", tmp.path().display());
    }
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    (tmp, ws)
}

/// A workspace nested `depth` characters deep, beyond the platform's socket path limit.
pub fn deep_workspace(depth: usize) -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join("d".repeat(depth)).join(".frontcap") };
    (tmp, ws)
}

pub fn profile() -> DeterminismProfile {
    DeterminismProfile { host_class: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), ..Default::default() }
}

pub fn spec(ws: &Workspace, request_id: &str, url: &str, rules: RuleSet) -> LaunchSpec {
    let launch_id = LaunchId::generate();
    LaunchSpec {
        request_id: RequestId::new(request_id).unwrap(),
        run_id: RunId::new(launch_id.as_str().replacen("g-", "r-", 1)).unwrap(),
        launch_id,
        url: url.to_string(),
        profile: profile(),
        rules,
        passthrough_origins: None,
        watchdog_ms: 10_000,
        repo_root: repo_root(),
        runs_root: ws.runs(),
        origin: None,
        restore: None,
    }
}

/// A runtime directory whose recorder and browser are killed (by identity) when the test ends, so a
/// failing test leaves no processes behind.
pub struct Runtime(pub PathBuf);

impl std::ops::Deref for Runtime {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Ok(Some(r)) = runtime::load_runtime(&self.0) {
            let _ = r.recorder.kill();
            if let Some(b) = r.browser {
                let _ = b.kill();
            }
        }
    }
}

/// Starts a recorder for `spec` directly (without the launcher) and connects to it.
pub async fn spawn_recorder(ws: &Workspace, spec: LaunchSpec) -> (Runtime, RecorderClient) {
    let dir = ws.runtime_dir(&spec.launch_id);
    std::fs::create_dir_all(&dir).unwrap();
    runtime::save_spec(&dir, &spec).unwrap();
    let mut child = std::process::Command::new(recorder_bin())
        .args(["launch", "--runtime-dir"])
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(if std::env::var("FRONTCAP_RECORDER_STDERR").is_ok() {
            std::process::Stdio::inherit()
        } else {
            std::fs::File::create(dir.join("recorder.log")).unwrap().into()
        })
        .spawn()
        .unwrap();
    // Reap the recorder when it exits; its lifetime is the generation's, not this handle's.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let dir = Runtime(dir);
    wait_ready(&dir).await;
    let c = connect(&dir).await;
    (dir, c)
}

pub async fn wait_ready(dir: &Path) {
    for _ in 0..3000 {
        if let Ok(Some(r)) = runtime::load_runtime(dir) {
            match r.state {
                RuntimeState::Ready => return,
                RuntimeState::Failed { reason, .. } => panic!("recorder failed: {reason}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("recorder not ready: {}", std::fs::read_to_string(dir.join("recorder.log")).unwrap_or_default());
}

pub async fn connect(dir: &Path) -> RecorderClient {
    let r = runtime::load_runtime(dir).unwrap().expect("runtime.json");
    RecorderClient::connect(&r.socket).await.unwrap().0
}

pub async fn wait_until_exited(dir: &Path) {
    for _ in 0..1500 {
        if let Ok(Some(r)) = runtime::load_runtime(dir)
            && r.state == RuntimeState::Stopped
            && !r.recorder.is_alive()
        {
            assert!(r.browser.as_ref().is_none_or(|b| !b.is_alive()), "the browser is stopped too");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("recorder did not exit");
}

/// Reattaches (waiting out a lease held by a dead connection) and stops the generation.
pub async fn stop(dir: &Path) {
    let mut c = connect(dir).await;
    let mut lease = None;
    for _ in 0..500 {
        if let Ok(Reply::Attached { lease: l, .. }) = c.call(Call::Reattach { request_id: RequestId::new("stop-attach").unwrap() }).await {
            lease = Some(l);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.call(Call::Mutate { lease, request_id: RequestId::new("stop").unwrap(), op: Op::Stop }).await.unwrap();
    wait_until_exited(dir).await;
}

pub fn launch_request(request_id: &str, url: &str) -> frontcap_recorder::launcher::LaunchRequest {
    frontcap_recorder::launcher::LaunchRequest {
        request_id: RequestId::new(request_id).unwrap(),
        url: url.to_string(),
        profile: profile(),
        rules: RuleSet::default(),
        passthrough_origins: None,
        watchdog_ms: 10_000,
        repo_root: repo_root(),
        origin: None,
        restore: None,
    }
}
