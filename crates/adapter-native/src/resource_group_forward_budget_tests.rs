use super::*;
use frontcap_core::{ids::LaunchId, write_budget::{Class, Limits, WriteBudget}};

#[test]
fn exhausted_forward_registration_preserves_owned_cleanup_proofs() {
    const CHILD: &str = "FRONTCAP_FORWARD_BUDGET_CHILD";
    const ORDINARY: u64 = 1024 * 1024;
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::generate(),
            Limits { ordinary: ORDINARY, retirement: 1024 * 1024 }).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "resource_group::tests::forward_budget::exhausted_forward_registration_preserves_owned_cleanup_proofs", "--nocapture"])
            .env(CHILD, "1");
        WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
        assert!(command.status().unwrap().success());
        return;
    }
    // Bind the recipe to its canonical target before the owner starts.
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let config = forwarding_config(root.as_path());
    let adb = fake_adb(root.as_path());
    let mut group = start(root.as_path(), &config);
    let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    let next_reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let next_port = next_reservation.local_addr().unwrap().port();
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "owned"), Reply::ForwardRegistered));
    drop(reservation);
    drop(next_reservation);
    assert!(capture_forward(&mut group, &forward_command(&adb, &config, port, true), "bind-owned").failure.is_none());
    let exchange = group.ready.target.join("bind-owned/exchange.json");
    let original_exchange = std::fs::read(&exchange).unwrap();
    let original_mapping = std::fs::read(root.as_path().join("mapping")).unwrap();
    let budget = WriteBudget::from_environment().unwrap().unwrap();
    budget.charge(Class::Ordinary, ORDINARY - budget.charged().unwrap().ordinary).unwrap();
    let before = budget.charged().unwrap();
    let rejected = root.as_path().join("group/forwarding/rejected");
    // Exercise this writer directly: the enclosing group's command admission is already covered.
    let registration = forwarding::Forward::new(ForwardRegistration { adb: adb.clone(), server_port: None, write_root: None, emulator: process.clone(),
        serial: format!("emulator-{}", config.ports[0]), socket: "webview_devtools_remote_456".into(),
        port: next_port, bind_evidence: "bind-rejected".into() }, &group.ready.target, &rejected);
    let mapping_unchanged = std::fs::read(root.as_path().join("mapping")).unwrap() == original_mapping;
    let result = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(result.passed, "{result:?}");
    assert!(process.has_exited().unwrap());
    assert!(result.forwarding["owned"].binding_confirmed && result.forwarding["owned"].removal_confirmed);
    assert!(matches!(result.forwarding["owned"].disposition, ForwardDisposition::Absent));
    assert_eq!(std::fs::read_to_string(root.as_path().join("removals")).unwrap(), "removed\n");
    assert!(!root.as_path().join("mapping").exists());
    assert_eq!(std::fs::read(exchange).unwrap(), original_exchange);
    assert!(root.as_path().join("group/forwarding/owned/cleanup-remove-intent.json").exists());
    assert!(root.as_path().join("group/forwarding/owned/retirement.json").exists());
    assert!(matches!(inspection::inspect(&root.as_path().join("group"), &config.target, &config.ports, None).unwrap(),
        inspection::Inspection::Finished(proof) if proof.passed));
    assert_eq!(budget.charged().unwrap().ordinary, before.ordinary);
    assert!(budget.charged().unwrap().retirement > before.retirement);
    assert!(mapping_unchanged);
    match registration {
        Err(error) => assert!(error.to_string().contains("allowance"), "{error}"),
        Ok(_) => panic!("exhausted forwarding metadata published a new bind registration"),
    }
    assert!(!rejected.join("intent.json").exists());
    assert!(!root.as_path().join("target/bind-rejected").exists());
}
