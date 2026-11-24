use frontcap_core::ids::RequestId;
use frontcap_native::request::{
    Completion, Identity, NativeRequest, Operation, Target,
};
use frontcap_native::store::{Recovery, RequestStore, StoreError};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GENERATION: &str = "4d75203e-0a3e-4c68-8e3b-77f0a831ab83";
const CONTROLLER: &str = "7a30a129-7ab1-4f10-b218-9e178f794970";
fn identity() -> Identity {
    Identity {
        target: Target::AndroidViews,
        device: "emulator-5580".into(),
        generation: GENERATION.into(),
        pid: 123,
    }
}
fn request_id() -> RequestId {
    RequestId::new("undo").unwrap()
}
fn operation() -> Operation {
    Operation::Advance { by_us: 1 }
}

#[test]
fn child_owner() {
    let Ok(root) = std::env::var("FRONTCAP_NATIVE_OWNER_TEST") else {
        return;
    };
    let root = Path::new(&root);
    let phase = std::env::var("FRONTCAP_NATIVE_OWNER_PHASE").unwrap();
    let mut store = RequestStore::create(
        &root.join("session"),
        identity(),
        CONTROLLER.into(),
        SOURCE.into(),
    )
    .unwrap();
    store.prepare(request_id(), operation()).unwrap();
    if phase != "prepared" {
        let request = store.dispatch(&request_id()).unwrap();
        if phase == "completed" {
            let NativeRequest::Clock(request) = request else {
                panic!()
            };
            store
                .complete(
                    &request_id(),
                    Completion::Clock {
                        generation: GENERATION.into(),
                        pid: 123,
                        last_id: 1,
                        before_ns: 0, now_ns: 1000, applied: true,
                        request,
                        receipt_sha256: SOURCE.into(),
                    },
                )
                .unwrap();
        }
    }
    std::fs::write(root.join("ready"), phase).unwrap();
    // The parent kills this process while it still owns the store and its OS lock.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_owner_releases_lock_and_preserves_each_request_boundary() {
    for phase in ["prepared", "dispatched", "completed"] {
        let temp = tempfile::tempdir().unwrap();
        let mut child = OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "child_owner", "--nocapture"])
                .env("FRONTCAP_NATIVE_OWNER_TEST", temp.path())
                .env("FRONTCAP_NATIVE_OWNER_PHASE", phase)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !temp.path().join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before {phase}"
            );
            assert!(Instant::now() < deadline, "child never reached {phase}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            RequestStore::open(&temp.path().join("session"), &identity(), SOURCE),
            Err(StoreError::Busy)
        ));
        child.0.kill().unwrap();
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        let mut reopened =
            RequestStore::open(&temp.path().join("session"), &identity(), SOURCE).unwrap();
        let restored = reopened.prepare(request_id(), operation()).unwrap();
        match (phase, restored) {
            ("prepared", Recovery::Prepared(NativeRequest::Clock(r)))
            | ("dispatched", Recovery::Reconcile(NativeRequest::Clock(r))) => {
                assert_eq!(r.request_id, 1);
                assert_eq!(r.generation, GENERATION);
                if phase == "dispatched" {
                    assert!(matches!(
                        reopened.dispatch(&request_id()),
                        Err(StoreError::AlreadyDispatched)
                    ));
                }
            }
            (
                "completed",
                Recovery::Completed(Completion::Clock {
                    last_id: 1,
                    now_ns: 1000,
                    ..
                }),
            ) => {}
            other => panic!("wrong recovery after death: {other:?}"),
        }
    }
}
