#![cfg(all(unix, feature = "fault-injection"))]
mod support;
use frontcap_core::{ids::{LaunchId, RequestId}, journal::{Journal, RequestState}, process::ProcessIdentity};
use frontcap_recorder::{launcher::{self, Workspace}, runtime::{self, LaunchSpec, RuntimeRecord, RuntimeState}};
use frontcap_recorder_protocol::{Answer, Outcome, Reply};
use std::process::{Child, Command};

struct OwnedProcess(Child);
impl Drop for OwnedProcess {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

struct Execution {
    _process: OwnedProcess,
    _tmp: tempfile::TempDir,
    ws: Workspace,
    id: LaunchId,
    browser: ProcessIdentity,
    original: Vec<u8>,
}
impl Execution {
    fn new() -> Self {
        let child = OwnedProcess(Command::new("sleep").arg("120").spawn().unwrap());
        let browser = ProcessIdentity::of(child.0.id()).unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace { root: tmp.path().to_path_buf() };
        let id = LaunchId::new("g-termination").unwrap();
        let dir = ws.runtime_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        runtime::save_spec(&dir, &LaunchSpec {
            request_id: RequestId::new("launch").unwrap(), launch_id: id.clone(), run_id: launcher::run_id_of(&id),
            url: "about:blank".into(), profile: Default::default(), rules: Default::default(), passthrough_origins: None,
            watchdog_ms: 20_000, repo_root: tmp.path().to_path_buf(), runs_root: ws.runs(), origin: None, restore: None,
        }).unwrap();
        runtime::save_runtime(&dir, &RuntimeRecord {
            launch_id: id.clone(), state: RuntimeState::Failed { reason: "original failure".into(), error: None },
            recorder: ProcessIdentity::current(), browser: Some(browser.clone()), endpoint: None, socket: dir.join("recorder.sock"),
        }).unwrap();
        let original = std::fs::read(runtime::runtime_path(&dir)).unwrap();
        Self { _process: child, _tmp: tmp, ws, id, browser, original }
    }
    fn child(&self) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "termination_owner", "--nocapture"])
            .env("FRONTCAP_TERMINATION_WORKSPACE", &self.ws.root);
        cmd
    }
    fn state(&self) -> RequestState {
        Journal::read_state(&runtime::journal_path(&self.ws.runtime_dir(&self.id)), &RequestId::new("stop").unwrap()).unwrap().unwrap()
    }
    async fn finish(&self) {
        let id = RequestId::new("stop").unwrap();
        let bundle = launcher::terminate_failed(&self.ws, &self.id, &id).await.unwrap();
        assert!(!self.browser.is_alive());
        assert!(!bundle.join("manifest.json").exists());
        assert_eq!(std::fs::read(runtime::runtime_path(&self.ws.runtime_dir(&self.id))).unwrap(), self.original);
        let RequestState::Completed(value) = self.state() else { panic!("stop is not durable") };
        assert!(matches!(serde_json::from_value::<Answer>(value).unwrap(), Answer::Ok(Reply::Done { outcome: Outcome::Stopped { finalized: false }, .. })));
        let path = runtime::journal_path(&self.ws.runtime_dir(&self.id));
        let before = std::fs::read(&path).unwrap();
        launcher::terminate_failed(&self.ws, &self.id, &id).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[tokio::test]
#[ignore = "subprocess fixture invoked by termination recovery oracles"]
async fn termination_owner() {
    let ws = Workspace { root: std::env::var_os("FRONTCAP_TERMINATION_WORKSPACE").unwrap().into() };
    let result = launcher::terminate_failed(&ws, &LaunchId::new("g-termination").unwrap(), &RequestId::new("stop").unwrap()).await;
    if std::env::var_os("FRONTCAP_EXPECT_TERMINATION_ERROR").is_some() {
        let error = result.unwrap_err();
        assert!(error.0.message.contains("oracle refusal"), "{error}");
        assert!(error.0.recovery.contains("same request id"));
    } else { result.unwrap(); }
}

#[tokio::test]
async fn termination_resumes_after_each_process_loss_boundary() {
    for point in ["terminate_before_dispatch", "terminate_after_dispatch", "terminate_after_exit", "terminate_after_complete"] {
        let execution = Execution::new();
        let output = execution.child().env("FRONTCAP_FAULT", format!("stop@{point}")).output().unwrap();
        assert!(!output.status.success(), "fault did not abort at {point}");
        match point {
            "terminate_before_dispatch" => assert_eq!(execution.state(), RequestState::Intent),
            "terminate_after_complete" => assert!(matches!(execution.state(), RequestState::Completed(_))),
            _ => assert_eq!(execution.state(), RequestState::Dispatched),
        }
        assert_eq!(execution.browser.is_alive(), matches!(point, "terminate_before_dispatch" | "terminate_after_dispatch"));
        execution.finish().await;
    }
}

#[tokio::test]
async fn termination_signal_and_probe_failures_remain_pending_until_exit_is_proven() {
    for program in ["kill", "ps"] {
        let execution = Execution::new();
        let mock_dir = tempfile::tempdir().unwrap();
        let mock = mock_dir.path().join(program);
        support::write_program(&mock, "#!/bin/sh\nprintf 'oracle refusal\\n' >&2\nexit 2\n");
        let mut paths = vec![mock_dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let output = execution.child().env("PATH", std::env::join_paths(paths).unwrap())
            .env("FRONTCAP_EXPECT_TERMINATION_ERROR", "1").output().unwrap();
        assert!(output.status.success(), "{program}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(execution.browser.is_alive());
        assert_eq!(execution.state(), RequestState::Dispatched);
        execution.finish().await;
    }
}
