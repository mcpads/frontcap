use super::*;
use crate::{app_data::Recipe, products::BundleInventory};
use std::fs;

fn fixture(root: &Path, mode: &str) -> Launch {
    let product = root.join("Client.app");
    fs::create_dir(&product).unwrap();
    fs::write(product.join("Info.plist"), br#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>org.example.client</string></dict></plist>"#).unwrap();
    let inventory = BundleInventory::capture(&product).unwrap();
    let source = root.join("source");
    fs::create_dir_all(source.join("Documents")).unwrap();
    fs::write(source.join("Documents/state.json"), b"initial state").unwrap();
    let recipe = Recipe {
        app_id: "org.example.client".into(),
        product_sha256: sha256_hex(&canonical_of(&inventory)),
        inventory: BundleInventory::capture(&source).unwrap(),
    };
    recipe
        .construct(
            &source,
            &root.join("prepared"),
            "10000000-0000-0000-0000-000000000001",
            &recipe.app_id,
            &recipe.product_sha256,
        )
        .unwrap();
    let tools = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools")
        .canonicalize()
        .unwrap();
    let script = root.join("worker.py");
    let prefix = format!(
        "import sys\nsys.path.insert(0,{})\nmode={}\n",
        serde_json::to_string(&tools).unwrap(),
        serde_json::to_string(mode).unwrap()
    );
    fs::write(&script, prefix + r#"
import json,pathlib,queue,threading,shutil,fcntl
from flutter_prelaunch import wait_for_initial_data
from native_host_process import write_json
config=json.loads(pathlib.Path(sys.argv[1]).read_text());out=pathlib.Path(sys.argv[2]);out.mkdir()
assert json.loads(sys.stdin.readline())=={'command':'start'}
controls=queue.Queue()
def read():
 for line in sys.stdin: controls.put(json.loads(line))
 controls.put({'command':'stop'})
threading.Thread(target=read,daemon=True).start()
device='20000000-0000-0000-0000-000000000002'
app=out/device/'Client.app';app.parent.mkdir();shutil.copytree(config['installed_app']['product'],app)
data=app.parent/'Data';data.mkdir();stat=data.stat()
containers=dict(app=str(app),data=str(data),data_device=stat.st_dev,data_inode=stat.st_ino,installed_library_modes={})
identity=dict(platform='ios',simulator=device,bundle='org.example.client',containers=containers)
checks=0
def verify():
 global checks
 checks+=1
 if checks>1 and mode=='changed':return dict(identity,bundle='dev.foreign.app')
 return identity
def command(*args):raise AssertionError('iOS application needs no SDK command')
with (out/'session.lock').open('w') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 try:
  if mode=='collision':(data/'Documents').mkdir();(data/'Documents/state.json').write_text('existing')
  wait_for_initial_data(controls,out/'prelaunch',identity,__import__('time').monotonic()+10,command,verify)
  write_json(out/'launch-observation.json',dict(state=(data/'Documents/state.json').read_text()))
  ready=dict(owner=dict(platform='ios',device=device.upper(),generation='a'*48,source_sha256=config['source_sha256']),endpoint=dict(port=12345,token='b'*48),external_url_target=dict(platform='ios',simulator=device,bundle='org.example.client'))
  write_json(out/'ready.json',ready)
  controls.get(timeout=10)
 finally:
  write_json(out/'result.json',dict(state='stopped',retirement=dict(passed=True,errors=[],signal_error=None),error=None))
"#).unwrap();
    Launch {
        python: std::env::var_os("FRONTCAP_TEST_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/bin/python3".into()),
        worker_script: script,
        config: Config {
            flutter: None,
            project: None,
            platform: Platform::Ios,
            source_sha256: "c".repeat(64),
            acquisition_timeout_s: 30,
            lifetime_s: 30,
            android_sdk: None,
            avd: None,
            runtime: None,
            app_defines: Default::default(),
            system_ui: None,
            installed_app: Some(App::Ios(crate::flutter_ios_app::Launch {
                product,
                inventory,
            })),
            initial_data: Some(root.join("prepared")),
            data_volume: None, resource_executable: None, android_resources: None,
        },
    }
}

#[test]
fn flutter_initial_data_is_frozen_applied_before_launch_and_not_reapplied() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let launch = fixture(&root, "normal");
    let directory = root.join("session");
    let deadline = Instant::now() + Duration::from_secs(15);
    let cancelled = AtomicBool::new(false);
    let mut session = Session::start(launch, &directory, deadline).unwrap();
    fs::remove_dir_all(root.join("prepared")).unwrap();
    fs::remove_dir_all(root.join("source")).unwrap();
    let ready = session.wait_ready(deadline, &cancelled).unwrap();
    assert!(
        fs::read_to_string(directory.join("worker/launch-observation.json"))
            .unwrap()
            .contains("initial state")
    );
    let data =
        directory.join("worker/20000000-0000-0000-0000-000000000002/Data/Documents/state.json");
    fs::write(&data, b"local edit").unwrap();
    assert_eq!(session.wait_ready(deadline, &cancelled).unwrap(), ready);
    assert_eq!(fs::read(data).unwrap(), b"local edit");
    assert!(matches!(
        session.stop(deadline).unwrap(),
        Recovery::Finished(_)
    ));
}

#[test]
fn flutter_initial_data_failures_never_launch_or_replay() {
    for mode in ["collision", "changed"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let launch = fixture(&root, mode);
        let directory = root.join("session");
        let deadline = Instant::now() + Duration::from_secs(15);
        let cancelled = AtomicBool::new(false);
        let mut session = Session::start(launch, &directory, deadline).unwrap();
        assert!(session.wait_ready(deadline, &cancelled).is_err());
        assert!(session.wait_ready(deadline, &cancelled).is_err());
        assert!(matches!(
            session.stop(deadline).unwrap(),
            Recovery::Finished(_)
        ));
        assert!(!directory.join("worker/launch-observation.json").exists());
        if mode == "collision" {
            assert_eq!(
                fs::read(
                    directory.join(
                        "worker/20000000-0000-0000-0000-000000000002/Data/Documents/state.json"
                    )
                )
                .unwrap(),
                b"existing"
            );
        }
    }
}

#[test]
fn flutter_initial_data_rejects_source_build_and_changed_input_before_worker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut launch = fixture(&root, "normal");
    fs::write(root.join("prepared/data/Documents/state.json"), b"changed").unwrap();
    let directory = root.join("session");
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(Session::start(launch.clone(), &directory, deadline).is_err());
    assert!(!directory.exists());
    launch.config.installed_app = None;
    assert!(admit(&launch.config).is_err());
}

#[test]
fn supervised_initial_data_binds_relocated_container_and_refuses_replay() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let launch = fixture(&root, "normal");
    let App::Ios(app) = launch.config.installed_app.as_ref().unwrap() else { unreachable!() };
    let input = Prepared::read(&root.join("prepared"), "org.example.client",
        &sha256_hex(&canonical_of(&app.inventory))).unwrap();
    let simulator = "20000000-0000-0000-0000-000000000002";
    let owner = root.join("owner");
    let mount = owner.join("data-volume/worker/volume/mount/device-data");
    let installed = mount.join("Client.app");
    let data = mount.join("Data");
    fs::create_dir_all(&installed).unwrap();
    fs::create_dir(&data).unwrap();
    fs::copy(app.product.join("Info.plist"), installed.join("Info.plist")).unwrap();
    let source = root.join(simulator).join("data");
    fs::create_dir(source.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&mount, &source).unwrap();
    fs::write(owner.join("ready.json"), serde_json::to_vec(simulator).unwrap()).unwrap();
    fs::write(owner.join("launch.json"), serde_json::to_vec(&serde_json::json!({
        "xcrun":"/usr/bin/xcrun", "runtime":"runtime", "device_type":"phone",
        "source_sha256":"c".repeat(64),
        "data_volume":{"python":"/usr/bin/python3", "worker":"/worker.py", "size_mib":8192,"reserve_mib":64}
    })).unwrap()).unwrap();
    fs::write(owner.join("data-relocation.json"), serde_json::to_vec(&serde_json::json!({
        "source":source,"destination":mount,"backup":root.join("backup"),
        "original_device":0,"original_inode":0,"initial":app.inventory
    })).unwrap()).unwrap();
    let stat = fs::metadata(&data).unwrap();
    let identity = Installation::Ios {
        simulator: simulator.into(), bundle: input.recipe.app_id.clone(),
        containers: crate::flutter_ios_app::Containers {
            app: installed, data: data.clone(), data_device: stat.dev(), data_inode: stat.ino(),
            installed_library_modes: Default::default(),
        },
    };
    let destination = root.join("child");
    // A different creator cannot authorize even an otherwise valid relocated container.
    fs::write(owner.join("ready.json"), b"\"30000000-0000-0000-0000-000000000003\"").unwrap();
    assert!(apply_ios(&input, &root.join("prepared"), &destination, app, &identity, Some(&owner)).is_err());
    assert!(!destination.exists());
    assert!(!data.join("Documents").exists());
    fs::write(owner.join("ready.json"), serde_json::to_vec(simulator).unwrap()).unwrap();
    apply_ios(&input, &root.join("prepared"), &destination, app, &identity, Some(&owner)).unwrap();
    assert_eq!(fs::read(data.join("Documents/state.json")).unwrap(), b"initial state");
    fs::write(data.join("Documents/state.json"), b"local edit").unwrap();
    assert!(apply_ios(&input, &root.join("prepared"), &destination, app, &identity, Some(&owner)).is_err());
    assert_eq!(fs::read(data.join("Documents/state.json")).unwrap(), b"local edit");
    // A changed original relocation cannot authorize a fresh destination either.
    fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(&root, &source).unwrap();
    assert!(apply_ios(&input, &root.join("prepared"), &root.join("another-child"), app, &identity, Some(&owner)).is_err());
    assert!(!root.join("another-child").exists());
}
