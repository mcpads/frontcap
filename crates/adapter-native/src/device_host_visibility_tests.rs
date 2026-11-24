use super::*;
use crate::device::tests::Fixture;

#[test]
fn host_visibility_preserves_process_and_original_effect_without_a_consumer_clock() {
    for failure in ["none","lost","replaced"] {
        let mut f=Fixture::new();
        f.config.host_app=Some(crate::android_webview::Profile {package:"org.example.app".into(),activity:"org.example.app.ClientActivity".into(),local_service:None});
        let source=std::fs::read_to_string(&f.config.adb).unwrap();
        let marker="if args == ['shell', 'pidof', 'org.example.app']:";
        let insert=r#"
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:sys.exit(0)
if args==['shell','pidof','org.example.app']:print('123');sys.exit(0)
if args==['shell','cat','/proc/sys/kernel/random/boot_id']:print('abcd0000-0000-0000-0000-000000000001');sys.exit(0)
if args in [['shell','cat','/proc/123/stat'],['shell','run-as','org.example.app','cat','/proc/123/stat']]:
 print('123 (owned) S '+' '.join(['0']*18)+' '+('999' if (root/'replaced').exists() else '42'));sys.exit(0)
if args==['shell','run-as','org.example.app','cat','/proc/123/cmdline']:sys.stdout.write('org.example.app\0');sys.exit(0)
if args==['shell','dumpsys','window']:
 component='com.android.launcher/.Launcher' if (root/'background').exists() else 'org.example.app/.ClientActivity'
 print('mCurrentFocus=Window{abc u0 '+component+'}');sys.exit(0)
if args==['shell','input','keyevent','KEYCODE_HOME'] or args==['shell','am','start','-W','--activity-reorder-to-front','-n','org.example.app/org.example.app.ClientActivity']:
 background=args[1]=='input'
 with (root/'visibility-effects').open('a') as log:log.write(('home' if background else 'activate')+'\n')
 if background:(root/'background').touch()
 else:(root/'background').unlink()
 failure=(root/'failure').read_text()
 if failure=='lost':sys.exit(9)
 if failure=='replaced':(root/'replaced').touch()
 sys.exit(0)
"#;
        assert!(source.contains(marker));
        std::fs::write(&f.config.adb,source.replacen(marker,&(insert.to_owned()+marker),1)).unwrap();
        std::fs::write(f.root().join("failure"),failure).unwrap();
        let cancel=AtomicBool::new(false);let deadline=Instant::now()+Duration::from_secs(30);
        let root=f.root().join("session");
        let mut device=Device::acquire_at(f.config.clone(),&root,deadline,&cancel,&f.root().join("host")).unwrap();
        let owner=device.app_process().unwrap().clone();let home=RequestId::new("home").unwrap();
        let mut stale=owner.clone();stale.process_start_ticks+=1;
        assert!(device.set_host_state(&home,&stale,State::Background,deadline,&cancel).is_err());
        assert!(device.set_host_state(&home,&owner,State::Foreground,deadline,&cancel).is_err());
        assert!(!f.root().join("visibility-effects").exists());
        let result=device.set_host_state(&home,&owner,State::Background,deadline,&cancel);
        if failure=="none" {
            let background=result.unwrap();background.validate().unwrap();
            assert!(device.require_host_foreground().is_err());
            assert_eq!(device.set_host_state(&home,&owner,State::Background,deadline,&cancel).unwrap(),background);
            assert!(device.set_host_state(&RequestId::new("duplicate-home").unwrap(),&owner,State::Background,deadline,&cancel).is_err());
            let foreground=device.set_host_state(&RequestId::new("return").unwrap(),&owner,State::Foreground,deadline,&cancel).unwrap();
            assert_eq!(background.process_after,foreground.process_before);
            device.require_host_foreground().unwrap();
            assert!(device.stop().passed);
            let calls=std::fs::read(f.root().join("adb-calls.jsonl")).unwrap();
            assert_eq!(read_host_transition(&root,&root,&home,&owner,State::Background).unwrap(),Some(background));
            assert_eq!(std::fs::read(f.root().join("adb-calls.jsonl")).unwrap(),calls);
            assert!(read_host_transition(&root,&root,&home,&owner,State::Foreground).is_err());
            let receipt=root.join("host-visibility/home/receipt.json");
            let proof:Proof=load(&receipt).unwrap().unwrap();
            let path=root.join(format!("command-{}/exchange.json",proof.samples.dispatch));
            std::fs::write(path,b"changed").unwrap();
            assert!(read_host_transition(&root,&root,&home,&owner,State::Background).is_err());
            assert_eq!(std::fs::read_to_string(f.root().join("visibility-effects")).unwrap(),"home\nactivate\n");
        } else {
            assert!(result.unwrap_err().retirement.unwrap().passed);
            assert!(device.app_process().is_none());
            for name in ["home","retry"] {assert!(device.set_host_state(&RequestId::new(name).unwrap(),&owner,State::Background,deadline,&cancel).is_err());}
            assert!(read_host_transition(&root,&root,&home,&owner,State::Background).unwrap().is_none());
            assert_eq!(std::fs::read_to_string(f.root().join("visibility-effects")).unwrap(),"home\n");
        }
        assert!(device.stop().passed);
    }
}
