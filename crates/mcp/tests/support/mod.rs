#![allow(dead_code)]
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Keep the recorder separate from fault-injection builds and other hosts. Linux tests mount this
/// repository alongside macOS tests, so a repository-relative shared executable can be overwritten
/// with the other host's binary while a replay is starting.
pub fn recorder_bin() -> PathBuf {
    // Focused native runs can explicitly reuse the recorder they just built in a shared cache.
    // The default still isolates this build from recorder fault-injection tests.
    if let Some(path) = std::env::var_os("FRONTCAP_TEST_RECORDER") {
        let path = PathBuf::from(path);
        assert!(path.is_absolute() && path.is_file(), "FRONTCAP_TEST_RECORDER must name the explicitly built recorder");
        return path;
    }
    static BUILD: Once = Once::new();
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"));
    let target_root = if target_root.is_absolute() {
        target_root
    } else {
        std::env::current_dir().unwrap().join(target_root)
    };
    let target = target_root.join("mcp-tests")
        .join(format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH));
    BUILD.call_once(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "frontcap-recorder", "--bin", "frontcap-recorder", "--target-dir"])
            .arg(&target)
            .current_dir(repo_root())
            .status()
            .unwrap();
        assert!(status.success(), "building the recorder failed");
    });
    target.join("debug/frontcap-recorder")
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("gif") => "image/gif",
        Some("png") => "image/png",
        Some("js") => "text/javascript; charset=utf-8",
        _ => "text/html; charset=utf-8",
    }
}

/// Serves `tests/fixtures` on 127.0.0.1.
pub fn serve() -> std::net::SocketAddr {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let rel = req.url().trim_start_matches('/').split('?').next().unwrap_or("").to_string();
            let resp = match std::fs::read(dir.join(&rel)) {
                Ok(bytes) => tiny_http::Response::from_data(bytes)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", content_type(&rel)).unwrap())
                    .boxed(),
                Err(_) => tiny_http::Response::from_string("not found").with_status_code(404).boxed(),
            };
            let _ = req.respond(resp);
        }
    });
    addr
}

pub fn toast_rules() -> Value {
    json!([{"id": "toast-api", "match": {"url": "https://api.example.test/toast"},
            "action": {"type": "mock", "status": 200,
                       "headers": {"content-type": "application/json", "access-control-allow-origin": "*"},
                       "body": "{\"text\":\"saved\"}", "latency_ms": 500}}])
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A `frontcap-mcp` process and a typed-enough MCP client for it.
pub struct Mcp {
    child: Child,
    stdin: ChildStdin,
    pending: Pending,
    null_answers: Arc<Mutex<Vec<oneshot::Sender<Value>>>>,
    next: i64,
    pub workspace: PathBuf,
    instructions: String,
}

impl Mcp {
    pub async fn start_in(workspace: &Path) -> Mcp {
        Self::start_from(workspace, &std::env::current_dir().unwrap()).await
    }

    /// A server whose working directory is `dir`.
    pub async fn start_from(workspace: &Path, dir: &Path) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_frontcap-mcp"))
            .current_dir(dir)
            .env("FRONTCAP_WORKSPACE", workspace)
            .env("FRONTCAP_RECORDER", recorder_bin())
            .env("FRONTCAP_HOME", repo_root())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let p = pending.clone();
        let null_answers: Arc<Mutex<Vec<oneshot::Sender<Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let n = null_answers.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: Value = serde_json::from_str(&line).unwrap();
                if let Some(id) = v["id"].as_i64() {
                    if let Some(tx) = p.lock().unwrap().remove(&id) {
                        let _ = tx.send(v);
                    }
                } else if v["id"].is_null() {
                    // Answers to lines whose id could not be read; matched to waiters in order.
                    let mut waiting = n.lock().unwrap();
                    if !waiting.is_empty() {
                        let _ = waiting.remove(0).send(v);
                    }
                }
            }
        });
        let mut m = Mcp { child, stdin, pending, null_answers, next: 1, workspace: workspace.to_path_buf(), instructions: String::new() };
        let init = m
            .request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "frontcap-tests", "version": "0"}}))
            .await;
        assert!(init["result"]["protocolVersion"].is_string(), "{init}");
        m.instructions = init["result"]["instructions"].as_str().unwrap_or_default().to_string();
        m.notify("notifications/initialized").await;
        m
    }

    /// The Server Instructions from `initialize`.
    #[allow(dead_code)]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// Sends a request and returns a receiver for its response, without waiting.
    pub async fn send(&mut self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let id = self.next;
        self.next += 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string() + "\n";
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
        rx
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let operation = params.get("name").and_then(Value::as_str).unwrap_or(method).to_owned();
        let rx = self.send(method, params).await;
        tokio::time::timeout(std::time::Duration::from_secs(60), rx)
            .await
            .unwrap_or_else(|_| panic!("{operation} ({method}) got no answer within 60 s"))
            .expect("the server answered")
    }

    pub async fn write_raw(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a hand-written line and returns the first answer whose id is null.
    pub async fn request_raw(&mut self, line: &str) -> Value {
        let (tx, rx) = oneshot::channel();
        self.null_answers.lock().unwrap().push(tx);
        self.write_raw(format!("{line}\n").as_bytes()).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), rx).await.expect("an answer").unwrap()
    }

    async fn notify(&mut self, method: &str) {
        let line = json!({"jsonrpc": "2.0", "method": method}).to_string() + "\n";
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    /// Calls a tool; returns (is_error, structured content).
    pub async fn try_tool(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args})).await;
        let result = &r["result"];
        assert!(result.is_object(), "{r}");
        (result["isError"].as_bool().unwrap(), result["structuredContent"].clone())
    }

    pub async fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let (is_error, v) = self.try_tool(name, args).await;
        assert!(!is_error, "{name} failed: {v}");
        v
    }

    /// The definition `name` of a drawer's `describe` of `of`, followed through its own `describe` as an
    /// agent does when the answer folds it.
    pub async fn definition(&mut self, drawer: &str, of: &str, name: &str) -> Value {
        let described = self.call_tool(drawer, json!({"operation": "describe", "of": of})).await;
        let definition = &described["schema"]["$defs"][name];
        match definition["describe"]["of"].as_str() {
            Some(pointer) => self.call_tool(drawer, json!({"operation": "describe", "of": pointer})).await["schema"].clone(),
            None => definition.clone(),
        }
    }

    pub async fn kill(mut self) {
        self.child.kill().await.unwrap();
    }
}

/// Kills every recorder and browser recorded in the workspace when the test ends.
pub struct Cleanup(pub PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(dirs) = std::fs::read_dir(self.0.join("runtimes")) else { return };
        for d in dirs.flatten() {
            if let Ok(Some(r)) = frontcap_recorder::runtime::load_runtime(&d.path()) {
                let _ = r.recorder.kill();
                if let Some(b) = r.browser {
                    let _ = b.kill();
                }
            }
        }
    }
}

/// A fresh workspace with cleanup; drop order kills the processes before the directory goes.
pub struct Workspace {
    pub cleanup: Cleanup,
    pub tmp: tempfile::TempDir,
}

/// With `FRONTCAP_KEEP_WORKSPACE` set, the directory stays for inspection, named on stderr with its test.
pub fn workspace() -> Workspace {
    let mut tmp = tempfile::tempdir().unwrap();
    if std::env::var_os("FRONTCAP_KEEP_WORKSPACE").is_some() {
        tmp.disable_cleanup(true);
        eprintln!("workspace of {}: {}", std::thread::current().name().unwrap_or("?"), tmp.path().display());
    }
    Workspace { cleanup: Cleanup(tmp.path().join(".frontcap")), tmp }
}

impl Workspace {
    pub fn root(&self) -> PathBuf {
        self.tmp.path().join(".frontcap")
    }
}
