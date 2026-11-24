use super::*;
use std::os::unix::fs::PermissionsExt;
pub(crate) struct Fixture {
    pub(crate) root: tempfile::TempDir,
    pub(crate) config: SimulatorLaunch,
}
impl Fixture {
    pub(crate) fn new(mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mode"), mode).unwrap();
        let script = root.path().join("xcrun");
        std::fs::write(&script,r#"#!/usr/bin/env python3
import json,sys,pathlib,time
p=pathlib.Path(__file__).parent
args=sys.argv[1:]
with (p/'calls').open('a') as f:f.write(json.dumps(args)+'\n')
mode=(p/'mode').read_text()
state=p/'devices.json'
devices=json.loads(state.read_text()) if state.exists() else []
if args[:3]==['simctl','list','devices']:
    if mode=='bad-retirement' and (p/'deleted').exists(): print('{');sys.exit(0)
    print(json.dumps({'devices':{'com.apple.CoreSimulator.SimRuntime.iOS-26-1':devices}}))
elif args[1]=='create':
    device={'udid':'ABCD0000-0000-0000-0000-000000000001','name':args[2],'deviceTypeIdentifier':args[3],'state':'Shutdown'}
    if mode=='foreign':device['name']='another-owner'
    devices.append(device);state.write_text(json.dumps(devices))
    if mode!='lost-create':print(device['udid'])
elif args[1]=='boot':
    if mode=='slow-boot':
        (p/'boot-started').write_text('yes');time.sleep(30)
    if mode=='boot-failure':sys.exit(7)
    devices[0]['state']='Booted';state.write_text(json.dumps(devices))
elif args[1]=='get_app_container':
    delay=p/'discovery-delay'
    if delay.exists():
        delay.unlink();time.sleep(6)
    pointer=p/'container-path'
    if not pointer.exists():sys.exit(1)
    print(pointer.read_text())
elif args[1]=='bootstatus':pass
elif args[1]=='listapps':print((p/'apps.plist').read_text())
elif args[1]=='openurl':
    saved=json.loads((p/'session/target/requests/requests.json').read_text())
    if 'event_count' in saved:saved=json.loads((p/'session/target/requests/events'/(str(saved['event_count']-1)+'.json')).read_text())
    assert saved['invalidated_through_ui']==saved['next_ui']-1
    with (p/'url-effects').open('a') as out:out.write(json.dumps(args)+'\n')
    if (p/'lose-url').exists():sys.exit(17)
elif args[1]=='shutdown':
    if mode=='shutdown-failure':sys.exit(8)
    devices[0]['state']='Shutdown';state.write_text(json.dumps(devices))
elif args[1]=='delete':
    assert args[2]=='ABCD0000-0000-0000-0000-000000000001'
    state.write_text('[]');(p/'deleted').write_text('yes')
else:raise AssertionError(args)
"#).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SimulatorLaunch {
            data_volume_root: None,
            xcrun: script,
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-26-1".into(),
            device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-16".into(),
            source_sha256: "a".repeat(64),
            data_volume: None,
        };
        Self { root, config }
    }
    fn acquire(&self, name: &str) -> Result<Simulator, AcquisitionError> {
        Simulator::acquire_at(
            self.config.clone(),
            &self.root.path().join(name),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &self.root.path().join("host"),
        )
    }
    fn host(&self) -> HostSession {
        frontcap_core::state::load(&self.root.path().join("host/native-session.json"))
            .unwrap()
            .unwrap()
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.path().join("calls")).unwrap()
    }
}
#[test]
fn relocated_containers_require_the_original_data_link_and_canonical_descendant() {
    for external in [false, true] {
    let fixture = Fixture::new("normal");
    let mut owner = fixture.acquire("container-owner").unwrap();
    owner.config.data_volume = Some(crate::storage_volume::Config {
        python: "/python".into(), worker: "/worker".into(), size_mib: 8192, reserve_mib: 64,
    });
    if external { owner.config.data_volume_root = Some(fixture.root.path().canonicalize().unwrap().join("external-volume")); }
    owner.config.validate_data_volume().unwrap();
    save_atomic(&owner.directory.join("launch.json"), &owner.config).unwrap();
    let destination = owner.config.data_volume_directory(&owner.directory).join("worker/volume/mount/device-data");
    let container = destination.join("Containers/Data/Application/app");
    std::fs::create_dir_all(&container).unwrap();
    let device = fixture.root.path().join(owner.udid());
    std::fs::create_dir(&device).unwrap();
    let source = device.join("data");
    std::os::unix::fs::symlink(&destination, &source).unwrap();
    save_atomic(&owner.directory.join("data-relocation.json"), &serde_json::json!({
        "source": source, "destination": destination, "backup": device.join("original-data"),
        "original_device": 1, "original_inode": 2, "initial": {"files": {}, "directories": []}
    })).unwrap();
    assert_eq!(owner.canonical_container(&source.join("Containers/Data/Application/app")).unwrap(), container);
    assert_eq!(owner.canonical_container(&container).unwrap(), container);
    assert!(owner.canonical_container(&destination).is_err());
    let outside = device.join("unrelated"); std::fs::create_dir(&outside).unwrap();
    // A matching UDID component cannot admit a path outside the declared relocated root.
    assert!(owner.canonical_container(&outside).is_err());
    std::os::unix::fs::symlink(&outside, container.join("escape")).unwrap();
    assert!(owner.canonical_container(&container.join("escape")).is_err());
    std::fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(&outside, &source).unwrap();
    assert!(owner.canonical_container(&container).is_err());
    owner.config.data_volume = None;
    assert!(owner.stop().passed);
    }
}

#[test]
fn owned_simulator_boots_and_deletes_once_and_excludes_another_native_owner() {
    let f = Fixture::new("normal");
    let mut owner = f.acquire("first").unwrap();
    assert_eq!(owner.udid(), "ABCD0000-0000-0000-0000-000000000001");
    assert!(!f.host().retired);
    assert!(f.acquire("busy").is_err());
    assert!(!f.root.path().join("busy").exists());
    assert!(owner.stop().passed);
    let calls = f.calls();
    assert!(owner.stop().passed);
    assert_eq!(f.calls(), calls);
    assert!(f.host().retired);
    assert!(f.acquire("second").unwrap().stop().passed);
}
#[test]
fn partial_boot_failure_retires_created_device_and_preserves_original_error() {
    let f = Fixture::new("boot-failure");
    let error = match f.acquire("failed") {
        Err(e) => e,
        Ok(_) => panic!(),
    };
    assert!(error.retirement.unwrap().passed);
    assert!(f.host().retired);
    assert!(
        f.root
            .path()
            .join("failed/acquisition-failure.json")
            .exists()
    );
    assert!(f.calls().contains("delete"));
}
#[test]
fn lost_create_reply_or_foreign_inventory_never_grants_deletion_authority() {
    for mode in ["lost-create", "foreign"] {
        let f = Fixture::new(mode);
        let error = match f.acquire("failed") {
            Err(e) => e,
            Ok(_) => panic!(),
        };
        assert!(!error.retirement.unwrap().passed);
        assert!(!f.host().retired);
        assert!(!f.calls().contains("delete"));
        let before = f.calls();
        assert!(f.acquire("replacement").is_err());
        assert_eq!(f.calls(), before);
    }
}
#[test]
fn deletion_requires_absence_evidence_but_can_follow_failed_shutdown() {
    let f = Fixture::new("bad-retirement");
    let mut owner = f.acquire("first").unwrap();
    assert!(!owner.stop().passed);
    assert!(!f.host().retired);
    assert!(f.acquire("replacement").is_err());
    let f = Fixture::new("shutdown-failure");
    let mut owner = f.acquire("first").unwrap();
    assert!(owner.stop().passed);
    assert!(f.host().retired);
    assert!(f.root.path().join("first/shutdown-failure.json").exists());
}
#[test]
fn cancellation_before_acquisition_has_no_target_effects() {
    let f = Fixture::new("normal");
    assert!(
        Simulator::acquire_at(
            f.config.clone(),
            &f.root.path().join("cancelled"),
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(true),
            &f.root.path().join("host")
        )
        .is_err()
    );
    assert!(!f.root.path().join("calls").exists());
}

#[test]
fn cancelled_boot_retires_created_simulator_with_separate_cleanup_budget() {
    let f = Fixture::new("slow-boot");
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !f.root.path().join("boot-started").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::Release);
        });
        let error = match Simulator::acquire_at(
            f.config.clone(),
            &f.root.path().join("cancelled"),
            Instant::now() + Duration::from_secs(8),
            &cancelled,
            &f.root.path().join("host"),
        ) {
            Err(error) => error,
            Ok(_) => panic!("cancelled acquisition succeeded"),
        };
        assert!(error.retirement.unwrap().passed);
        assert!(f.host().retired);
        assert!(f.calls().contains("delete"));
    });
}
#[test]
fn owner_entry() {
    let Some(root) = std::env::var_os("FRONTCAP_SIMULATOR_OWNER_TEST") else {
        return;
    };
    let root = PathBuf::from(root);
    let config: SimulatorLaunch = frontcap_core::state::load(&root.join("config.json"))
        .unwrap()
        .unwrap();
    let _owner = Simulator::acquire_at(
        config,
        &root.join("owned"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &root.join("host"),
    )
    .unwrap();
    std::fs::write(root.join("owner-ready"), b"ready").unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}
#[test]
fn owner_death_keeps_host_fenced_without_adopting_or_deleting_saved_device() {
    let f = Fixture::new("normal");
    save_atomic(&f.root.path().join("config.json"), &f.config).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "simulator::tests::owner_entry"])
        .env("FRONTCAP_SIMULATOR_OWNER_TEST", f.root.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.root.path().join("owner-ready").exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("owner did not become ready");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let calls = f.calls();
    assert!(!f.host().retired);
    assert!(f.acquire("replacement").is_err());
    assert_eq!(f.calls(), calls);
    assert!(!calls.contains("delete"));
}

#[test]
fn allowance_retirement_entry() {
    use frontcap_core::write_budget::{Class, WriteBudget};
    if std::env::var_os("FRONTCAP_SIMULATOR_BUDGET_TEST").is_none() { return; }
    let budget = WriteBudget::from_environment().unwrap().unwrap();
    let f = Fixture::new("normal");
    let mut simulator = f.acquire("simulator").unwrap();
    budget.charge(Class::Ordinary, (1 << 20) - budget.charged().unwrap().ordinary).unwrap();
    let result = simulator.stop();
    assert!(result.passed, "{result:?}");
    assert!(f.root.path().join("deleted").exists());
    assert!(f.host().retired);
    let retired = budget.charged().unwrap().retirement;
    assert!(retired > 0);
    assert!(simulator.stop().passed);
    assert_eq!(budget.charged().unwrap().retirement, retired);
    // The synchronous cleanup scope must not leak into later ordinary work.
    let mut command = Command::new("/bin/echo");command.arg("ordinary");
    assert!(capture_recorded(&mut command, Instant::now()+Duration::from_secs(1),
        &AtomicBool::new(false), 1024, &f.root.path().join("after-stop")).is_err());
    assert_eq!(budget.charged().unwrap().ordinary, 1 << 20);
    assert_eq!(budget.charged().unwrap().retirement, retired);
}

#[test]
fn sdk_cleanup_uses_retirement_after_ordinary_allowance_exhaustion() {
    use frontcap_core::write_budget::{Limits, WriteBudget};
    let root = tempfile::tempdir().unwrap();
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-sdk-cleanup").unwrap(),
        Limits { ordinary: 1 << 20, retirement: 65536 }).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "simulator::tests::allowance_retirement_entry", "--nocapture"])
        .env("FRONTCAP_SIMULATOR_BUDGET_TEST", "1");
    WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
    assert!(command.status().unwrap().success());
    assert_eq!(budget.charged().unwrap().ordinary, 1 << 20);
    assert!(budget.charged().unwrap().retirement > 0);
}

#[test]
fn data_volume_refuses_unsupervised_acquisition_before_creating_a_device() {
    let mut fixture = Fixture::new("normal");
    fixture.config.data_volume = Some(crate::storage_volume::Config {
        python: "/python".into(), worker: "/volume-worker".into(), size_mib: 8192, reserve_mib: 64,
    });
    let error = fixture.acquire("unsupported").err().unwrap();
    assert!(error.cause.contains("independent resource group"));
    assert!(!fixture.root.path().join("calls").exists());
    assert!(!fixture.root.path().join("unsupported").exists());
}

#[test]
fn explicit_data_volume_location_requires_a_canonical_parent_and_volume() {
    let fixture = Fixture::new("normal");
    let mut config = fixture.config.clone();
    let root = fixture.root.path().canonicalize().unwrap();
    config.data_volume_root = Some(root.join("fresh"));
    assert!(config.validate_data_volume().is_err());
    config.data_volume = Some(crate::storage_volume::Config {
        python: "/python".into(), worker: "/worker".into(), size_mib: 8192, reserve_mib: 64,
    });
    config.validate_data_volume().unwrap();
    std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
    config.data_volume_root = Some(root.join("alias/fresh"));
    assert!(config.validate_data_volume().is_err());
    config.data_volume_root = Some("relative".into());
    assert!(config.validate_data_volume().is_err());
}
