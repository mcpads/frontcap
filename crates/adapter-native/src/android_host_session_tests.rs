use super::*;
use frontcap_core::{ids::LaunchId, native::android_webview::AppProcess, write_budget::Limits};

#[test]
fn host_dispatch_requires_admitted_intent_and_keeps_uncertainty_after_result_refusal() {
    for refuse_intent in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for name in ["commands", "requests"] { std::fs::create_dir(root.join(name)).unwrap(); }
        let limit = 1024 * 1024;
        let budget = WriteBudget::create(&root.join("budget"), LaunchId::generate(),
            Limits { ordinary: limit, retirement: 1024 }).unwrap();
        let (send, receive) = mpsc::channel();
        let cancelled = AtomicBool::new(false);
        let stopping = AtomicBool::new(false);
        let owner = AppProcess { emulator: ProcessIdentity::current(), package: "org.example.app".into(),
            activity: "org.example.app.MainActivity".into(), serial: "emulator-5580".into(),
            pid: 123, process_start_ticks: 42, apk_sha256: "a".repeat(64), source_sha256: "b".repeat(64) };
        let mut driver = Driver { budget: Some(budget.clone()), directory: root, commands: &receive,
            cancelled: &cancelled, stopping: &stopping, end: Instant::now() + Duration::from_secs(10),
            expected: 1, ready: host::Ready { owner, contacts: None }, epoch: None, depth: 0,
            released_connections: Vec::new() };
        let id = RequestId::new("observe").unwrap();
        send.send(Message::Execute { number: 1, id: id.clone(),
            operation: Operation::Host { request: host::Operation::Observe { input_basis: false } },
            deadline_ns: monotonic_ns().unwrap() + 10_000_000_000 }).unwrap();
        if refuse_intent { budget.charge(Class::Ordinary, limit).unwrap(); }
        let next = driver.next();
        if refuse_intent {
            assert!(next.is_err());
            assert!(!root.join("requests/observe").exists());
            assert!(!root.join("commands/1/command.json").exists());
        } else {
            let dispatch = next.unwrap().unwrap();
            let intent = std::fs::read(dispatch.request.join("operation.json")).unwrap();
            assert!(budget.charged().unwrap().ordinary > 0);
            budget.charge(Class::Ordinary, limit - budget.charged().unwrap().ordinary).unwrap();
            assert!(driver.finish(&dispatch, Err(io::Error::other("original effect failed"))).is_err());
            assert_eq!(std::fs::read(dispatch.request.join("operation.json")).unwrap(), intent);
            assert!(!dispatch.request.join("result.json").exists());
            assert!(!dispatch.attempt.join("result.json").exists());
        }
        assert_eq!(budget.charged().unwrap().retirement, 0);
    }
}
