use frontcap_core::state::save_atomic;
use super::*;
use crate::{products::BundleInventory, simulator::tests::Fixture};
use std::{process::Command, time::Duration};
struct Consumer { identity:ProcessIdentity, reaper:Option<std::thread::JoinHandle<()>> }
impl Consumer {
    fn start(executable:&Path) -> Self {
        let mut child=Command::new(executable).arg("60").spawn().unwrap();
        let identity=ProcessIdentity::of(child.id()).unwrap().unwrap();
        Self {identity,reaper:Some(std::thread::spawn(move || {child.wait().unwrap();}))}
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        if !self.identity.has_exited().unwrap() {unsafe {libc::kill(self.identity.pid as i32,libc::SIGTERM);}}
        self.reaper.take().unwrap().join().unwrap();
    }
}
fn end() -> Instant {Instant::now()+Duration::from_secs(10)}
fn fixture(mode:&str) -> (Fixture,Launch,PathBuf,Consumer) {
    let (f,mut config,installed)=prepared::tests::fixture();
    let original=config.prepared.as_ref().unwrap().original.clone();
    let executable=f.root.path().join("entry-consumer");std::fs::copy("/bin/sleep",&executable).unwrap();
    #[cfg(target_os="macos")]
    assert!(Command::new("/usr/bin/codesign").args(["--force","--sign","-"]).arg(&executable).status().unwrap().success());
    for app in [&original,&config.app,&installed] {std::fs::copy(&executable,app.join("Consumer")).unwrap();}
    let info=serde_json::json!({"CFBundleIdentifier":config.bundle,"CFBundleExecutable":"Consumer","CFBundleSupportedPlatforms":["iPhoneSimulator"],"CFBundleURLTypes":[{"CFBundleURLSchemes":["local-consumer"]}]});
    for app in [&original,&config.app,&installed] {plist::to_file_xml(app.join("Info.plist"),&info).unwrap();}
    let prepared=config.prepared.as_mut().unwrap();
    prepared.original_inventory=BundleInventory::capture(&original).unwrap();prepared.inventory=BundleInventory::capture(&config.app).unwrap();
    std::fs::write(f.root.path().join("apps.plist"),format!("{{\"{0}\" = {{CFBundleIdentifier = \"{0}\"; Path = \"{1}\";}};}}",config.bundle,installed.display())).unwrap();
    let script=std::fs::read_to_string(&f.config.xcrun).unwrap()
        .replace("    saved=json.loads((p/'session/target/requests/requests.json').read_text())\n    if 'event_count' in saved:saved=json.loads((p/'session/target/requests/events'/(str(saved['event_count']-1)+'.json')).read_text())\n    assert saved['invalidated_through_ui']==saved['next_ui']-1\n","")
        .replace("elif args[1]=='bootstatus':pass",r#"elif args[1]=='terminate':
    import signal
    os.kill(int((p/'app-pid').read_text()),signal.SIGTERM)
elif args[1]=='bootstatus':pass"#);
    std::fs::write(&f.config.xcrun,script).unwrap();
    if mode=="lost-url" {std::fs::write(f.root.path().join("lose-url"),b"yes").unwrap();}
    let first=Consumer::start(&installed.join("Consumer"));
    std::fs::write(f.root.path().join("app-pid"),first.identity.pid.to_string()).unwrap();
    (f,config,installed,first)
}
fn endpoint(app:&App,pid:u32,generation:&str) {
    let directory=app.container.join("Documents/frontcap-hid");std::fs::create_dir_all(&directory).unwrap();
    save_atomic(&directory.join("endpoint.json"),&contacts::Endpoint {pid,generation:generation.into(),available:true,short_tap: false, observed_contacts:true,request_capacity:4096}).unwrap();
}
#[test]
fn cold_entry_waits_without_relaunching_and_preserves_runner_data_and_original_acceptance() {
    let (f,config,installed,first)=fixture("success");
    let mut runner=crate::xctest::tests::config(&f);runner.app.bundle=config.bundle.clone();
    crate::xctest::tests::prepare_ready_runner(&f,&runner);
    let script=std::fs::read_to_string(&f.config.xcrun).unwrap().replace("pointer=p/('installed-path' if args[4]=='app' else 'container-path')","pointer=p/('runner-container-path' if args[3].endswith('.xctrunner') else ('installed-path' if args[4]=='app' else 'container-path'))");
    std::fs::write(&f.config.xcrun,script).unwrap();
    let script=std::fs::read_to_string(&runner.xcodebuild).unwrap().replace("root/'container-path'","root/'runner-container-path'")
        .replace("'last_id':0","'last_id':0,'system_alerts':True,'system_only':True");
    std::fs::write(&runner.xcodebuild,script).unwrap();
    let mut app=App::launch(crate::simulator_app::tests::acquire(&f),config,&f.root.path().join("app"),end(),&AtomicBool::new(false)).unwrap();
    let ready=app.attach_xctest(runner.into(),end(),&AtomicBool::new(false)).unwrap();
    let marker=app.container.join("saved-result");std::fs::write(&marker,b"retained consumer record").unwrap();
    endpoint(&app,first.identity.pid,"abcd0000-0000-0000-0000-000000000003");
    let root=f.root.path().join("entry");let url="local-consumer://history";
    let begun=app.begin_entry(url,&root,end(),&AtomicBool::new(false)).unwrap();
    assert!(first.identity.has_exited().unwrap());assert!(!ready.process.has_exited().unwrap());assert!(app.entry_pending());
    assert!(app.ensure_running(end(),&AtomicBool::new(false)).is_err());
    assert!(app.begin_entry(url,&f.root.path().join("another"),end(),&AtomicBool::new(false)).is_err());
    for _ in 0..2 {assert_eq!(app.poll_entry(end(),&AtomicBool::new(false)).unwrap(),None);}
    assert!(!root.join("process.json").exists());
    let second=Consumer::start(&installed.join("Consumer"));
    endpoint(&app,second.identity.pid,"abcd0000-0000-0000-0000-000000000004");
    assert_eq!(app.poll_entry(end(),&AtomicBool::new(false)).unwrap(),Some(second.identity.clone()));
    app.ensure_running(end(),&AtomicBool::new(false)).unwrap();
    assert!(!app.entry_pending());assert!(!ready.process.has_exited().unwrap());
    assert_eq!(std::fs::read(marker).unwrap(),b"retained consumer record");
    assert!(app.poll_entry(end(),&AtomicBool::new(false)).is_err());
    assert!(app.stop().passed);assert!(ready.process.has_exited().unwrap());
    let simulator=f.root.path().join("simulator");
    assert_eq!(read_begin(&root,&simulator,&begun.previous,url).unwrap(),Some(begun.clone()));
    let before=std::fs::read(root.join("entry-accepted.json")).unwrap();
    assert_eq!(read_begin(&root,&simulator,&begun.previous,url).unwrap(),Some(begun.clone()));
    assert_eq!(std::fs::read(root.join("entry-accepted.json")).unwrap(),before);
    save_atomic(&root.join("terminated.json"),&second.identity).unwrap();
    assert!(read_begin(&root,&simulator,&begun.previous,url).is_err());
    let calls:Vec<Vec<String>>=std::fs::read_to_string(f.root.path().join("calls")).unwrap().lines().map(|s|serde_json::from_str(s).unwrap()).collect();
    for operation in ["install","launch","openurl","delete"] {assert_eq!(calls.iter().filter(|c|c[1]==operation).count(),1,"{operation}");}
}
#[test]
fn cold_entry_fences_uncertain_delivery_and_foreign_or_changed_replacements() {
    for mode in ["lost-url","foreign-process","old-generation","changed-install","changed-container"] {
        let (f,config,installed,first)=fixture(mode);
        let mut app=App::launch(crate::simulator_app::tests::acquire(&f),config,&f.root.path().join("app"),end(),&AtomicBool::new(false)).unwrap();
        endpoint(&app,first.identity.pid,"abcd0000-0000-0000-0000-000000000003");
        let root=f.root.path().join("entry");let begun=app.begin_entry("local-consumer://history",&root,end(),&AtomicBool::new(false));
        if mode=="lost-url" {
            assert!(begun.unwrap_err().retirement.unwrap().passed);
            assert!(!root.join("entry-accepted.json").exists());
        } else {
            begun.unwrap();let executable=if mode=="foreign-process" {PathBuf::from("/bin/sleep")} else {installed.join("Consumer")};let second=Consumer::start(&executable);
            endpoint(&app,second.identity.pid,if mode=="old-generation" {"abcd0000-0000-0000-0000-000000000003"} else {"abcd0000-0000-0000-0000-000000000004"});
            if mode=="changed-install" {std::fs::write(installed.join("asset"),b"changed").unwrap();}
            if mode=="changed-container" {let path=f.root.path().join("foreign-data");std::fs::create_dir(&path).unwrap();std::fs::write(f.root.path().join("container-path"),path.to_str().unwrap()).unwrap();}
            let error=app.poll_entry(end(),&AtomicBool::new(false)).unwrap_err();assert!(error.retirement.unwrap().passed,"{mode}");
            assert!(!root.join("restarted.json").exists());
            assert!(!second.identity.has_exited().unwrap(),"fixture simulator never kills an unrelated host process");
        }
        assert!(app.stop().passed);assert!(root.join("failure.json").exists());
        let calls:Vec<Vec<String>>=std::fs::read_to_string(f.root.path().join("calls")).unwrap().lines().map(|s|serde_json::from_str(s).unwrap()).collect();
        for operation in ["launch","openurl","delete"] {assert_eq!(calls.iter().filter(|c|c[1]==operation).count(),1,"{mode}: {operation}");}
    }
}
