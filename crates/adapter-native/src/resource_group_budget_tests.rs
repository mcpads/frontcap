use super::*;
use frontcap_core::{ids::LaunchId, write_budget::{Class, Limits, WriteBudget}};

#[test]
fn exhausted_metadata_admission_preserves_retirement_without_dispatch() {
    const CHILD: &str = "FRONTCAP_GROUP_BUDGET_CHILD";
    const ORDINARY: u64 = 1024 * 1024;
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::generate(),
            Limits { ordinary: ORDINARY, retirement: 1024 * 1024 }).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "resource_group::budget_tests::exhausted_metadata_admission_preserves_retirement_without_dispatch", "--nocapture"])
            .env(CHILD, "1");
        WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
        assert!(command.status().unwrap().success());
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let arguments = |entry: &str| -> Vec<String> { vec!["-c".into(),
        format!("export FRONTCAP_RESOURCE_GROUP_DIRECTORY=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
        "resource-group-budget-test".into(), std::env::current_exe().unwrap().to_str().unwrap().into()] };
    let config = Config { host: root.join("host"), target: root.join("target"), evidence_root: None,
        artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
        resource_executable: "/bin/sh".into(), resource_arguments: arguments("resource_entry"),
        ports: vec![], deadline_ns: monotonic_ns().unwrap() + 20_000_000_000 };
    let mut command = Command::new("/bin/sh");
    command.args(arguments("group_entry")).env("FRONTCAP_RESOURCE_GROUP_TEST", &root);
    let directory = root.join("group");
    let mut group = Group::start(&mut command, &config, &directory, Instant::now() + Duration::from_secs(5)).unwrap();
    let budget = WriteBudget::from_environment().unwrap().unwrap();
    budget.charge(Class::Ordinary, ORDINARY - budget.charged().unwrap().ordinary).unwrap();
    let before = budget.charged().unwrap();
    let reply = group.execute(Operation::Start { id: RequestId::new("never-started").unwrap(), launch: Launch {
        program: "/bin/sleep".into(), arguments: vec!["300".into()], environment: BTreeMap::new(),
        remove_environment: vec![], directory: config.target.clone(), deadline_ns: config.deadline_ns } },
        Instant::now() + Duration::from_secs(5));
    let retirement = group.stop(Instant::now() + Duration::from_secs(12)).unwrap();
    let error = reply.expect_err("exhausted metadata must reject before command publication");
    assert!(error.to_string().contains("allowance"), "{error}");
    assert!(!directory.join("commands/1.json").exists());
    assert!(!directory.join("resources/never-started").exists());
    assert!(retirement.passed, "{retirement:?}");
    assert!(retirement.resources.is_empty());
    assert_eq!(budget.charged().unwrap().ordinary, before.ordinary);
    assert!(budget.charged().unwrap().retirement > before.retirement);
    assert!(load::<HostSession>(&config.host.join("native-session.json")).unwrap().unwrap().retired);
    assert!(matches!(inspection::inspect(&directory, &config.target, &[], None).unwrap(),
        inspection::Inspection::Finished(result) if result.passed));
}
