use super::*;
use frontcap_core::native::external_entry::Observation;
use std::cell::Cell;

fn request(url: &str) -> Request {
    Request { target: Target::Android { serial: "emulator-5580".into(), package: "org.example.client".into() },
        generation: "aaaaaaaa-bbbb-cccc-dddd-000000000001".into(), url: url.into() }
}
fn id(s: &str) -> RequestId { RequestId::new(s).unwrap() }
fn store(root: &Path) -> Dispatches {
    let req = request("client-test://item/1");
    Dispatches::create(&root.join("entry"), req.target, req.generation).unwrap()
}

#[test]
fn accepted_url_recovery_never_dispatches_or_claims_destination() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = store(temp.path());
    let req = request("client-test://item/1?edit=true");
    let calls = Cell::new(0);
    let expected = store.dispatch(&id("link"), req.clone(), |_| { calls.set(calls.get()+1); Ok(()) }).unwrap();
    assert_eq!(expected.confirmation, Observation::Unobserved);
    assert_eq!(expected.destination, Observation::Unobserved);
    let replay = store.dispatch(&id("link"), req.clone(), |_| panic!("replayed URL")).unwrap();
    assert_eq!(replay, expected);
    assert_eq!(calls.get(), 1);
    assert!(store.dispatch(&id("link"), request("client-test://item/2"), |_| panic!("changed URL")).is_err());
    let path = store.directory().to_owned();
    drop(store);
    assert_eq!(read_accepted(&path, &id("link"), &req).unwrap(), Some(expected));
    let mut changed = req.clone(); changed.generation = "aaaaaaaa-bbbb-cccc-dddd-000000000002".into();
    assert!(read_accepted(&path, &id("link"), &changed).is_err());
    assert!(Dispatches::create(&path, req.target, req.generation).is_err());
}

#[test]
fn lost_dispatch_result_blocks_replay_and_new_request_ids() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = store(temp.path());
    let req = request("client-test://item/1");
    assert!(store.dispatch(&id("lost"), req.clone(), |_| Err(io::Error::other("lost OS reply"))).is_err());
    for name in ["lost", "try-again"] {
        assert!(store.dispatch(&id(name), req.clone(), |_| panic!("redispatched uncertain URL")).is_err());
    }
    let path = store.directory().to_owned();drop(store);
    assert_eq!(read_accepted(&path, &id("lost"), &req).unwrap(), None);
    assert_eq!(Journal::read_state(&path.join("requests.jsonl"), &id("lost")).unwrap(), Some(RequestState::Dispatched));
}

#[test]
fn stale_owner_or_invalid_transport_never_reaches_os() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = store(temp.path());
    for url in ["relative/path", "1bad:route", "client-test:", "client-test://item/1\n", "https://a b"] {
        assert!(store.dispatch(&id("invalid"), request(url), |_| panic!("invalid URL reached OS")).is_err());
    }
    let mut stale = request("client-test://item/1");stale.generation = "aaaaaaaa-bbbb-cccc-dddd-000000000002".into();
    assert!(store.dispatch(&id("stale"), stale, |_| panic!("stale owner reached OS")).is_err());
    let mut foreign = request("client-test://item/1");
    foreign.target = Target::Android { serial: "emulator-5582".into(), package: "org.example.client".into() };
    assert!(store.dispatch(&id("foreign"), foreign, |_| panic!("foreign device reached OS")).is_err());
    assert_eq!(store.journal.state(&id("invalid")), None);
    store.dispatch(&id("valid"), request("client-test://item/0"), |_| Ok(())).unwrap();
}

#[test]
fn android_arguments_preserve_literal_url_and_require_explicit_os_acceptance() {
    let url = "client-test://item/1?x='$(printf${IFS}injected)'&y=2#fragment";
    let args = android_arguments("org.example.client", url);
    // Exercise the device-shell quoting using a harmless argv printer, never eval the raw URL.
    let output = std::process::Command::new("/bin/sh").args(["-c", &format!("printf '%s' {}", args[7])]).output().unwrap();
    assert!(output.status.success());assert_eq!(String::from_utf8(output.stdout).unwrap(), url);
    assert_eq!(&args[8..], &["-p", "org.example.client"]);
    assert!(android_accepted("Status: ok\nActivity: org.example.client/.Main\n", "org.example.client", false).is_ok());
    assert!(android_accepted("Status: timeout\nActivity: org.example.client/.Main\n", "org.example.client", true).is_ok());
    for output in ["Warning: Activity not started\nStatus: ok\nActivity: org.example.client/.Main",
        "Error: Activity not started\nStatus: ok\nActivity: org.example.client/.Main",
        "Status: timeout\nActivity: dev.foreign.app/.Main"] {
        assert!(android_accepted(output,"org.example.client",true).is_err());
    }
    for output in ["Error: Activity not started", "Status: timeout\nActivity: org.example.client/.Main",
        "Status: ok\nActivity: dev.other/.Main", "Status: ok\nStatus: ok\nActivity: org.example.client/.Main"] {
        assert!(android_accepted(output, "org.example.client", false).is_err());
    }
}

#[test]
fn dispatch_admits_all_journal_bytes_before_os_and_cached_reads_spend_nothing() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    for target in [request("client:one").target, Target::Ios {
        simulator: "aaaaaaaa-bbbb-cccc-dddd-000000000002".into(), bundle: "org.example.client".into() }] {
        let root = tempfile::tempdir().unwrap();
        let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-entry").unwrap(),
            Limits { ordinary: 64 * 1024, retirement: 4096 }).unwrap();
        let mut req = request("client-test://item/1"); req.target = target;
        let mut store = Dispatches::create_with_budget(&root.path().join("entry"), req.target.clone(), req.generation.clone(), Some(&budget)).unwrap();
        let owner_bytes = std::fs::metadata(store.directory().join("owner.json")).unwrap().len();
        assert_eq!(budget.charged().unwrap().ordinary, owner_bytes);
        let key = id("link");
        let expected = owner_bytes + Journal::admission_bytes(&key, &sha256_hex(&canonical_of(&req)))
            + Journal::completion_bytes(&key, &serde_json::to_value(Accepted::new(req.clone())).unwrap());
        let accepted = store.dispatch(&key, req.clone(), |_| {
            assert_eq!(budget.charged().unwrap().ordinary, expected);
            // Another cooperating writer may exhaust ordinary space while the OS call runs.
            budget.charge(Class::Ordinary, 64 * 1024 - expected).unwrap();
            Ok(())
        }).unwrap();
        let journal = store.directory().join("requests.jsonl");
        assert_eq!(owner_bytes + journal.metadata().unwrap().len(), expected);
        assert_eq!(store.dispatch(&key, req.clone(), |_| panic!("cached URL redispatched")).unwrap(), accepted);
        let before = std::fs::read(&journal).unwrap();
        assert!(store.dispatch(&id("refused"), req.clone(), |_| panic!("unadmitted URL reached OS")).is_err());
        assert!(!store.contains(&id("refused")));
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        assert_eq!(budget.charged().unwrap().retirement, 0);
        let directory = store.directory().to_owned(); drop(store);
        assert_eq!(read_accepted(&directory, &key, &req).unwrap(), Some(accepted));
    }
}

#[test]
fn failed_os_delivery_keeps_precharged_bytes_and_original_pending_verdict() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap(); let req = request("client-test://item/1");
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-failure").unwrap(),
        Limits { ordinary: 64 * 1024, retirement: 4096 }).unwrap();
    let mut store = Dispatches::create_with_budget(&root.path().join("entry"), req.target.clone(), req.generation.clone(), Some(&budget)).unwrap();
    let key = id("lost");
    assert!(store.dispatch(&key, req.clone(), |_| Err(io::Error::other("lost reply"))).is_err());
    let charged = budget.charged().unwrap();
    assert!(charged.ordinary > std::fs::metadata(store.directory().join("owner.json")).unwrap().len()
        + std::fs::metadata(store.directory().join("requests.jsonl")).unwrap().len());
    for key in [key.clone(), id("new")] {
        assert!(store.dispatch(&key, req.clone(), |_| panic!("uncertain URL replayed")).is_err());
    }
    assert_eq!(budget.charged().unwrap(), charged);
    assert_eq!(read_accepted(store.directory(), &key, &req).unwrap(), None);
}
