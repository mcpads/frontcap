use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn emulator_disks_are_owned_through_acquisition_failure_and_retirement() {
    use std::os::unix::fs::symlink;
    for mode in ["stop", "install-failed", "replaced-directory"] {
        let f = Fixture::view(false);
        let shared = f.root().join("shared-avd");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("base.qcow2"), b"shared backing").unwrap();
        let emulator = std::fs::read_to_string(&f.config.emulator).unwrap();
        let setup = "\nimport os\nscratch=Path(os.environ['ANDROID_TMP'])\n(scratch/'overlay.qcow2').write_bytes(b'owned disk')\n";
        std::fs::write(&f.config.emulator, emulator.replace("sockets=[]", &format!("{setup}\nsockets=[]"))).unwrap();
        let inherited = std::env::var_os("ANDROID_TMP");
        if mode == "install-failed" { std::fs::write(f.root().join("install-fail"), b"").unwrap(); }
        let acquired = f.acquire(&AtomicBool::new(false));
        let scratch = f.root().join("session/emulator-temporary");
        if mode == "install-failed" {
            let Err(error) = acquired else { panic!("fixture install unexpectedly passed") };
            assert!(error.retirement.unwrap().passed);
        } else {
            let mut device = acquired.unwrap();
            assert_eq!(std::fs::read(scratch.join("overlay.qcow2")).unwrap(), b"owned disk");
            symlink(&shared, scratch.join("backing-link")).unwrap();
            if mode == "replaced-directory" {
                std::fs::rename(&scratch, f.root().join("displaced-temporary")).unwrap();
                symlink(&shared, &scratch).unwrap();
                let retirement = device.stop();
                assert!(!retirement.passed);
                assert!(retirement.errors.iter().any(|e| e.contains("identity changed")));
                assert!(!load::<HostSession>(&f.root().join("host/native-session.json")).unwrap().unwrap().retired);
                assert!(scratch.is_symlink());
            } else {
                assert!(device.stop().passed);
                assert!(device.stop().passed);
            }
        }
        assert_eq!(std::env::var_os("ANDROID_TMP"), inherited);
        assert_eq!(std::fs::read(shared.join("base.qcow2")).unwrap(), b"shared backing");
        if mode != "replaced-directory" {
            assert!(!scratch.exists());
            let report = std::fs::read_to_string(f.root().join("session/emulator-temporary-cleanup.json")).unwrap();
            assert!(report.contains("overlay.qcow2"));
            f.retired();
        }
    }
}

#[test]
fn view_restart_preserves_installation_and_fences_uncertain_stop_or_cold_start_without_replay() {
    use frontcap_core::{ids::RequestId,native::request::{ClockRequest,ClockOperation}};
    let cold="client-test://item/1?literal='value'&second=2";
    for (url,failure) in [(None,None),(Some(cold),None),(Some(cold),Some("lose-stop")),(Some(cold),Some("lose-start"))] {
        let mut f=Fixture::view(false);
        f.config.view_app.as_mut().unwrap().launch_extras.insert("app.label".into(),"한글 'quote' $(printf injected)".into());
        f.record_view_restarts();
        let cancel=AtomicBool::new(false);let mut device=f.acquire(&cancel).unwrap();
        let old=device.view_ready().unwrap().clone();let deadline=Instant::now()+Duration::from_secs(20);
        let id=RequestId::new("restart").unwrap();
        assert!(device.restart_view(&id,"abcd0000-0000-0000-0000-000000000099",None,deadline,&cancel).is_err());
        assert!(!f.root().join("replacement-effects.jsonl").exists());
        assert!(device.restart_view(&id,&old.generation,Some("relative/path"),deadline,&cancel).is_err());
        assert!(!f.root().join("replacement-effects.jsonl").exists());
        if let Some(name)=failure {std::fs::write(f.root().join(name),b"").unwrap();}
        if url.is_some() {std::fs::write(f.root().join("start-status"),b"timeout").unwrap();}
        let result=device.restart_view(&id,&old.generation,url,deadline,&cancel);
        if failure.is_some() {
            assert!(result.is_err());assert!(device.view_ready().is_none());
            for name in ["restart","another"] {
                assert!(device.restart_view(&RequestId::new(name).unwrap(),&old.generation,url,deadline,&cancel).is_err());
                assert!(device.restart_view(&RequestId::new(name).unwrap(),&old.generation,None,deadline,&cancel).is_err());
            }
            assert!(read_view_replacement(&f.root().join("session"),&id,&old.generation,url).unwrap().is_none());
            assert_eq!(std::fs::read_to_string(f.root().join("replacement-effects.jsonl")).unwrap(),if failure==Some("lose-stop") {"stop\n"} else {"stop\nstart\n"});
        } else {
            let result=result.unwrap();assert_eq!(result.previous,old);assert_ne!(result.owner.generation,old.generation);
            assert_eq!(result.entry.as_ref().map(|e|e.request.url.as_str()),url);
            if let Some(url)=url {
                let args:Vec<String>=load(&f.root().join("replacement-launch.json")).unwrap().unwrap();
                let mut expected=crate::external_entry::android_arguments(&old.package,url);
                expected.extend(["--es".into(),"app.label".into(),"'한글 '\"'\"'quote'\"'\"' $(printf injected)'".into()]);
                assert_eq!(args,expected);
                assert!(device.restart_view(&id,&old.generation,Some("client-test://item/2"),deadline,&cancel).is_err());
            }
            assert_eq!(device.view_ready(),Some(&result.owner));
            let immutable=std::fs::read(f.root().join("session/view-ready.json")).unwrap();
            assert_eq!(load::<crate::android_app::Ready>(&f.root().join("session/view-ready.json")).unwrap().unwrap(),old);
            assert_eq!(device.restart_view(&id,&old.generation,url,deadline,&cancel).unwrap(),result);
            let request=crate::request::NativeRequest::Clock(ClockRequest {generation:result.owner.generation.clone(),request_id:1,operation:ClockOperation::Read,delta_ns:0});
            device.execute_view_operation(&request,&f.root().join("new-clock"),deadline,&cancel).unwrap();
            assert_eq!(std::fs::read(f.root().join("session/view-ready.json")).unwrap(),immutable);
            assert_eq!(std::fs::read_to_string(f.root().join("replacement-effects.jsonl")).unwrap(),"stop\nstart\n");
            assert_eq!(read_view_replacement(&f.root().join("session"),&id,&old.generation,url).unwrap(),Some(result));
        }
        assert!(device.stop().passed);f.retired();
    }
}

#[test]
fn external_entry_binds_view_owner_and_never_replays_lost_adb_reply() {
    use frontcap_core::{ids::RequestId, native::external_entry::{Request, Target}};
    for lost in [false, true] {
        let f = Fixture::view(false);
        f.record_url_dispatches(lost);
        let cancel = AtomicBool::new(false);
        let mut device = f.acquire(&cancel).unwrap();
        let owner = device.view_ready().unwrap();
        let request = Request { target: Target::Android { serial: owner.serial.clone(), package: owner.package.clone() },
            generation: owner.generation.clone(), url: "client-test://item/1?value='x'&other=2".into() };
        let id = RequestId::new("entry").unwrap();
        let deadline = Instant::now()+Duration::from_secs(8);
        let mut stale = request.clone();stale.generation = "aaaaaaaa-bbbb-cccc-dddd-000000000002".into();
        assert!(device.dispatch_view_url(&id, stale, deadline, &cancel).is_err());
        assert!(!f.root().join("entry-effects.jsonl").exists());
        let first = device.dispatch_view_url(&id, request.clone(), deadline, &cancel);
        if lost {
            assert!(first.is_err());
            for name in ["entry", "retry"] {
                assert!(device.dispatch_view_url(&RequestId::new(name).unwrap(), request.clone(), deadline, &cancel).is_err());
            }
        } else {
            assert_eq!(device.dispatch_view_url(&id, request.clone(), deadline, &cancel).unwrap(), first.unwrap());
        }
        assert_eq!(std::fs::read_to_string(f.root().join("entry-effects.jsonl")).unwrap().lines().count(), 1);
        assert!(device.stop().passed);
        assert!(device.dispatch_view_url(&RequestId::new("stopped").unwrap(), request.clone(), deadline, &cancel).is_err());
        let saved = crate::external_entry::read_accepted(&f.root().join("session/external-entry"), &id, &request).unwrap();
        assert_eq!(saved.is_some(), !lost);
        f.retired();
    }
}

pub(crate) struct Fixture {
    temp: tempfile::TempDir,
    pub(crate) config: AndroidLaunch,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(20000);
        let start = NEXT_PORT.fetch_add(100, Ordering::Relaxed);
        let port = (start..start + 100)
            .step_by(2)
            .find(|port| {
                let Ok(a) = TcpListener::bind((Ipv4Addr::LOCALHOST, *port)) else {
                    return false;
                };
                let b = TcpListener::bind((Ipv4Addr::LOCALHOST, *port + 1));
                drop(a);
                b.is_ok()
            })
            .unwrap();
        let emulator = root.join("emulator");
        std::fs::write(&emulator, "#!/usr/bin/env python3\nimport socket,sys,time\nfrom pathlib import Path\np=int(sys.argv[sys.argv.index('-port')+1])\nPath(__file__).with_name('emulator-argv.json').write_text(__import__('json').dumps(sys.argv[1:]))\nPath(__file__).with_name('emulator-avd-home.txt').write_text(__import__('os').environ.get('ANDROID_AVD_HOME', ''))\nsockets=[]\nfor port in [p,p+1]:\n s=socket.socket(); s.bind(('127.0.0.1',port)); s.listen(); sockets.append(s)\nend=time.monotonic()+30\nwhile time.monotonic()<end and not Path(__file__).with_name('retire-fake').exists(): time.sleep(.01)\n").unwrap();
        std::fs::set_permissions(&emulator, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adb = root.join("adb");
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/adb.py"),
        )
        .unwrap();
        let source = source
            .replace(
                "path = Path(os.environ['FRONTCAP_FAKE_STATE'])",
                &format!(
                    "path = Path({})",
                    serde_json::to_string(&root.join("target.json")).unwrap()
                ),
            )
            .replace("'emulator-5580'", &format!("'emulator-{port}'"));
        let point = "if args == ['shell', 'pidof', 'org.example.app']:";
        let insert = format!("
owned = Path({})
assert owned.is_file() or (owned.parent/'device'/owned.name).is_file(), 'ADB before process ownership publication'
root=path.parent
if (root/'diagnostic-block').exists() and args == ['shell','pidof','org.example.app']:
 (root/'diagnostic-started').touch()
 while not (root/'diagnostic-release-output').exists(): time.sleep(.005)
 print('partial snapshot',flush=True)
 (root/'diagnostic-output-ready').touch()
 time.sleep(30)
with (root/'adb-calls.jsonl').open('a') as log: log.write(json.dumps(args)+'\\n')
if args[:1] == ['logcat'] and (root/'startup-block').exists():
 (root/'startup-started').touch(); time.sleep(30)
if args[:1] == ['logcat'] and 'am_anr:I' in args:
 assert '--pid' not in args, 'service evidence filtered to fixture PID'
 buffers=[args[i+1] for i,a in enumerate(args[:-1]) if a=='-b']
 if 'main' in buffers and 'crash' in buffers and 'AndroidRuntime:E' in args:
  print('AndroidRuntime: retained fixture crash',flush=True)
 if (root/'service-denied').exists(): print('Permission denied',file=sys.stderr); sys.exit(13)
 print('am_anr: [0,999,com.google.android.apps.messaging,0,fixture service timeout]'); sys.exit(0)
if args[:1] == ['logcat']: print('phase evidence'); sys.exit(0)
if args == ['shell','dumpsys','dropbox','--print','system_app_anr']:
 if (root/'service-denied').exists(): print('Permission Denial: cannot dump DropBoxManagerService'); sys.exit(0)
 print('system_app_anr: retained startup stack'); sys.exit(0)
if args == ['shell','dumpsys','activity','broadcasts']:
 if (root/'queue-sample-block').exists():
  (root/'queue-sample-started').touch(); time.sleep(30)
 if (root/'large-broadcasts').exists(): print('x' * (1024 * 1024))
 print('pending org.example.app; running com.google.android.apps.messaging BOOT_COMPLETED'); sys.exit(0)
if args[:3] == ['shell','dumpsys','activity']: print('broadcast inventory'); sys.exit(0)
if args[:3] == ['shell','ps','-A']: print('PID PPID STAT NAME'); print('42 1 S org.example.app'); sys.exit(0)
if args == ['shell','cat','/sys/devices/system/cpu/online'] and (root/'resource-delay').exists():
 (root/'resource-started').touch(); time.sleep(float((root/'resource-delay').read_text()))
if args in [['shell','cat','/sys/devices/system/cpu/present'],['shell','cat','/sys/devices/system/cpu/online']]: print('0-3'); sys.exit(0)
if args == ['shell','cat','/proc/meminfo']: print('MemTotal: 3800000 kB'); sys.exit(0)
if args == ['shell','getprop','sys.boot_completed']:
 (root/'boot-polled').touch(); print('0' if (root/'boot-block').exists() else '1'); sys.exit(0)
if args == ['shell','pm','list','packages','--user','0','org.example.app']: print(''); sys.exit(0)
if args[:1] == ['install']:
 if (root/'install-fail').exists(): print('install failed',file=sys.stderr); sys.exit(7)
 print('Success'); sys.exit(0)
if args in [['shell','input','keyevent','KEYCODE_WAKEUP'], ['shell','wm','dismiss-keyguard'], ['shell','am','start','-W','-n','org.example.app/org.example.app.ClientActivity']]: sys.exit(0)
",serde_json::to_string(&root.join("session/emulator-process.json")).unwrap());
        std::fs::write(&adb, source.replace(point, &(insert + point))).unwrap();
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o700)).unwrap();
        let apk = root.join("app.apk");
        std::fs::write(&apk, b"frozen fake APK").unwrap();
        let apk_sha256 = Sha256::digest(b"frozen fake APK")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let config = AndroidLaunch {
                    artifact_budget_bytes: None,
                    host_app: None,
            view_app: None,
            webview_app: None,
            emulator,
            adb,
            adb_server_port: None,
            avd: "fixture".into(),
            avd_home: None,
                    avd_inventory: None,
                    temporary_storage_root: None,
                    scratch_volume: None,
                    confine_emulator_writes: false,
            port,
            cores: None,
            memory_mb: None,
            startup_diagnostics: false,
            startup_broadcast_sample_ms: 0,
            startup_broadcast_interval_ms: 250,
            apk,
            apk_sha256,
            initial_data: None,
            python: "python3".into(),
            bridge_script: PathBuf::new(),
            input_registration: Default::default(),
            clock_transport: Default::default(),
            source_sha256: "a".repeat(64),
        };
        Self { temp, config }
    }
    pub(crate) fn view(wrong_window: bool) -> Self {
        let mut f=Fixture::new();
        let source=f.root().join("source");
        std::fs::create_dir(&source).unwrap();
        crate::android_capture::tests::fixture(&source);
        if wrong_window {
            std::fs::write(source.join("window-before.txt"),b"mCurrentFocus=Window{abc u0 dev.other/.Main}\n").unwrap();
        }
        let original=std::fs::read_to_string(&f.config.adb).unwrap();
        let original=original.replace("org.example.app", "org.example.client")
            .replace("ClientActivity", "Main");
        let point="if args == ['shell', 'pidof', 'org.example.client']:";
        let insert=r#"
source=root/'source'
if args[:6]==['shell','am','start','-W','-n','org.example.client/org.example.client.Main'] and len(args)>6:
 import shlex
 assert shlex.split(' '.join(args[6:]))==['--es','app.label',"한글 'quote' $(printf injected)"]
 (root/'extras-received').touch(); sys.exit(0)
if args==['shell','pidof','org.example.client']: print('123'); sys.exit(0)
if args==['shell','run-as','org.example.client','cat','files/clock-control.json']:
 sys.stdout.buffer.write((source/'clock-before.json').read_bytes()); sys.exit(0)
if args[:3]==['shell','content','call'] and args[args.index('--uri')+1]=='content://org.example.client.clock':
 values={}
 for i,arg in enumerate(args):
  if arg=='--extra':
   name,kind,value=args[i+1].split(':',2); values[name]=int(value) if kind=='l' else value
 clock=json.loads((source/'clock-before.json').read_bytes())
 assert values['generation']==clock['generation'] and values['request_id']==clock['last_id']+1
 before=clock['now_ns']; applied=values['operation']=='advance'
 clock.update(request=values,last_id=values['request_id'],before_ns=before,now_ns=before+(values['delta_ns'] if applied else 0),applied=applied,status='ok',observation={})
 (source/'clock-before.json').write_text(json.dumps(clock))
 with (root/'clock-effects.jsonl').open('a') as log:log.write(json.dumps(values)+'\n')
 if applied and (root/'block').exists():
  (root/'advance-blocked').touch()
  while (root/'block').exists(): time.sleep(.02)
 print('Result: Bundle[{submitted=true}]'); sys.exit(0)
if args==['shell','run-as','org.example.client','cat','files/view-scroll.json']:
 sys.stdout.buffer.write((root/'view-scroll.json').read_bytes());sys.exit(0)
if args[:3]==['shell','content','call'] and args[args.index('--method')+1]=='scroll':
 import base64
 request=json.loads(base64.b64decode(next(a.split(':',2)[2] for a in args if a.startswith('request_base64:s:'))))
 clock=json.loads((source/'clock-before.json').read_bytes())
 result=dict(request=request,generation=clock['generation'],pid=clock['pid'],now_ns=clock['now_ns'],last_id=request['request_id'],state='armed')
 if (root/'scroll-rejected').exists():result.update(state='rejected',reason='changed_view_state')
 (root/'view-scroll.json').write_text(json.dumps(result));print('Result: Bundle[{submitted=true}]');sys.exit(0)
if args[:3]==['shell','input','swipe']:
 result=json.loads((root/'view-scroll.json').read_bytes());assert result['state']=='armed';r=result['request']
 assert args[3:]==[str(r['x']),str(r['y']),str(r['end_x']),str(r['end_y']),'600']
 value=json.loads((source/'views-before.json').read_bytes());scroll=value['catalog']['targets'][0]['scroll'];before=list(scroll['offset_px'])
 scroll['offset_px'][1]+=50
 result.update(state='completed',down_handled=True,up_handled=True,moves=12,before_offset_px=before,after_offset_px=scroll['offset_px'])
 (source/'views-before.json').write_text(json.dumps(value));(root/'view-scroll.json').write_text(json.dumps(result))
 with (root/'scroll-effects.jsonl').open('a') as log:log.write(json.dumps(r)+'\n')
 if (root/'scroll-lost-reply').exists():sys.exit(1)
 sys.exit(0)
if args==['shell','run-as','org.example.client','cat','files/view-tap.json']:
 sys.stdout.buffer.write((root/'view-tap.json').read_bytes());sys.exit(0)
if args[:3]==['shell','content','call'] and args[args.index('--method')+1]=='tap':
 import base64
 request=json.loads(base64.b64decode(next(a.split(':',2)[2] for a in args if a.startswith('request_base64:s:'))))
 clock=json.loads((source/'clock-before.json').read_bytes())
 result=dict(request=request,generation=clock['generation'],pid=clock['pid'],now_ns=clock['now_ns'],last_id=request['request_id'],state='armed')
 if (root/'tap-rejected').exists():result.update(state='rejected',reason='changed_view_state')
 (root/'view-tap.json').write_text(json.dumps(result));print('Result: Bundle[{submitted=true}]');sys.exit(0)
if args[:3]==['shell','input','tap']:
 result=json.loads((root/'view-tap.json').read_bytes());assert result['state']=='armed'
 assert args[3:]==[str(result['request']['x']),str(result['request']['y'])]
 result.update(state='completed',down_handled=True,up_handled=True)
 value=json.loads((source/'views-before.json').read_bytes());value['catalog']['targets'][0]['selected']=True
 (source/'views-before.json').write_text(json.dumps(value))
 (root/'view-tap.json').write_text(json.dumps(result))
 with (root/'tap-effects.jsonl').open('a') as log:log.write(json.dumps(result['request'])+'\n')
 if (root/'tap-lost-reply').exists():sys.exit(1)
 sys.exit(0)
if args[:5]==['shell','-T','run-as','org.example.client','tee']:
 raw=sys.stdin.buffer.read(); (root/Path(args[5]).name).write_bytes(raw);sys.stdout.buffer.write(raw);sys.exit(0)
if args==['shell','run-as','org.example.client','cat','files/view-edit.json']:
 sys.stdout.buffer.write((root/'view-edit.json').read_bytes());sys.exit(0)
if args[:3]==['shell','content','call'] and args[args.index('--method')+1]=='edit':
 import hashlib
 digest=next(a.split(':',2)[2] for a in args if a.startswith('payload_sha256:s:'))
 raw=(root/('view-edit-'+digest+'.json')).read_bytes();assert hashlib.sha256(raw).hexdigest()==digest
 request=json.loads(raw);clock=json.loads((source/'clock-before.json').read_bytes())
 result=dict(request=request,generation=clock['generation'],pid=clock['pid'],now_ns=clock['now_ns'],last_id=request['request_id'])
 value=json.loads((source/'views-before.json').read_bytes());view=value['catalog']['targets'][0]
 if request['observation']!=(root/'view-request').read_text() or request['now_ns']!=clock['now_ns']:
  result.update(state='rejected',reason='stale_observation')
 else:
  before=view['text'];units=before.encode('utf-16-le')
  after=units[:request['start']*2].decode('utf-16-le')+request['text']+units[request['end']*2:].decode('utf-16-le')
  result.update(state='completed',before=before,after=after);view['text']=after
  (source/'views-before.json').write_text(json.dumps(value))
  with (root/'edit-effects.jsonl').open('a') as log:log.write(json.dumps(request)+'\n')
 (root/'view-edit.json').write_text(json.dumps(result))
 if (root/'edit-lost-reply').exists():sys.exit(1)
 print('Result: Bundle[{submitted=true}]');sys.exit(0)
if args[:3]==['shell','content','call']:
 assert args[args.index('--uri')+1]=='content://org.example.client.views'
 (root/'view-request').write_text(next(a.split(':',2)[2] for a in args if a.startswith('request:s:')))
 print('Result: Bundle[{submitted=true}]'); sys.exit(0)
if args==['shell','run-as','org.example.client','cat','files/view-observation.json']:
 value=json.loads((source/'views-before.json').read_bytes())
 value['request']=(root/'view-request').read_text(); value['now_ns']=json.loads((source/'clock-before.json').read_bytes())['now_ns']; print(json.dumps(value)); sys.exit(0)
if args==['shell','dumpsys','window']:
 sys.stdout.buffer.write((source/'window-before.txt').read_bytes()); sys.exit(0)
if args==['exec-out','screencap','-p']:
 if (root/'change-on-screen').exists():
  value=json.loads((source/'views-before.json').read_bytes());value['catalog']['ime_visible']=True
  (source/'views-before.json').write_text(json.dumps(value));(root/'change-on-screen').unlink()
 sys.stdout.buffer.write((source/'screen.png').read_bytes()); sys.exit(0)
"#;
        std::fs::write(&f.config.adb,original.replace(point,&format!("{insert}\n{point}"))).unwrap();
        f.config.python="/usr/bin/python3".into();
        f.config.view_app=Some(crate::android_app::Profile { launch_extras: Default::default(), service_url_extra: None, package:"org.example.client".into(),
            activity:"org.example.client.Main".into(),scroll_script:Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_android_view_scroll.py"),tap_script:Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_android_view_tap.py"),edit_script:Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_android_view_edit.py"),clock_script:Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_android_view_clock.py"),capture_script:Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tools/native_android_view_capture.py") });
        f
    }
    pub(crate) fn record_view_restarts(&self) {
        let script=std::fs::read_to_string(&self.config.adb).unwrap();
        let extra=r#"
source=root/'source'
stopped=(root/'view-stopped').exists();replaced=(root/'view-replaced').exists();pid=124 if replaced else 123
if args==['shell','pm','path','org.example.client']:
 print('package:/data/app/~~owner==/org.example.client-product==/base.apk');sys.exit(0)
if args[:2]==['shell','sha256sum']:
 print(FROZEN_HASH+'  '+args[2]);sys.exit(0)
if args==['shell','run-as','org.example.client','pwd']:
 print('/data/user/0/org.example.client');sys.exit(0)
if args==['shell','run-as','org.example.client','stat','-c',"'%d %i'",'.']:
 print('2049 5511');sys.exit(0)
if args==['shell','cat','/proc/sys/kernel/random/boot_id']:
 print('abcd0000-0000-0000-0000-000000000001');sys.exit(0)
if args[:4]==['shell','run-as','org.example.client','cat'] and args[4].startswith('/proc/'):
 assert args[4].split('/')[2]==str(pid)
 if args[4].endswith('/stat'):print(str(pid)+' (client main) S '+' '.join(['0']*18)+' '+('60' if replaced else '50')+' 0 0')
 else:sys.stdout.buffer.write(b'org.example.client\0')
 sys.exit(0)
if args==['shell','pidof','org.example.client']:
 if stopped:sys.exit(1)
 print(pid);sys.exit(0)
if args[:6]==['shell','run-as','org.example.client','test','!','-d']:
 assert stopped;sys.exit(0)
if args==['shell','am','force-stop','org.example.client']:
 (root/'view-stopped').touch()
 with (root/'replacement-effects.jsonl').open('a') as log:log.write('stop\n')
 if (root/'lose-stop').exists():sys.exit(9)
 sys.exit(0)
if stopped and args[:4]==['shell','am','start','-W']:
 with (root/'replacement-effects.jsonl').open('a') as log:log.write('start\n')
 (root/'replacement-launch.json').write_text(json.dumps(args))
 def changed(v):
  if type(v)is dict:return {k:124 if k=='pid' else 'abcd0000-0000-0000-0000-000000000009' if k=='generation' else changed(x) for k,x in v.items()}
  if type(v)is list:return [changed(x) for x in v]
  return v
 for path in source.glob('*.json'):
  value=changed(json.loads(path.read_text()))
  if path.name.startswith('clock-'):value.update(now_ns=0,status='ready',last_id=0)
  path.write_text(json.dumps(value))
 (root/'view-stopped').unlink();(root/'view-replaced').touch()
 if (root/'lose-start').exists():sys.exit(9)
 status=(root/'start-status').read_text() if (root/'start-status').exists() else 'ok'
 print('Status: '+status+'\nActivity: org.example.client/.Main');sys.exit(0)
"#.replace("FROZEN_HASH",&serde_json::to_string(&self.config.apk_sha256).unwrap());
        std::fs::write(&self.config.adb,script.replacen("if args in [['shell','input','keyevent'",&format!("{extra}\nif args in [['shell','input','keyevent'"),1)).unwrap();
    }
    pub(crate) fn record_view_lifecycle(&self, mode: &str) {
        self.record_view_restarts();
        let source = std::fs::read_to_string(&self.config.adb).unwrap();
        let extra = r#"
mode=MODE
if args==['shell','input','keyevent','KEYCODE_HOME']:
 (root/'view-background').touch()
 with (root/'host-effects.jsonl').open('a') as log:log.write('background\n')
 sys.exit(9 if mode=='lost' else 0)
if args==['shell','am','start','-W','--activity-reorder-to-front','-n','org.example.client/org.example.client.Main']:
 (root/'view-background').unlink()
 with (root/'host-effects.jsonl').open('a') as log:log.write('foreground\n')
 print('Status: ok\nActivity: org.example.client/.Main');sys.exit(0)
if args==['shell','dumpsys','window'] and (root/'view-background').exists():
 print('mCurrentFocus=Window{123 u0 dev.example.launcher/.Main}');sys.exit(0)
if ((mode=='replaced' and (root/'view-background').exists()) or (mode=='between' and (root/'replace-return').exists())) and args==['shell','run-as','org.example.client','cat','/proc/123/stat']:
 print('123 (client main) S '+' '.join(['0']*18)+' 99 0 0');sys.exit(0)
"#.replace("MODE", &serde_json::to_string(mode).unwrap());
        let marker = "source=root/'source'\nstopped=";
        assert!(source.contains(marker));
        std::fs::write(&self.config.adb, source.replacen(marker, &format!("{extra}\n{marker}"), 1)).unwrap();
    }
    pub(crate) fn record_url_dispatches(&self, lost: bool) {
        let source = std::fs::read_to_string(&self.config.adb).unwrap();
        let marker = "if args == ['shell', 'pidof', 'org.example.client']:";
        let addition = format!(r#"
if args[:6] == ['shell','am','start','-W','-a','android.intent.action.VIEW']:
 with (root/'entry-effects.jsonl').open('a') as log: log.write(json.dumps(args)+'\n')
 if {lost}: sys.exit(9)
 print('Status: ok\nActivity: org.example.client/.Main');sys.exit(0)
"#, lost=if lost {"True"} else {"False"});
        assert!(source.contains(marker));
        std::fs::write(&self.config.adb, source.replacen(marker, &format!("{addition}\n{marker}"), 1)).unwrap();
    }
    pub(crate) fn root(&self) -> &Path {
        self.temp.path()
    }
    fn acquire(&self, cancelled: &AtomicBool) -> Result<Device, AcquisitionError> {
        Device::acquire_at(
            self.config.clone(),
            &self.root().join("session"),
            Instant::now() + Duration::from_secs(8),
            cancelled,
            &self.root().join("host"),
        )
    }
    fn retired(&self) {
        let identity: ProcessIdentity = load(&self.root().join("session/emulator-process.json"))
            .unwrap()
            .unwrap();
        assert!(identity.has_exited().unwrap());
        assert!(
            load::<HostSession>(&self.root().join("host/native-session.json"))
                .unwrap()
                .unwrap()
                .retired
        );
    }
}

#[test]
fn owned_acquisition_binds_ready_identity_excludes_second_owner_and_retires_device() {
    let f = Fixture::view(false);
    let cancel = AtomicBool::new(false);
    let mut device = f.acquire(&cancel).unwrap();
    assert_eq!(
        device.view_ready().unwrap().serial,
        format!("emulator-{}", f.config.port)
    );
    assert_eq!(device.view_ready().unwrap().apk_sha256, f.config.apk_sha256);
    let python_lock = || {
        Command::new("python3").args(["-c",
        "import fcntl,sys; f=open(sys.argv[1],'a');\ntry: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)\nexcept BlockingIOError: sys.exit(2)"])
        .arg(f.root().join("host/native-tests.lock")).status().unwrap()
    };
    assert!(
        !python_lock().success(),
        "legacy Python runner must share the Rust host lock"
    );
    let other = Device::acquire_at(
        f.config.clone(),
        &f.root().join("second"),
        Instant::now() + Duration::from_secs(2),
        &cancel,
        &f.root().join("host"),
    );
    assert!(other.is_err());
    assert!(!f.root().join("second").exists());
    assert!(device.stop().passed);
    assert!(python_lock().success());


    f.retired();
}
#[test]
fn install_failure_preserves_primary_error_and_retires_partial_acquisition() {
    let f = Fixture::view(false);
    std::fs::write(f.root().join("install-fail"), b"fail").unwrap();
    let Err(error) = f.acquire(&AtomicBool::new(false)) else {
        panic!("installation must fail")
    };
    assert!(error.cause.contains("exited unsuccessfully"), "{error:?}");
    assert!(error.retirement.unwrap().passed);
    f.retired();
    assert!(!f.root().join("session/ready.json").exists());
}
#[test]
fn cancellation_during_boot_retires_owned_emulator_without_installing() {
    let f = Fixture::view(false);
    std::fs::write(f.root().join("boot-block"), b"block").unwrap();
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !f.root().join("boot-polled").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancel.store(true, Ordering::Release);
        });
        let Err(error) = f.acquire(&cancel) else {
            panic!("cancelled acquisition succeeded")
        };
        assert!(error.cause.contains("cancelled"), "{error:?}");
        assert!(error.retirement.unwrap().passed);
    });
    f.retired();
    assert!(
        !std::fs::read_to_string(f.root().join("adb-calls.jsonl"))
            .unwrap()
            .contains("install")
    );
}
#[test]
fn failed_retirement_publication_fences_later_launch_even_after_process_exit() {
    let f = Fixture::view(false);
    let cancel = AtomicBool::new(false);
    let mut device = f.acquire(&cancel).unwrap();
    std::fs::create_dir(f.root().join("session/retirement.json")).unwrap();
    assert!(!device.stop().passed);
    drop(device);
    let Err(error) = Device::acquire_at(
        f.config.clone(),
        &f.root().join("second"),
        Instant::now() + Duration::from_secs(2),
        &cancel,
        &f.root().join("host"),
    ) else {
        panic!("unretired host fence ignored")
    };
    assert!(error.cause.contains("unretired"), "{error:?}");
    assert!(!f.root().join("second").exists());
}
#[test]
fn changed_apk_and_busy_ports_do_not_contact_an_existing_target() {
    let f = Fixture::view(false);
    std::fs::write(&f.config.apk, b"changed").unwrap();
    assert!(f.acquire(&AtomicBool::new(false)).is_err());
    assert!(!f.root().join("session").exists());
    let f = Fixture::view(false);
    let _foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, f.config.port)).unwrap();
    assert!(f.acquire(&AtomicBool::new(false)).is_err());
    assert!(!f.root().join("adb-calls.jsonl").exists());
    assert!(!f.root().join("session/emulator-process.json").exists());
}

#[test]
fn device_owner_entry() {
    let Ok(root) = std::env::var("FRONTCAP_DEVICE_OWNER_TEST") else {
        return;
    };
    let root = PathBuf::from(root);
    let config: AndroidLaunch = load(&root.join("config.json")).unwrap().unwrap();
    let _device = Device::acquire_at(
        config,
        &root.join("session"),
        Instant::now() + Duration::from_secs(8),
        &AtomicBool::new(false),
        &root.join("host"),
    )
    .unwrap();
    std::fs::write(root.join("owner-ready"), b"ready").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
fn owner_death_keeps_host_fenced_without_signalling_saved_pids_or_starting_another_device() {
    let f = Fixture::view(false);
    save_atomic(&f.root().join("config.json"), &f.config).unwrap();
    struct Owner(std::process::Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut owner = Owner(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device::tests::device_owner_entry",
                "--nocapture",
            ])
            .env("FRONTCAP_DEVICE_OWNER_TEST", f.root())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(7);
    while !f.root().join("owner-ready").exists() {
        assert!(owner.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    let identity: ProcessIdentity = load(&f.root().join("session/emulator-process.json"))
        .unwrap()
        .unwrap();
    assert!(
        !identity.has_exited().unwrap(),
        "control requires a surviving device"
    );
    let Err(error) = Device::acquire_at(
        f.config.clone(),
        &f.root().join("second"),
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
        &f.root().join("host"),
    ) else {
        panic!("orphaned session replaced")
    };
    assert!(error.cause.contains("unretired"), "{error:?}");
    assert!(!f.root().join("second").exists());
    // Test-only retirement of our fake emulator by its private file protocol, never a recovered PID.
    std::fs::write(f.root().join("retire-fake"), b"stop").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !identity.has_exited().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !load::<HostSession>(&f.root().join("host/native-session.json"))
            .unwrap()
            .unwrap()
            .retired
    );
}




#[test]
fn requested_resources_reach_emulator_and_guest_resources_remain_distinct() {
    for explicit in [false, true] {
        let mut f = Fixture::view(false);
        if explicit {
            f.config.cores = Some(4);
            f.config.memory_mb = Some(4096);
        }
        let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
        let actual: Vec<String> = load(&f.root().join("emulator-argv.json")).unwrap().unwrap();
        let (_, recorded): (PathBuf, Vec<String>) =
            load(&f.root().join("session/emulator-command.json"))
                .unwrap()
                .unwrap();
        assert_eq!(actual, recorded);
        for (flag, value) in [("-cores", "4"), ("-memory", "4096")] {
            if explicit {
                let i = actual.iter().position(|a| a == flag).unwrap();
                assert_eq!(actual[i + 1], value);
            } else {
                assert!(!actual.iter().any(|a| a == flag));
            }
        }
        assert_eq!(
            std::fs::read_to_string(f.root().join("session/cpu-online.txt")).unwrap(),
            "0-3\n"
        );
        assert_eq!(
            std::fs::read_to_string(f.root().join("session/meminfo.txt")).unwrap(),
            "MemTotal: 3800000 kB\n"
        );
        assert!(device.stop().passed);
    }
}

#[test]
fn zero_resources_are_rejected_before_target_or_evidence_creation() {
    for memory in [false, true] {
        let mut f = Fixture::view(false);
        if memory {
            f.config.memory_mb = Some(0);
        } else {
            f.config.cores = Some(0);
        }
        assert!(f.acquire(&AtomicBool::new(false)).is_err());
        assert!(!f.root().join("session").exists());
        assert!(!f.root().join("emulator-argv.json").exists());
        assert!(!f.root().join("adb-calls.jsonl").exists());
    }
}

#[test]
fn isolated_avd_inventory_is_scoped_to_owned_emulator() {
    let mut f = Fixture::view(false);
    let home = f.root().join("isolated-avds");
    std::fs::create_dir(&home).unwrap();
    f.config.avd_home = Some(home.clone());
    let inherited = std::env::var_os("ANDROID_AVD_HOME");
    let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read_to_string(f.root().join("emulator-avd-home.txt")).unwrap(), home.to_str().unwrap());
    assert_eq!(std::env::var_os("ANDROID_AVD_HOME"), inherited);
    assert!(device.stop().passed);
    let f = Fixture::view(false);
    for invalid in [PathBuf::from("relative"), f.root().join("missing")] {
        let mut config = f.config.clone(); config.avd_home = Some(invalid);
        assert!(Device::acquire_at(config, &f.root().join("session"), Instant::now()+Duration::from_secs(2),
            &AtomicBool::new(false), &f.root().join("host")).is_err());
        assert!(!f.root().join("session").exists());
        assert!(!f.root().join("emulator-argv.json").exists());
    }
}

#[test]
fn unsupported_write_confinement_refuses_before_sdk_or_session_creation() {
    let mut fixture = Fixture::view(false);
    fixture.config.confine_emulator_writes = true;
    let error = fixture.acquire(&AtomicBool::new(false)).err().expect("missing confinement prerequisites");
    assert!(error.to_string().contains("emulator write confinement"));
    assert!(!fixture.root().join("session").exists());
    assert!(!fixture.root().join("emulator-argv.json").exists());
    assert!(!fixture.root().join("adb-calls.jsonl").exists());
}

#[test]
fn private_adb_requires_supervised_storage_before_any_sdk_effect() {
    let mut fixture = Fixture::view(false);
    for port in [0, 5037, fixture.config.port, fixture.config.port + 1, 15037] {
        fixture.config.adb_server_port = Some(port);
        let error = fixture.acquire(&AtomicBool::new(false)).err().expect("private server prerequisites absent");
        assert!(error.to_string().contains("private ADB server"), "{error}");
        assert!(!fixture.root().join("session").exists());
        assert!(!fixture.root().join("emulator-argv.json").exists());
        assert!(!fixture.root().join("adb-calls.jsonl").exists());
    }
}

#[cfg(unix)]
#[test]
fn retired_owner_releases_inherited_host_lock_but_keeps_unpublished_retirement_fenced() {
    use std::{io::Write, os::{fd::AsRawFd, unix::net::UnixStream}};
    let f = Fixture::view(false);
    let cancel = AtomicBool::new(false);
    let mut device = f.acquire(&cancel).unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    reader.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let read_fd = reader.as_raw_fd(); let write_fd = writer.as_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // Only async-signal-safe calls in the forked multithreaded test process.
        unsafe {
            libc::close(write_fd);
            let mut byte = 0u8;
            libc::read(read_fd, (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    drop(reader);
    struct Fork { pid: libc::pid_t, writer: UnixStream }
    impl Drop for Fork {
        fn drop(&mut self) {
            let _ = self.writer.write_all(&[1]);
            loop {
                let waited = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
                if waited >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted { break; }
            }
        }
    }
    let inherited = Fork { pid, writer };
    std::fs::create_dir(f.root().join("session/retirement.json")).unwrap();
    assert!(!device.stop().passed);
    drop(device);
    let result = Device::acquire_at(f.config.clone(), &f.root().join("second"),
        Instant::now()+Duration::from_secs(2), &cancel, &f.root().join("host"));
    drop(inherited);
    let Err(error) = result else { panic!("unpublished retirement was not fenced"); };
    assert!(error.cause.contains("unretired"), "{error:?}");
    assert!(!f.root().join("second").exists());
}

#[test]
fn startup_service_evidence_precedes_fixture_operations_and_preserves_denials() {
    for enabled in [false, true] {
        let mut f = Fixture::view(false);
        f.config.startup_diagnostics = enabled;
        std::fs::write(f.root().join("large-broadcasts"), b"large queue inventory").unwrap();
        std::fs::write(f.root().join("service-denied"), b"deny optional diagnostics").unwrap();
        let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
        let directory = f.root().join("session/startup-observation");
        assert_eq!(directory.exists(), enabled);
        assert!(!f.root().join("session/startup-broadcasts").exists());
        let calls = std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap();
        if enabled {
            let logcat = calls.find("logcat").unwrap();
            let dropbox = calls.find("dropbox").unwrap();
            let packages = calls.find("packages").unwrap();
            let broadcasts = calls.find("broadcasts").unwrap();
            assert!(logcat < dropbox && dropbox < broadcasts && broadcasts < packages);
            let events: crate::process::Exchange = crate::process::load_exchange(&directory.join("service-events/exchange.json")).unwrap().unwrap();
            assert_eq!(events.exit_code, Some(13));
            assert_eq!(events.stderr, b"Permission denied\n");
            let anr: crate::process::Exchange = crate::process::load_exchange(&directory.join("system-app-anr/exchange.json")).unwrap().unwrap();
            assert_eq!(anr.stdout, b"Permission Denial: cannot dump DropBoxManagerService\n");
            let commands: Vec<DiagnosticCommand> = load(&directory.join("commands.json")).unwrap().unwrap();
            assert_eq!(commands.len(), 3);
            let queues: crate::process::Exchange = crate::process::load_exchange(&directory.join("broadcasts/exchange.json")).unwrap().unwrap();
            assert!(queues.failure.is_none());
            assert!(queues.stdout.len() > 1024 * 1024);
            assert!(String::from_utf8(queues.stdout).unwrap().contains("com.google.android.apps.messaging"));
            assert!(commands[0].failure.is_some());
        } else {
            assert!(!calls.contains("logcat") && !calls.contains("dropbox") && !calls.contains("broadcasts"));
        }
        assert!(device.stop().passed);
    }
}

#[test]
fn startup_diagnostic_cancellation_retires_without_installing_fixture() {
    let mut f = Fixture::view(false);
    f.config.startup_diagnostics = true;
    std::fs::write(f.root().join("startup-block"), b"block first diagnostic").unwrap();
    let cancelled = AtomicBool::new(false);
    let error = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !f.root().join("startup-started").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::Release);
        });
        match f.acquire(&cancelled) { Ok(_) => panic!("cancelled acquisition succeeded"), Err(error) => error }
    });
    assert!(error.cause.contains("cancel"), "{error}");
    assert!(error.retirement.unwrap().passed);
    let directory = f.root().join("session/startup-observation");
    let exchange: crate::process::Exchange = crate::process::load_exchange(&directory.join("service-events/exchange.json")).unwrap().unwrap();
    assert!(exchange.failure.unwrap().contains("cancelled"));
    assert!(exchange.cleanup_failure.is_none());
    assert!(!directory.join("system-app-anr").exists());
    assert!(!std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap().contains("install"));
    assert!(!f.root().join("session/acquisition-observation").exists());
    f.retired();
}


#[test]
fn passive_queue_sampling_is_bounded_and_precedes_package_work() {
    let mut invalid = Fixture::view(false);
    invalid.config.startup_broadcast_sample_ms = 30_001;
    assert!(invalid.acquire(&AtomicBool::new(false)).is_err());
    assert!(!invalid.root().join("session").exists());
    assert!(!invalid.root().join("emulator-argv.json").exists());
    let mut f = Fixture::view(false);
    f.config.startup_broadcast_sample_ms = 800;
    let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
    let directory = f.root().join("session/startup-broadcasts");
    let samples: Vec<DiagnosticCommand> = load(&directory.join("samples.json")).unwrap().unwrap();
    assert!(!samples.is_empty());
    assert!(directory.join("finished.json").is_file());
    for sample in samples {
        let exchange: crate::process::Exchange = crate::process::load_exchange(&directory.join(sample.name).join("exchange.json")).unwrap().unwrap();
        assert!(exchange.cleanup_failure.is_none());
    }
    let calls = std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap();
    assert!(calls.rfind("broadcasts").unwrap() < calls.find("packages").unwrap());
    assert!(!calls.contains("wait-for-broadcast") && !calls.contains("flush"));
    assert!(device.stop().passed);
}

#[test]
fn passive_queue_sampling_cancellation_preserves_exchange_and_retires() {
    let mut f = Fixture::view(false);
    f.config.startup_broadcast_sample_ms = 30_000;
    std::fs::write(f.root().join("queue-sample-block"), b"block observer").unwrap();
    let cancelled = AtomicBool::new(false);
    let error = std::thread::scope(|scope| {
        scope.spawn(|| {
            let end = Instant::now() + Duration::from_secs(5);
            while !f.root().join("queue-sample-started").exists() {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::Release);
        });
        match f.acquire(&cancelled) { Ok(_) => panic!("cancelled sampling succeeded"), Err(error) => error }
    });
    assert!(error.cause.contains("cancel"));
    assert!(error.retirement.unwrap().passed);
    let directory = f.root().join("session/startup-broadcasts");
    let samples: Vec<DiagnosticCommand> = load(&directory.join("samples.json")).unwrap().unwrap();
    assert_eq!(samples.len(), 1);
    assert!(samples[0].failure.as_ref().unwrap().contains("cancel"));
    assert!(!directory.join("finished.json").exists());
    assert!(!std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap().contains("install"));
    f.retired();
}


#[test]
fn broadcast_sample_interval_is_retained_and_does_not_extend_window() {
    let mut f = Fixture::view(false);
    let mut legacy = serde_json::to_value(&f.config).unwrap();
    legacy.as_object_mut().unwrap().remove("startup_broadcast_interval_ms");
    let decoded: AndroidLaunch = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.startup_broadcast_interval_ms, 250);
    for interval in [0, 249, 30_001] {
        f.config.startup_broadcast_interval_ms = interval;
        assert!(f.acquire(&AtomicBool::new(false)).is_err());
        assert!(!f.root().join("session").exists());
        assert!(!f.root().join("emulator-argv.json").exists());
    }
    f.config.startup_broadcast_sample_ms = 500;
    f.config.startup_broadcast_interval_ms = 30_000;
    let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
    let directory = f.root().join("session/startup-broadcasts");
    let samples: Vec<DiagnosticCommand> = load(&directory.join("samples.json")).unwrap().unwrap();
    assert_eq!(samples.len(), 1);
    let elapsed: u128 = load(&directory.join("finished.json")).unwrap().unwrap();
    // The 30-second interval must not keep this half-second window running for 30 seconds.
    assert!(elapsed < 8_000);
    let saved: AndroidLaunch = load(&f.root().join("session/launch.json")).unwrap().unwrap();
    assert_eq!(saved.startup_broadcast_interval_ms, 30_000);
    assert!(device.stop().passed);
}





#[test]
fn resource_read_uses_remaining_acquisition_budget() {
    let f = Fixture::view(false);
    std::fs::write(f.root().join("resource-delay"), b"2.2").unwrap();
    let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
    assert_eq!(std::fs::read_to_string(f.root().join("session/cpu-online.txt")).unwrap().trim(), "0-3");
    assert!(device.stop().passed);
    f.retired();
}

#[test]
fn resource_read_cancellation_retires_without_installing() {
    let f = Fixture::view(false);
    std::fs::write(f.root().join("resource-delay"), b"30").unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !f.root().join("resource-started").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancelled.store(true, Ordering::Release);
        });
        let Err(error) = f.acquire(&cancelled) else { panic!("cancelled acquisition succeeded") };
        assert!(error.cause.contains("cancelled"), "{error:?}");
        assert!(error.retirement.unwrap().passed);
    });
    f.retired();
    assert!(!std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap().contains("install"));
    assert!(!f.root().join("session/ready.json").exists());
}

#[test]
fn resource_read_does_not_extend_acquisition_deadline() {
    let f = Fixture::view(false);
    std::fs::write(f.root().join("resource-delay"), b"30").unwrap();
    let error = Device::acquire_at(f.config.clone(), &f.root().join("session"),
        Instant::now() + Duration::from_secs(3), &AtomicBool::new(false),
        &f.root().join("host"));
    let Err(error) = error else { panic!("expired acquisition succeeded") };
    assert!(f.root().join("resource-started").exists());
    assert!(error.cause.contains("deadline"), "{error:?}");
    assert!(error.retirement.unwrap().passed);
    f.retired();
    assert!(!std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap().contains("install"));
    assert!(!f.root().join("session/ready.json").exists());
}


#[test]
fn acquisition_failure_retains_service_evidence_without_replacing_primary_error() {
    let mut f = Fixture::view(false);
    f.config.startup_diagnostics = true;
    std::fs::write(f.root().join("install-fail"), b"1").unwrap();
    std::fs::write(f.root().join("service-denied"), b"1").unwrap();
    let Err(error) = f.acquire(&AtomicBool::new(false)) else { panic!("install unexpectedly succeeded") };
    assert_eq!(error.cause, "bridge exited unsuccessfully");
    assert!(error.retirement.unwrap().passed);
    let root=f.root().join("session/acquisition-observation");
    let commands: Vec<DiagnosticCommand> = load(&root.join("commands.json")).unwrap().unwrap();
    assert_eq!(commands.len(), 3);
    assert!(commands[0].failure.as_ref().unwrap().contains("unsuccessfully"));
    let events: crate::process::Exchange = crate::process::load_exchange(&root.join("service-events/exchange.json")).unwrap().unwrap();
    assert!(String::from_utf8(events.stdout).unwrap().contains("AndroidRuntime: retained fixture crash"));
    assert!(root.join("system-app-anr/exchange.json").is_file());
    assert!(root.join("broadcasts/exchange.json").is_file());
    let calls=std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap();
    assert_eq!(calls.lines().filter(|line| line.starts_with("[\"install\"")).count(),1);
    assert!(!f.root().join("session/ready.json").exists());
    f.retired();
}


#[test]
fn view_profile_acquires_selected_app_and_retires_on_capture_failure() {
    for wrong_window in [false, true] {
        let mut f=Fixture::view(wrong_window);
        if !wrong_window {
            f.config.view_app.as_mut().unwrap().launch_extras.insert("app.label".into(), "한글 'quote' $(printf injected)".into());
        }
        let cancel=AtomicBool::new(false);
        let result=f.acquire(&cancel);
        if wrong_window {
            let error=match result { Ok(_)=>panic!("foreign foreground accepted"),Err(e)=>e };
            assert!(error.retirement.unwrap().passed);
            assert!(f.root().join("session/initial-view/capture/progress.json").is_file());
            assert!(!f.root().join("session/view-ready.json").exists());
        } else {
            let mut device=result.unwrap();
            assert!(f.root().join("extras-received").is_file());
            assert_eq!(device.view_ready().unwrap().package,"org.example.client");


            let capture=device.collect_view(&f.root().join("next-capture"),
                Instant::now()+Duration::from_secs(10),&cancel).unwrap();
            assert_eq!(capture.catalog.targets[0].text.as_deref(),Some("Open"));
            assert!(device.stop().passed);
            assert!(device.view_ready().is_none());
        }
        f.retired();
        let calls=std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap();
        assert!(calls.contains("org.example.client/org.example.client.Main"));
        assert!(!calls.contains("org.example.app"));
        assert!(!calls.contains("files/input-control.json"));
        assert!(!calls.contains("files/text-control.json"));
        assert!(!calls.contains("content://org.example.client.clock"));
    }
}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn webview_discovery_uses_owned_app_process_without_consumer_controllers() {
    for (changed, forwarding) in [(false, "ok"), (true, "ok"), (false, "lost"), (false, "foreign"), (false, "reverse-lost")] {
        let mut f = Fixture::new();
        f.config.webview_app = Some(crate::android_webview::Profile {
            package: "org.example.app".into(), activity: "org.example.app.ClientActivity".into(),
            local_service: Some(crate::android_webview::LocalService {device_port:18763,host_port:18763}),
        });
        // These unused source-adapter programs must never be invoked for external discovery.
        f.config.python = "/missing-python".into();
        f.config.bridge_script = "/missing-bridge".into();
        let source = std::fs::read_to_string(&f.config.adb).unwrap();
        let point = "if args == ['shell', 'pidof', 'org.example.app']:";
        let insert = format!(r#"
if args==['reverse','--no-rebind','tcp:18763','tcp:18763']:
 (root/'reverse.json').write_text(json.dumps(['guest','tcp:18763','tcp:18763']));sys.exit(1 if '{forwarding}'=='reverse-lost' else 0)
if args==['reverse','--list']:
 if (root/'reverse.json').exists():print(' '.join(json.loads((root/'reverse.json').read_text())))
 sys.exit(0)
if args==['reverse','--remove','tcp:18763']:
 (root/'reverse.json').unlink();sys.exit(0)
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:
 assert (root/'reverse.json').exists();sys.exit(0)
if args[:2]==['forward','--no-rebind']:
 mapping=['emulator-9998' if '{forwarding}'=='foreign' else 'emulator-{port}',args[2],args[3]]
 (root/'forward.json').write_text(json.dumps(mapping))
 sys.exit(1 if '{forwarding}' in ['lost','foreign'] else 0)
if args==['forward','--list']:
 if (root/'forward.json').exists():print(' '.join(json.loads((root/'forward.json').read_text())))
 sys.exit(0)
if args[:2]==['forward','--remove']:
 (root/'forward.json').unlink();(root/'forward-removed').touch();sys.exit(0)
if args==['shell','pidof','org.example.app']: print('123');sys.exit(0)
if args==['shell','cat','/proc/123/stat']:
 count=root/'stat-count'; n=int(count.read_text())+1 if count.exists() else 1;count.write_text(str(n))
 fields=['S']+['0']*18+[str(12346 if {changed} and n>1 else 12345)]
 print('123 (consumer (webview)) '+' '.join(fields));sys.exit(0)
if args==['shell','cat','/proc/net/unix']:
 print('0: 0 0 0 0 0 1 @webview_devtools_remote_999')
 print('0: 0 0 0 0 0 2 @webview_devtools_remote_123');sys.exit(0)
"#, changed = if changed { "True" } else { "False" }, forwarding=forwarding, port=f.config.port);
        std::fs::write(&f.config.adb, source.replace(point, &(insert + point))).unwrap();
        match f.acquire(&AtomicBool::new(false)) {
            Ok(mut device) => {
                assert!(!changed);
                let debugger = device.webview_debugger().unwrap();
                assert_eq!(debugger.pid, 123);
                assert_eq!(debugger.process_start_ticks, 12345);
                assert_eq!(debugger.socket, "webview_devtools_remote_123");

                assert!(device.view_ready().is_none());

                let connection = device.connect_webview_debugger(Instant::now()+Duration::from_secs(4), &AtomicBool::new(false));
                if forwarding == "ok" {
                    assert!(connection.is_ok(), "{connection:?}");
                    let curl = f.root().join("curl");
                    std::fs::write(&curl, r#"#!/usr/bin/env python3
import json,sys
from urllib.parse import urlsplit
url=urlsplit(sys.argv[-1]);assert url.hostname=='127.0.0.1' and url.path=='/json/list'
assert '--noproxy' in sys.argv and '--location' not in sys.argv
print(json.dumps([dict(id='page-one',type='page',url='about:blank',title='host',webSocketDebuggerUrl='ws://127.0.0.1:'+str(url.port)+'/devtools/page/page-one')]))
"#).unwrap();
                    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
                    let pages = device.inspect_webview_pages(&curl, &f.root().join("pages"),
                        Instant::now()+Duration::from_secs(4), &AtomicBool::new(false)).unwrap();
                    assert_eq!(pages.len(), 1);
                    assert_eq!(pages[0].id, "page-one");
                    assert!(f.root().join("pages/pages.json").exists());
                    use futures_util::{SinkExt,StreamExt};
                    use tokio_tungstenite::tungstenite::Message;
                    let listener=tokio::net::TcpListener::bind(("127.0.0.1",connection.unwrap())).await.unwrap();
                    let sent=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));let count=sent.clone();
                    let timer_effects=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));let timer_count=timer_effects.clone();
                    let server=tokio::spawn(async move {
                        let mut last=None;
                        for connection in 0..3 {
                            let (stream,_)=listener.accept().await.unwrap();let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();
                            while let Some(Ok(Message::Text(text)))=socket.next().await {
                                let request:serde_json::Value=serde_json::from_str(text.as_str()).unwrap();
                                if request["method"]=="Emulation.setVirtualTimePolicy" {
                                    timer_count.fetch_add(1,Ordering::Relaxed);
                                    socket.send(Message::Text(serde_json::json!({"id":request["id"],"result":{"virtualTimeTicksBase":123.0}}).to_string().into())).await.unwrap();
                                    if request["params"]["budget"]==200 {break;} // Timer ack arrived; completion was lost.
                                    if request["params"]["policy"]=="advance" {socket.send(Message::Text(serde_json::json!({"method":"Emulation.virtualTimeBudgetExpired","params":{}}).to_string().into())).await.unwrap();}
                                    continue;
                                }
                                assert_eq!(request["method"],"Runtime.evaluate");
                                let expression=request["params"]["expression"].as_str().unwrap();
                                let dispatch=expression.contains(".dispatch(");
                                if dispatch {
                                    count.fetch_add(1,Ordering::Relaxed);
                                    let frame:serde_json::Value=serde_json::from_str(expression.strip_prefix("JSON.stringify(globalThis.__frontcapFrames.dispatch(").unwrap().strip_suffix("))").unwrap()).unwrap();
                                    last=Some(serde_json::json!({"request":frame,"state":"pending","result":null,"error":null}));
                                    if connection==0 {break;} // Effect admitted, transport reply lost.
                                } else if let Some(value)=&mut last {
                                    value["state"]="completed".into();value["result"]=serde_json::json!({"timestamp":value["request"]["timestamp"]});
                                }
                                let receipt=serde_json::json!({"document":"abcd0000-0000-0000-0000-000000000001","legacyUsed":false,"last":last});
                                socket.send(Message::Text(serde_json::json!({"id":request["id"],"result":{"result":{"type":"string","value":receipt.to_string()}}}).to_string().into())).await.unwrap();
                            }
                        }
                    });
                    let deadline=Instant::now()+Duration::from_secs(15);let cancelled=AtomicBool::new(false);
                    let id=|value|frontcap_core::ids::RequestId::new(value).unwrap();
                    let directory=f.root().join("page");
                    let frame;
                    {
                        let mut page=device.connect_webview_page("page-one",&directory,deadline,&cancelled).await.unwrap();
                        let initial=page.inspect_frames(&id("initial"),deadline,&cancelled).await.unwrap().unwrap();
                        page.control_timers(&id("pause"),&crate::android_timers::Action::Pause,deadline,&cancelled).await.unwrap();
                        frame=crate::android_frames::Request{document:initial.document,id:1,timestamp:10.0};
                        assert!(page.dispatch_frame(&id("frame"),&frame,deadline,&cancelled).await.is_err());
                        assert!(page.dispatch_frame(&id("frame"),&frame,deadline,&cancelled).await.is_err());
                        assert_eq!(sent.load(Ordering::Relaxed),1);
                    }
                    let saved_owner:crate::android_webview::Debugger=load(&directory.join("connection-owner.json")).unwrap().unwrap();
                    let mut foreign=saved_owner.clone();foreign.pid+=1;
                    save_atomic(&directory.join("connection-owner.json"),&foreign).unwrap();
                    assert!(device.resume_webview_page(&directory,&id("foreign"),deadline,&cancelled).await.is_err());
                    save_atomic(&directory.join("connection-owner.json"),&saved_owner).unwrap();
                    {
                        let mut page=device.resume_webview_page(&directory,&id("reconnect"),deadline,&cancelled).await.unwrap();
                        let next=crate::android_frames::Request{id:2,timestamp:20.0,..frame.clone()};
                        assert!(page.dispatch_frame(&id("early"),&next,deadline,&cancelled).await.is_err());
                        page.inspect_frames(&id("initial"),deadline,&cancelled).await.unwrap();
                        assert!(page.dispatch_frame(&id("still-early"),&next,deadline,&cancelled).await.is_err());
                        let completed=page.inspect_frames(&id("recovered"),deadline,&cancelled).await.unwrap().unwrap();
                        assert_eq!(completed.last.unwrap().state,crate::android_frames::State::Completed);
                        assert_eq!(sent.load(Ordering::Relaxed),1);
                        assert!(page.dispatch_frame(&id("needs-pause"),&next,deadline,&cancelled).await.is_err());
                        assert!(page.control_timers(&id("advance-before-pause"),&crate::android_timers::Action::Advance{milliseconds:100},deadline,&cancelled).await.is_err());
                        page.control_timers(&id("pause-again"),&crate::android_timers::Action::Pause,deadline,&cancelled).await.unwrap();
                        let advanced=page.control_timers(&id("advance"),&crate::android_timers::Action::Advance{milliseconds:100},deadline,&cancelled).await.unwrap();
                        assert!(advanced.budget_expired);
                        let pending=page.dispatch_frame(&id("next-frame"),&next,deadline,&cancelled).await.unwrap();
                        assert_eq!(pending.last.unwrap().state,crate::android_frames::State::Pending);
                        page.inspect_frames(&id("next-completed"),deadline,&cancelled).await.unwrap();
                        assert!(page.control_timers(&id("lost-budget"),&crate::android_timers::Action::Advance{milliseconds:200},deadline,&cancelled).await.is_err());
                        let cached=page.dispatch_frame(&id("next-frame"),&next,deadline,&cancelled).await.unwrap();
                        assert_eq!(cached.last.unwrap().state,crate::android_frames::State::Pending);
                        assert_eq!(sent.load(Ordering::Relaxed),2);
                    }
                    {
                        let mut page=device.resume_webview_page(&directory,&id("after-lost-budget"),deadline,&cancelled).await.unwrap();
                        page.inspect_frames(&id("after-lost-budget-read"),deadline,&cancelled).await.unwrap();
                        assert!(page.control_timers(&id("do-not-overwrite"),&crate::android_timers::Action::Pause,deadline,&cancelled).await.is_err());
                        assert!(page.dispatch_frame(&id("blocked-by-budget"),&crate::android_frames::Request{id:3,timestamp:30.0,..frame.clone()},deadline,&cancelled).await.is_err());
                        let original=page.control_timers(&id("advance"),&crate::android_timers::Action::Advance{milliseconds:100},deadline,&cancelled).await.unwrap();
                        assert!(original.budget_expired);
                        assert_eq!(timer_effects.load(Ordering::Relaxed),4);
                    }
                    let other=f.root().join("other-journal");std::fs::create_dir(&other).unwrap();
                    assert!(device.claim_webview_control("page-one",&other).is_err());
                    server.abort();let _=server.await;
                    // The guest can disappear before Stop; host forwarding still needs removal.
                    assert_eq!(unsafe { libc::kill(device.process.as_ref().unwrap().id() as i32, libc::SIGTERM) }, 0);
                    let retirement = device.stop();
                    assert!(retirement.passed, "{retirement:?}");
                } else {
                    assert!(connection.unwrap_err().retirement.unwrap().passed);
                }
                assert!(device.webview_debugger().is_none());
                assert_eq!(f.root().join("forward-removed").exists(), forwarding != "foreign");
                assert_eq!(f.root().join("forward.json").exists(), forwarding == "foreign");
            }
            Err(error) => {
                assert!(changed || forwarding=="reverse-lost", "{error}");
                if changed {assert!(error.cause.contains("process changed"));}
                assert!(error.retirement.unwrap().passed);
                assert!(!f.root().join("session/webview-debugger.json").exists());
            }
        }
        f.retired();
        let calls = std::fs::read_to_string(f.root().join("adb-calls.jsonl")).unwrap();
        assert!(!calls.contains("run-as"));
        assert!(!calls.contains("clock-control"));
        assert!(!calls.contains("content"));
        assert_eq!(calls.lines().filter(|line| line.contains("--no-rebind") && line.contains("forward")).count(), usize::from(!changed && forwarding!="reverse-lost"));
        assert!(f.root().join("session/webview-reverse-retired.json").exists());
        if changed || forwarding != "ok" {assert!(!f.root().join("reverse.json").exists());}
    }
}

#[test]
fn native_host_capture_precedes_webview_and_survives_its_removal_but_not_process_replacement() {
    let mut f = Fixture::new();
    f.config.artifact_budget_bytes = Some(48);
    f.config.host_app = Some(crate::android_webview::Profile {
        package: "org.example.app".into(), activity: "org.example.app.ClientActivity".into(), local_service: None,
    });
    f.config.python = "/missing-python".into();
    f.config.bridge_script = "/missing-bridge".into();
    let source = std::fs::read_to_string(&f.config.adb).unwrap();
    let point = "if args == ['shell', 'pidof', 'org.example.app']:";
    let insert = r#"
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:sys.exit(0)
if args==['shell','pidof','org.example.app']:print('123');sys.exit(0)
if args==['shell','cat','/proc/123/stat']:
 print('123 (app) '+' '.join(['S']+['0']*18+['54321' if (root/'replaced').exists() else '12345']));sys.exit(0)
if args==['shell','cat','/proc/net/unix']:
 (root/'debugger-queried').touch()
 if (root/'webview').exists():print('0: 0 0 0 0 0 1 @webview_devtools_remote_123')
 sys.exit(0)
if args==['exec-out','screencap','-p']:
 sys.stdout.buffer.write(b'\x89PNG\r\n\x1a\n'+(13).to_bytes(4,'big')+b'IHDR'+(100).to_bytes(4,'big')+(80).to_bytes(4,'big'));sys.exit(0)
"#;
    std::fs::write(&f.config.adb, source.replace(point, &(insert.to_owned()+point))).unwrap();
    let cancel = AtomicBool::new(false);
    let mut device = f.acquire(&cancel).unwrap();
    let deadline = Instant::now()+Duration::from_secs(10);
    let owner = device.app_process().unwrap().clone();
    assert!(device.webview_debugger().is_none());
    assert!(!f.root().join("debugger-queried").exists());
    device.capture_app_png(&f.root().join("catalog"),deadline,&cancel).unwrap();
    std::fs::write(f.root().join("webview"),b"").unwrap();
    device.discover_webview_debugger(deadline,&cancel).unwrap();
    assert_eq!(device.webview_debugger().unwrap().process(),owner);
    std::fs::remove_file(f.root().join("webview")).unwrap();
    device.capture_app_png(&f.root().join("results"),deadline,&cancel).unwrap();
    let exhausted = f.root().join("exhausted");
    assert!(device.capture_app_png(&exhausted, deadline, &cancel).unwrap_err().to_string().contains("budget unavailable"));
    assert!(!exhausted.exists());
    std::fs::write(f.root().join("replaced"),b"").unwrap();
    assert!(device.capture_app_png(&f.root().join("stale"),deadline,&cancel).is_err());
    assert!(device.discover_webview_debugger(deadline,&cancel).is_err());
    assert!(!f.root().join("stale").exists());
    assert!(device.stop().passed);
    assert!(device.app_process().is_none());
    f.retired();
}

#[test]
fn external_contact_driver_is_frozen_owned_and_retired_on_startup_failure() {
    contact_driver_cases(&["ok", "wrong-hash", "foreign-ready", "wrong-capacity"]);
}

fn contact_driver_cases(modes: &[&str]) {
    for &mode in modes {
        let mut f = Fixture::new();
        let emulator = std::fs::read_to_string(&f.config.emulator).unwrap();
        std::fs::write(&f.config.emulator, emulator.replace("time.monotonic()+30", "time.monotonic()+120")).unwrap();
        f.config.webview_app = Some(crate::android_webview::Profile {
            package: "org.example.app".into(), activity: "org.example.app.ClientActivity".into(),
            local_service: None,
        });
        let jar = f.root().join("input.jar");
        std::fs::write(&jar, b"frozen-driver").unwrap();
        let hash = frontcap_core::canonical::sha256_bytes_hex(b"frozen-driver");
        let source = std::fs::read_to_string(&f.config.adb).unwrap();
        let point = "if args == ['shell', 'pidof', 'org.example.app']:";
        let insert = format!(r#"
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:sys.exit(0)
if args==['shell','pidof','org.example.app']:print('123');sys.exit(0)
if args[:2]==['shell','cat'] and args[2] in ['/proc/123/stat','/proc/456/stat']:
 pid=args[2].split('/')[2];print(pid+' (owned) '+' '.join(['S']+['0']*18+['12345']));sys.exit(0)
if args==['shell','cat','/proc/net/unix']:
 print('0: 0 0 0 0 0 1 @webview_devtools_remote_123');sys.exit(0)
if args[:2]==['shell','mkdir']:sys.exit(0)
if args[:1]==['push']:
 assert Path(args[1]).read_bytes()==b'frozen-driver';sys.exit(0)
if args[:2]==['shell','sha256sum']:print('{hash}  '+args[2]);sys.exit(0)
if args[:2]==['shell','-T']:
 assert 'app_process' in args[2] and 'TouchInput' in args[2];print('driver stdout',flush=True);print('driver stderr',file=sys.stderr,flush=True);time.sleep(120);sys.exit(0)
if args[:2]==['shell','cat'] and args[2].endswith('/journal/ready.json'):
 print(json.dumps(dict(status='ready',generation='abcd0000-0000-0000-0000-000000000001',pid=456,uid={uid},request_capacity={capacity})));sys.exit(0)
"#, uid=if mode=="foreign-ready" {0} else {2000}, capacity=if mode=="ok" {26} else if mode=="wrong-capacity" {25} else {24});
        let input = r#"
if args==['shell','dumpsys','window']:
 print('mCurrentFocus=Window{abc u0 '+('foreign/.Main' if (root/'foreign-focus').exists() else 'org.example.app/.ClientActivity')+'}');sys.exit(0)
if args==['exec-out','screencap','-p']:
 sys.stdout.buffer.write(b'\x89PNG\r\n\x1a\n'+(13).to_bytes(4,'big')+b'IHDR'+(100).to_bytes(4,'big')+(80).to_bytes(4,'big'));sys.exit(0)
if args[:1]==['push'] and args[2].endswith('.pending'):
 (root/'contact-request.json').write_bytes(Path(args[1]).read_bytes());sys.exit(0)
if args[:2]==['shell','mv']:
 request=json.loads((root/'contact-request.json').read_text())
 assert request['contacts']==[dict(id=1,x=49.5,y=39.5)]
 (root/'contact-accepted.json').write_text(json.dumps(request))
 (root/'contact-reply.json').write_text(json.dumps(dict(generation=request['generation'],request_id=request['request_id'],status='injected',contact_revision=request['expected_revision']+1,contacts=request['contacts'],frames=[dict(accepted=True)])))
 with (root/'contact-publications').open('a') as log:log.write('sent\n')
 sys.exit(1) # Native effect/result exists, but publication acknowledgement was lost.
if args[:2]==['shell','cat'] and '/accepted-' in args[2]:
 print((root/'contact-accepted.json').read_text());sys.exit(0)
if args[:2]==['shell','cat'] and '/reply-' in args[2]:
 print((root/'contact-reply.json').read_text());sys.exit(0)
"#;
        // Input-specific push handling must precede the artifact push handler.
        std::fs::write(&f.config.adb,source.replace(point,&(input.to_owned()+&insert+point))).unwrap();
        let mut device=f.acquire(&AtomicBool::new(false)).unwrap();
        let result=if mode=="ok" {
            device.start_contact_driver_bounded(&jar,&hash,ContactDriverLimits { lifetime: Duration::from_secs(900), request_capacity: 26 },
                Instant::now()+Duration::from_secs(5),&AtomicBool::new(false))
        } else {
            device.start_contact_driver(&jar,if mode=="wrong-hash" {"wrong"} else {&hash},
                Instant::now()+Duration::from_secs(5),&AtomicBool::new(false))
        };
        if mode=="ok" {
            let owner=result.unwrap();
            assert_eq!(owner.pid,456);
            assert_eq!(owner.lifetime_ms,900_000);
            assert_eq!(owner.request_capacity,26);
            assert_eq!(owner.process_start_ticks,12345);
            assert_eq!(owner.jar_sha256,hash);
            use frontcap_core::{ids::RequestId,native::contacts::{Contact,Operation}};
            let id=|s:&str|RequestId::new(s).unwrap();
            let deadline=Instant::now()+Duration::from_secs(20);
            let cancelled=AtomicBool::new(false);
            let capture=device.capture_contacts(&id("first"),deadline,&cancelled).unwrap();
            assert_eq!((capture.width,capture.height),(100,80));
            let down=Operation::Set{expected_revision:0,contacts:vec![Contact{id:1,x:0.5,y:0.5}]};
            assert!(device.input_contacts(&id("down"),&id("first"),&down,deadline,&cancelled).is_err());
            assert!(device.input_contacts(&id("next"),&id("first"),&down,deadline,&cancelled).is_err());
            assert!(device.capture_contacts(&id("during-pending"),deadline,&cancelled).is_err());
            assert!(device.transition_webview(&id("home-pending"),WebViewLifecycle::Home,deadline,&cancelled).is_err());
            let reply_path=f.root().join("contact-reply.json");
            let original=std::fs::read(&reply_path).unwrap();
            let mut wrong:serde_json::Value=serde_json::from_slice(&original).unwrap();
            wrong["contact_revision"]=serde_json::json!(99);
            std::fs::write(&reply_path,serde_json::to_vec(&wrong).unwrap()).unwrap();
            assert!(device.reconcile_contacts(&id("down"),deadline,&cancelled).is_err());
            std::fs::write(&reply_path,original).unwrap();
            let receipt=device.reconcile_contacts(&id("down"),deadline,&cancelled).unwrap();
            assert_eq!(receipt["contact_revision"],1);
            assert!(device.transition_webview(&id("home-held"),WebViewLifecycle::Home,deadline,&cancelled).is_err());
            assert!(!device.directory.join("webview-lifecycle").exists());
            assert_eq!(device.input_contacts(&id("down"),&id("first"),&down,deadline,&cancelled).unwrap(),receipt);
            let release=Operation::Set{expected_revision:1,contacts:vec![]};
            assert!(device.input_contacts(&id("down"),&id("first"),&release,deadline,&cancelled).is_err());
            assert!(device.input_contacts(&id("stale"),&id("first"),&release,deadline,&cancelled).is_err());
            device.capture_contacts(&id("second"),deadline,&cancelled).unwrap();
            std::fs::write(f.root().join("foreign-focus"),b"").unwrap();
            assert!(device.input_contacts(&id("foreign"),&id("second"),&release,deadline,&cancelled).is_err());
            std::fs::remove_file(f.root().join("foreign-focus")).unwrap();
            assert!(device.input_contacts(&id("returned-focus"),&id("second"),&release,deadline,&cancelled).is_err());
            device.capture_contacts(&id("third"),deadline,&cancelled).unwrap();
            device.invalidate_contact_capture();
            assert!(device.input_contacts(&id("invalidated"),&id("third"),&release,deadline,&cancelled).is_err());
            assert_eq!(std::fs::read_to_string(f.root().join("contact-publications")).unwrap(),"sent\n");
            // A complete journey crosses the old 24-request ceiling. Each lost reply is
            // reconciled without replay, and the declared capacity rejects the next effect.
            let deadline=Instant::now()+Duration::from_secs(90);
            for revision in 1..26 {
                let capture_id=id(&format!("capacity-capture-{revision}"));
                let input_id=id(&format!("capacity-input-{revision}"));
                device.capture_contacts(&capture_id,deadline,&cancelled).unwrap();
                let operation=Operation::Set{expected_revision:revision,contacts:vec![Contact{id:1,x:0.5,y:0.5}]};
                assert!(device.input_contacts(&input_id,&capture_id,&operation,deadline,&cancelled).is_err());
                assert_eq!(device.reconcile_contacts(&input_id,deadline,&cancelled).unwrap()["contact_revision"],revision+1);
            }
            device.capture_contacts(&id("exhausted"),deadline,&cancelled).unwrap();
            let operation=Operation::Cancel{expected_revision:26};
            assert!(device.input_contacts(&id("over-capacity"),&id("exhausted"),&operation,deadline,&cancelled).is_err());
            assert_eq!(std::fs::read_to_string(f.root().join("contact-publications")).unwrap().lines().count(),26);
            let pid=device.contact_process.as_ref().unwrap().id();
            assert!(!device.contact_process.as_mut().unwrap().exited().unwrap());
            let retirement = device.stop();
            assert!(retirement.passed, "{retirement:?}");
            assert!(ProcessIdentity::of(pid).unwrap().is_none());
            assert_eq!(device.reconcile_contacts(&id("down"),deadline,&cancelled).unwrap(),receipt);
        } else {
            assert!(result.unwrap_err().retirement.unwrap().passed);
        }
        if mode=="wrong-hash" { assert!(device.contact_process.is_none()); }
        f.retired();
        if std::env::var_os("FRONTCAP_MANAGED_EMULATOR_TEST").is_some() {
            let contact = device.host_evidence_directory().join("contact-driver");
            for (path, expected) in [(contact.join("stdout.log"), b"driver stdout\n".as_slice()),
                (contact.join("stderr.log"), b"driver stderr\n".as_slice()),
                (f.root().join("session/emulator.log"), b"".as_slice())] {
                let root = path.with_file_name(format!("{}.diagnostics", path.file_name().unwrap().to_str().unwrap()));
                let directory = std::fs::read_dir(root).unwrap().next().unwrap().unwrap().path();
                let deadline = Instant::now() + Duration::from_secs(10);
                let result: crate::diagnostic_log::Summary = loop {
                    if let Some(result) = load(&directory.join("result.json")).unwrap() { break result; }
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(result.eof && result.error.is_none());
                assert_eq!(std::fs::read(path).unwrap(), expected);
                let collector: ProcessIdentity = load(&directory.join("ready.json")).unwrap().unwrap();
                while !collector.has_exited().unwrap() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[test]
fn webview_host_transitions_preserve_owner_and_never_replay_uncertain_effects() {
    use frontcap_core::ids::RequestId;
    for mode in ["ok", "lost", "replaced"] {
        let mut f=Fixture::new();
        f.config.webview_app=Some(crate::android_webview::Profile {
            package:"org.example.app".into(),activity:"org.example.app.ClientActivity".into(),local_service:None,
        });
        let source=std::fs::read_to_string(&f.config.adb).unwrap();
        let point="if args in [['shell','input','keyevent','KEYCODE_WAKEUP']";
        let insert=format!(r#"
if args==['shell','am','start','-n','org.example.app/org.example.app.ClientActivity']:sys.exit(0)
if args==['shell','pidof','org.example.app']:print('123');sys.exit(0)
if args==['shell','cat','/proc/123/stat']:
 tick=12346 if '{mode}'=='replaced' and (root/'home').exists() else 12345
 print('123 (owned) '+' '.join(['S']+['0']*18+[str(tick)]));sys.exit(0)
if args==['shell','cat','/proc/net/unix']:
 print('0: 0 0 0 0 0 1 @webview_devtools_remote_123');sys.exit(0)
if args==['shell','dumpsys','window']:
 if (root/'home').exists() and not (root/'focus-gap').exists():
  (root/'focus-gap').touch();print('mCurrentFocus=null');sys.exit(0)
 component='dev.frontcap.launcher/.Home' if (root/'home').exists() else 'org.example.app/.ClientActivity'
 print('mCurrentFocus=Window{{abc u0 '+component+'}}');sys.exit(0)
if args==['shell','input','keyevent','KEYCODE_HOME']:
 (root/'home').touch()
 with (root/'transitions').open('a') as log:log.write('home\n')
 sys.exit(1 if '{mode}'=='lost' else 0)
if args==['shell','am','start','-W','-n','org.example.app/org.example.app.ClientActivity']:
 (root/'home').unlink()
 with (root/'transitions').open('a') as log:log.write('activate\n')
 sys.exit(0)
"#);
        std::fs::write(&f.config.adb,source.replace(point,&(insert+point))).unwrap();
        let cancelled=AtomicBool::new(false);
        let mut device=f.acquire(&cancelled).unwrap();
        let deadline=Instant::now()+Duration::from_secs(10);
        let home=RequestId::new("home").unwrap();
        let activate=RequestId::new("activate").unwrap();
        let initial=device.inspect_lifecycle_progress().unwrap();
        assert!(!initial.uncertain && initial.pending.is_none() && initial.last_completed.is_none());
        let result=device.transition_webview(&home,WebViewLifecycle::Home,deadline,&cancelled);
        let command_count=device.command_number;
        let progress=device.inspect_lifecycle_progress().unwrap();
        assert_eq!(device.command_number,command_count);
        assert_eq!(device.inspect_lifecycle_progress(),Some(progress.clone()));
        if mode=="ok" {
            assert!(!progress.uncertain && progress.pending.is_none());
            let completed=progress.last_completed.unwrap();
            assert_eq!(completed.id,home);
            assert_eq!(completed.operation,WebViewLifecycle::Home);
            assert_eq!(completed.receipt.as_ref(),result.as_ref().ok());
            let first=result.unwrap();
            assert_eq!(first.focused_component,"dev.frontcap.launcher/dev.frontcap.launcher.Home");
            let returned=device.transition_webview(&activate,WebViewLifecycle::Activate,deadline,&cancelled).unwrap();
            assert_eq!(first.owner,returned.owner);
            assert_eq!(returned.focused_component,"org.example.app/org.example.app.ClientActivity");
            assert_eq!(device.transition_webview(&home,WebViewLifecycle::Home,deadline,&cancelled).unwrap(),first);
            assert!(device.transition_webview(&home,WebViewLifecycle::Activate,deadline,&cancelled).is_err());
            let returned_progress=device.inspect_lifecycle_progress().unwrap();
            assert_eq!(returned_progress.last_completed.as_ref().unwrap().id,activate);
            assert_eq!(returned_progress.last_completed.unwrap().receipt,Some(returned));
            assert_eq!(std::fs::read_to_string(f.root().join("transitions")).unwrap(),"home\nactivate\n");
            assert!(device.stop().passed);
            assert_eq!(device.transition_webview(&home,WebViewLifecycle::Home,deadline,&cancelled).unwrap(),first);
        } else {
            assert!(result.is_err());
            assert!(progress.uncertain && progress.last_completed.is_none());
            let pending=progress.pending.as_ref().unwrap();
            assert_eq!(pending.id,home);
            assert_eq!(pending.operation,WebViewLifecycle::Home);
            assert!(pending.receipt.is_none());
            assert!(device.transition_webview(&home,WebViewLifecycle::Home,deadline,&cancelled).is_err());
            assert!(device.transition_webview(&activate,WebViewLifecycle::Activate,deadline,&cancelled).is_err());
            assert!(device.check_webview_lifecycle().is_err());
            assert_eq!(std::fs::read_to_string(f.root().join("transitions")).unwrap(),"home\n");
            assert!(device.stop().passed);
        }
        f.retired();
    }
}


#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn released_page_transport_survives_selection_and_ends_with_host_owner() {
    use futures_util::{SinkExt,StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f=Fixture::view(false);
    let mut device=f.acquire(&AtomicBool::new(false)).unwrap();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("ws://{}",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {
        let (stream,_)=listener.accept().await.unwrap();
        let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut methods=Vec::new();
        while let Some(Ok(Message::Text(text)))=socket.next().await {
            let request:serde_json::Value=serde_json::from_str(text.as_str()).unwrap();
            methods.push(request["method"].as_str().unwrap().to_owned());
            socket.send(Message::Text(serde_json::json!({"id":request["id"],"result":{}}).to_string().into())).await.unwrap();
        }
        methods
    });
    let debugger=crate::android_webview::Debugger {
        emulator:ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package:"dev.example.consumer".into(),activity:"dev.example.consumer.Main".into(),
        serial:"emulator-5580".into(),pid:123,process_start_ticks:42,socket:"test".into(),
        apk_sha256:"a".repeat(64),source_sha256:"b".repeat(64),
    };
    let client=cdp::Client::connect(&url).await.unwrap();
    let retained={
        let mut page=crate::android_webview_page::Connection::new(&mut device,debugger,
            crate::android_webview::Page {id:"page-one".into(),url:"http://localhost/game".into(),title:"Game".into(),websocket:url},
            client,f.root().join("page"));
        page.retain_transport()
    };
    // A live CDP response proves dropping selection did not explicitly close all client clones.
    tokio::time::timeout(Duration::from_secs(5),retained.call(None,&cdp::protocol::runtime::DisableParams {})).await.unwrap().unwrap();
    drop(retained);
    let methods=tokio::time::timeout(Duration::from_secs(5),server).await.unwrap().unwrap();
    assert_eq!(methods,vec!["Runtime.disable"]); // Release sent no timer, navigation or disposal command.
    assert!(device.stop().passed);
    f.retired();
}


#[test]
fn managed_emulator_logging_entry() {
    if std::env::var_os("FRONTCAP_MANAGED_EMULATOR_TEST").is_none() { return; }
    contact_driver_cases(&["foreign-ready"]);
    let f = Fixture::view(false);
    let emulator = std::fs::read_to_string(&f.config.emulator).unwrap();
    std::fs::write(&f.config.emulator, emulator.replace("sockets=[]",
        "import os\nos.write(1,b'x'*(5*1024*1024))\nPath(__file__).with_name('output-drained').touch()\nsockets=[]")).unwrap();
    let mut device = f.acquire(&AtomicBool::new(false)).unwrap();
    assert!(f.root().join("output-drained").exists());
    let directory = std::fs::read_dir(f.root().join("session/emulator.log.diagnostics"))
        .unwrap().next().unwrap().unwrap().path();
    let collector: ProcessIdentity = load(&directory.join("ready.json")).unwrap().unwrap();
    assert!(!collector.has_exited().unwrap());
    assert!(!directory.join("result.json").exists());
    assert!(device.stop().passed);
    f.retired();
    assert!(!f.root().join("session/emulator-temporary").exists());
    let deadline = Instant::now() + Duration::from_secs(10);
    let result: crate::diagnostic_log::Summary = loop {
        if let Some(result) = load(&directory.join("result.json")).unwrap() { break result; }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(result.eof && result.error.is_none());
    assert_eq!(result.observed_bytes, 5*1024*1024);
    assert_eq!(std::fs::read(f.root().join("session/emulator.log")).unwrap(), vec![b'x'; 4*1024*1024]);
    while !collector.has_exited().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn managed_direct_emulator_output_is_bounded_without_blocking_retirement() {
    let temporary = tempfile::tempdir().unwrap(); let root = temporary.path().canonicalize().unwrap();
    let collector = root.join("collector.sh");
    std::fs::write(&collector, format!("#!/bin/sh\nexport FRONTCAP_DIAGNOSTIC_TEST_DIRECTORY=\"$2\"\nexec '{}' --exact diagnostic_log::tests::entry --nocapture\n",
        std::env::current_exe().unwrap().display())).unwrap();
    std::fs::set_permissions(&collector, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "device::tests::managed_emulator_logging_entry", "--nocapture"])
        .env("FRONTCAP_MANAGED_EMULATOR_TEST", "1")
        .env(crate::diagnostic_log::RECORDER_ENV, &collector).status().unwrap();
    assert!(status.success());
}
