use super::*;
use crate::device::tests::Fixture;
use frontcap_core::native::contacts::{Contact, Operation};

fn fixture() -> Fixture {
    let mut f = Fixture::new();
    f.config.host_app = Some(crate::android_webview::Profile { package: "org.example.app".into(),
        activity: "org.example.app.ClientActivity".into(), local_service: None });
    let emulator = std::fs::read_to_string(&f.config.emulator).unwrap();
    std::fs::write(&f.config.emulator, emulator.replace("time.monotonic()+30", "time.monotonic()+240")).unwrap();
    let source = std::fs::read_to_string(&f.config.adb).unwrap();
    let point = "if args == ['shell', 'pidof', 'org.example.app']:";
    let insert = r#"
epoch=int((root/'epoch').read_text()) if (root/'epoch').exists() else 0
pid=123+epoch;driver=456+epoch;stopped=(root/'stopped').exists()
def stat(p,t):return str(p)+' (owned) S '+' '.join(['0']*18)+' '+str(t)
cold=args[:6]==['shell','am','start','-W','-a','android.intent.action.VIEW']
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity'] or cold:
 if cold:
  import shlex
  assert stopped, 'URL launched before original process absence'
  assert shlex.split(' '.join(args[6:]))==['-d',"example://item/1?literal='value'&second=2",'-p','org.example.app']
  with (root/'url-effects').open('a') as out:out.write('url\n')
  if (root/'missing-handler').exists():print('Error: Activity not found');sys.exit(0)
 if stopped:
  assert (root/'input-dead').exists(), 'replacement started with old input alive'
  (root/'epoch').write_text(str(epoch+1));(root/'stopped').unlink();(root/'input-dead').unlink()
  with (root/'effects').open('a') as out:out.write('start\n')
  if (root/'lose-start').exists():sys.exit(9)
 if cold:
  if (root/'reused-url').exists():print('Warning: Activity not started, intent delivered to running activity')
  print('Status: timeout\nActivity: org.example.app/.ClientActivity')
 sys.exit(0)
if args==['shell','pidof','org.example.app']:
 if stopped:
  if (root/'deny-absence').exists():print('permission denied',file=sys.stderr);sys.exit(13)
  sys.exit(1)
 print(pid);sys.exit(0)
if args==['shell','cat',f'/proc/{pid}/stat'] or args==['shell','run-as','org.example.app','cat',f'/proc/{pid}/stat']:
 print(stat(pid,12345+epoch));sys.exit(0)
if args==['shell','run-as','org.example.app','cat',f'/proc/{pid}/cmdline']:
 sys.stdout.buffer.write(b'org.example.app\0');sys.exit(0)
if args==['shell','cat','/proc/sys/kernel/random/boot_id']:
 print('abcd0000-0000-0000-0000-000000000001');sys.exit(0)
if args==['shell','pm','path','org.example.app']:
 print('package:/data/app/owned/base.apk');sys.exit(0)
if args==['shell','sha256sum','/data/app/owned/base.apk']:
 print(APK_HASH+'  '+args[2]);sys.exit(0)
if args==['shell','run-as','org.example.app','pwd']:
 print('/data/user/0/org.example.app');sys.exit(0)
if args==['shell','run-as','org.example.app','stat','-c',"'%d %i'",'.']:
 print('2049 '+('9999' if stopped and (root/'changed-data').exists() else '5511'));sys.exit(0)
if args==['shell','am','force-stop','org.example.app']:
 (root/'stopped').touch()
 with (root/'effects').open('a') as out:out.write('stop\n')
 sys.exit(9 if (root/'lose-stop').exists() else 0)
if args[:6]==['shell','run-as','org.example.app','test','!','-d']:
 assert stopped;sys.exit(0)
if args==['shell','test','!','-d',f'/proc/{driver}']:
 sys.exit(0 if (root/'input-dead').exists() else 1)
if args==['shell','cat',f'/proc/{driver}/stat']:
 if stopped and (root/'exit-before-stat').exists():(root/'input-dead').touch();sys.exit(1)
 print(stat(driver,99999 if stopped and (root/'foreign-input').exists() else 54321+epoch));sys.exit(0)
if args==['shell','kill','-TERM',str(driver)]:
 assert stopped and not (root/'foreign-input').exists()
 (root/'input-dead').touch()
 if (root/'exit-before-signal').exists():sys.exit(1)
 with (root/'effects').open('a') as out:out.write('input-stop\n')
 sys.exit(0)
if args[:2]==['shell','mkdir']:sys.exit(0)
if args[:1]==['push'] and args[2].endswith('driver.jar'):
 assert Path(args[1]).read_bytes()==b'frozen-driver';sys.exit(0)
if args[:2]==['shell','sha256sum']:
 print(DRIVER_HASH+'  '+args[2]);sys.exit(0)
if args[:2]==['shell','-T']:time.sleep(240);sys.exit(0)
if args[:2]==['shell','cat'] and args[2].endswith('/journal/ready.json'):
 print(json.dumps(dict(status='ready',generation=f'abcd0000-0000-0000-0000-{epoch+1:012d}',pid=driver,uid=2000,request_capacity=24)));sys.exit(0)
if args==['shell','dumpsys','window']:
 print('mCurrentFocus=Window{abc u0 org.example.app/.ClientActivity}');sys.exit(0)
if args==['exec-out','screencap','-p']:
 sys.stdout.buffer.write(b'\x89PNG\r\n\x1a\n'+(13).to_bytes(4,'big')+b'IHDR'+(100).to_bytes(4,'big')+(80).to_bytes(4,'big'));sys.exit(0)
if args[:1]==['push'] and args[2].endswith('.pending'):
 (root/'input-request').write_bytes(Path(args[1]).read_bytes());sys.exit(0)
if args[:2]==['shell','mv']:
 request=json.loads((root/'input-request').read_text())
 assert request['contacts'], 'restart must not inject releases'
 (root/'input-reply').write_text(json.dumps(dict(generation=request['generation'],request_id=request['request_id'],status='injected',contact_revision=request['expected_revision']+1,contacts=request['contacts'],frames=[dict(accepted=True)])))
 sys.exit(9 if (root/'lose-input').exists() else 0)
if args[:2]==['shell','cat'] and '/accepted-' in args[2]:print((root/'input-request').read_text());sys.exit(0)
if args[:2]==['shell','cat'] and '/reply-' in args[2]:print((root/'input-reply').read_text());sys.exit(0)
"#.replace("APK_HASH", &serde_json::to_string(&f.config.apk_sha256).unwrap())
        .replace("DRIVER_HASH", &serde_json::to_string(&frontcap_core::canonical::sha256_bytes_hex(b"frozen-driver")).unwrap());
    std::fs::write(&f.config.adb, source.replace(point, &(insert + point))).unwrap();
    std::fs::write(f.root().join("input.jar"), b"frozen-driver").unwrap();
    f
}
fn acquire(f: &Fixture, cancel: &AtomicBool) -> Device {
    let mut device = Device::acquire_at(f.config.clone(), &f.root().join("session"),
        Instant::now()+Duration::from_secs(15), cancel, &f.root().join("host")).unwrap();
    device.start_contact_driver(&f.root().join("input.jar"),
        &frontcap_core::canonical::sha256_bytes_hex(b"frozen-driver"),
        Instant::now()+Duration::from_secs(10), cancel).unwrap();
    device
}
fn id(value: &str) -> RequestId { RequestId::new(value).unwrap() }

#[test]
fn host_restart_preserves_data_and_old_input_evidence_across_held_and_unknown_requests() {
    let f = fixture();
    let cancel = AtomicBool::new(false);
    let mut device = acquire(&f, &cancel);
    let deadline = Instant::now()+Duration::from_secs(90);
    let initial = std::fs::read(f.root().join("session/app-process.json")).unwrap();
    for name in ["held", "unknown"] {
        let url = (name == "unknown").then_some("example://item/1?literal='value'&second=2");
        let old = device.app_process().unwrap().clone();
        let old_directory = device.host_evidence_directory().to_owned();
        let old_driver = device.inspect_contact_progress().unwrap().owner;
        let observation = id(&format!("observe-{name}"));
        device.capture_contacts(&observation, deadline, &cancel).unwrap();
        let operation = Operation::Set { expected_revision: 0, contacts: vec![Contact { id: 1, x: 0.5, y: 0.5 }] };
        if name == "unknown" { std::fs::write(f.root().join("lose-input"), b"").unwrap(); }
        let input = device.input_contacts(&id(name), &observation, &operation, deadline, &cancel);
        assert_eq!(input.is_ok(), name == "held");
        let original = std::fs::read(old_directory.join("contact-driver/state.json")).unwrap();
        let mut stale = old.clone(); stale.process_start_ticks += 1;
        assert!(device.restart_host(&id("stale"), &stale, None, deadline, &cancel).is_err());
        assert!(!f.root().join("session/host-restarts/stale").exists());
        let count = device.command_number;
        assert!(device.restart_host(&id("invalid-url"), &old, Some("relative/path"), deadline, &cancel).is_err());
        assert_eq!(device.command_number, count);
        let replacement = device.restart_host(&id(&format!("restart-{name}")), &old, url, deadline, &cancel).unwrap();
        assert_eq!(replacement.installation.data_inode, 5511);
        assert_eq!(replacement.entry.as_ref().map(|e| e.url.as_str()), url);
        if let Some(entry) = &replacement.entry {
            assert_eq!(entry.owner, old);
            assert_eq!(entry.destination, frontcap_core::native::external_entry::Observation::Unobserved);
            assert!(read_host_replacement(&f.root().join("session"), &id(&format!("restart-{name}")), &old, None).is_err());
        }
        assert_ne!(replacement.ready.owner, old);
        assert_eq!(device.app_process(), Some(&replacement.ready.owner));
        assert_eq!(std::fs::read(old_directory.join("contact-driver/state.json")).unwrap(), original);
        assert_eq!(Device::read_contact_result(&old_directory, &old_driver, &id(name), &observation, &operation).unwrap(), input.ok());
        assert!(device.input_contacts(&id("old-basis"), &observation, &operation, deadline, &cancel).is_err());
        assert!(device.verify_app_process(&old, deadline, &cancel).is_err());
        let count = device.command_number;
        assert_eq!(device.restart_host(&id(&format!("restart-{name}")), &old, url, deadline, &cancel).unwrap(), replacement);
        assert_eq!(device.command_number, count, "historical read must not contact ADB");
        device.capture_contacts(&id(&format!("fresh-{name}")), deadline, &cancel).unwrap();
    }
    assert_eq!(std::fs::read(f.root().join("session/app-process.json")).unwrap(), initial);
    assert_eq!(std::fs::read_to_string(f.root().join("effects")).unwrap(), "stop\ninput-stop\nstart\nstop\ninput-stop\nstart\n");
    assert_eq!(std::fs::read_to_string(f.root().join("url-effects")).unwrap(), "url\n");
    assert!(device.stop().passed);
    let old: AppProcess = serde_json::from_slice(&initial).unwrap();
    let before = device.command_number;
    assert!(read_host_replacement(&f.root().join("session"), &id("restart-held"), &old, None).unwrap().is_some());
    assert_eq!(device.command_number, before);
    let receipt = f.root().join("session/host-restarts/restart-held/receipt.json");
    let proof: Proof = load(&receipt).unwrap().unwrap();
    let command = f.root().join("session").join(proof.commands.keys().next().unwrap());
    std::fs::write(command, b"changed").unwrap();
    assert!(read_host_replacement(&f.root().join("session"), &id("restart-held"), &old, None).is_err());
}

#[test]
fn host_restart_retires_on_uncertain_stop_start_or_input_owner_and_never_replays() {
    let cold = Some("example://item/1?literal='value'&second=2");
    for (failure,url) in [("lose-stop",None),("deny-absence",None),("foreign-input",None),
        ("changed-data",None),("lose-start",None),("lose-start",cold),("missing-handler",cold),("reused-url",cold)] {
        let f = fixture(); let cancel = AtomicBool::new(false);
        let mut device = acquire(&f, &cancel);
        let owner = device.app_process().unwrap().clone();
        std::fs::write(f.root().join(failure), b"").unwrap();
        let deadline = Instant::now()+Duration::from_secs(30);
        let error = device.restart_host(&id("restart"), &owner, url, deadline, &cancel).unwrap_err();
        assert!(error.retirement.unwrap().passed, "{failure}: {}", error.cause);
        assert!(device.app_process().is_none());
        let effects = std::fs::read(f.root().join("effects")).unwrap();
        for name in ["restart", "different-id"] {
            assert!(device.restart_host(&id(name), &owner, url, deadline, &cancel).is_err());
        }
        assert_eq!(std::fs::read(f.root().join("effects")).unwrap(), effects);
        if url.is_some() {
            assert_eq!(std::fs::read_to_string(f.root().join("url-effects")).unwrap(), "url\n");
            assert!(device.restart_host(&id("fallback"), &owner, None, deadline, &cancel).is_err());
            assert_eq!(std::fs::read(f.root().join("effects")).unwrap(), effects);
        }
        if failure == "foreign-input" { assert_eq!(effects, b"stop\n"); }
        assert!(read_host_replacement(&f.root().join("session"), &id("restart"), &owner, url).unwrap().is_none());
    }
}

#[test]
fn host_restart_accepts_confirmed_input_exit_between_probe_and_stat_or_signal() {
    for boundary in ["exit-before-stat", "exit-before-signal"] {
        let f = fixture(); let cancel = AtomicBool::new(false);
        let mut device = acquire(&f, &cancel);
        let owner = device.app_process().unwrap().clone();
        std::fs::write(f.root().join(boundary), b"").unwrap();
        let replacement = device.restart_host(&id("restart"), &owner, None,
            Instant::now()+Duration::from_secs(30), &cancel).unwrap();
        assert_ne!(replacement.ready.owner, owner);
        assert_eq!(std::fs::read_to_string(f.root().join("effects")).unwrap(), "stop\nstart\n");
        assert!(device.stop().passed);
    }
}

#[path = "device_supervision_tests.rs"]
mod supervised_tests;
