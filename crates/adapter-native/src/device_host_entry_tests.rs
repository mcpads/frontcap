use super::*;
use crate::device::tests::Fixture;

#[test]
fn host_url_acceptance_never_replays_or_adopts_a_replaced_process() {
    for failure in ["none", "lost", "replaced"] {
        let mut f = Fixture::new();
        f.config.host_app = Some(crate::android_webview::Profile { package: "org.example.app".into(),
            activity: "org.example.app.ClientActivity".into(), local_service: None });
        let source = std::fs::read_to_string(&f.config.adb).unwrap();
        let marker = "if args == ['shell', 'pidof', 'org.example.app']:";
        let insert = r#"
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:sys.exit(0)
if args==['shell','pidof','org.example.app']:print('123');sys.exit(0)
if args==['shell','cat','/proc/123/stat']:
 print('123 (owned) S '+' '.join(['0']*18)+' '+('999' if (root/'replaced').exists() else '42'));sys.exit(0)
if args[:6]==['shell','am','start','-W','-a','android.intent.action.VIEW']:
 import shlex
 assert shlex.split(' '.join(args[6:]))==['-d',"example://item/1?value='a'&second=2",'-p','org.example.app']
 with (root/'entry-effects').open('a') as log:log.write('dispatch\n')
 failure=(root/'failure').read_text()
 if failure=='lost':sys.exit(9)
 if failure=='replaced':(root/'replaced').touch()
 print('Status: ok\nActivity: org.example.app/.ClientActivity');sys.exit(0)
"#;
        assert!(source.contains(marker));
        std::fs::write(&f.config.adb, source.replacen(marker, &(insert.to_owned()+marker), 1)).unwrap();
        std::fs::write(f.root().join("failure"), failure).unwrap();
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now()+Duration::from_secs(25);
        let root = f.root().join("session");
        let mut device = Device::acquire_at(f.config.clone(), &root, deadline, &cancel, &f.root().join("host")).unwrap();
        let owner = device.app_process().unwrap().clone();
        let id = RequestId::new("entry").unwrap();
        let url = "example://item/1?value='a'&second=2";
        let mut stale = owner.clone(); stale.process_start_ticks += 1;
        assert!(device.dispatch_host_url(&id, &stale, url, deadline, &cancel).is_err());
        assert!(device.dispatch_host_url(&id, &owner, "relative/path", deadline, &cancel).is_err());
        assert!(!f.root().join("entry-effects").exists());
        let first = device.dispatch_host_url(&id, &owner, url, deadline, &cancel);
        if failure == "none" {
            let accepted = first.unwrap();
            assert_eq!(accepted.destination, Observation::Unobserved);
            assert_eq!(device.dispatch_host_url(&id, &owner, url, deadline, &cancel).unwrap(), accepted);
            assert!(device.dispatch_host_url(&id, &owner, "example://item/2", deadline, &cancel).is_err());
            assert!(device.stop().passed);
            let before = std::fs::read(f.root().join("adb-calls.jsonl")).unwrap();
            assert_eq!(read_host_url(&root, &root, &id, &owner, url).unwrap(), Some(accepted.clone()));
            assert_eq!(device.dispatch_host_url(&id, &owner, url, deadline, &cancel).unwrap(), accepted);
            assert_eq!(std::fs::read(f.root().join("adb-calls.jsonl")).unwrap(), before);
            let receipt = root.join("host-urls/entry/receipt.json");
            let proof: Proof = load(&receipt).unwrap().unwrap();
            let command = root.join(proof.commands.keys().next().unwrap());
            std::fs::write(command, b"changed").unwrap();
            assert!(read_host_url(&root, &root, &id, &owner, url).is_err());
        } else {
            assert!(first.unwrap_err().retirement.unwrap().passed);
            assert!(device.app_process().is_none());
            for name in ["entry", "retry"] {
                assert!(device.dispatch_host_url(&RequestId::new(name).unwrap(), &owner, url, deadline, &cancel).is_err());
            }
            assert!(read_host_url(&root, &root, &id, &owner, url).unwrap().is_none());
        }
        assert_eq!(std::fs::read_to_string(f.root().join("entry-effects")).unwrap(), "dispatch\n");
        assert!(device.stop().passed);
    }
}
