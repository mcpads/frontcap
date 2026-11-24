use super::*;
use crate::{app_data::Recipe, simulator::tests::Fixture};
use std::{fs, os::unix::fs::PermissionsExt};

fn fixture() -> (Fixture, XctestLaunch, PathBuf) {
    let f = Fixture::new("normal");
    let mut config = crate::xctest::tests::config(&f);
    fs::write(config.products.join("SampleApp.xctestrun"), r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>__xctestrun_metadata__</key><dict><key>FormatVersion</key><integer>1</integer></dict>
<key>SampleAppTests</key><dict><key>IsUITestBundle</key><true/>
<key>TestHostBundleIdentifier</key><string>org.example.app.uitests.xctrunner</string>
<key>TestBundlePath</key><string>__TESTHOST__/PlugIns/SampleAppTests.xctest</string>
<key>TestHostPath</key><string>__TESTROOT__/Debug-iphonesimulator/SampleAppTests-Runner.app</string>
<key>UITargetAppPath</key><string>__TESTROOT__/Debug-iphonesimulator/SampleApp.app</string>
<key>DependentProductPaths</key><array><string>__TESTROOT__/Debug-iphonesimulator/SampleApp.app</string><string>__TESTROOT__/Debug-iphonesimulator/SampleAppTests-Runner.app</string></array>
<key>TestingEnvironmentVariables</key><dict><key>UNCHANGED</key><string>value</string><key>ROOT</key><string>__TESTROOT__/Debug-iphonesimulator</string></dict>
</dict></dict></plist>"#).unwrap();
    let mut header = vec![0xcf, 0xfa, 0xed, 0xfe];
    header.extend([0; 8]);
    header.extend(6_u32.to_le_bytes());
    for name in ["vclock.dylib", "ui_schedule.dylib"] {
        fs::write(
            config
                .products
                .join("Debug-iphonesimulator/SampleApp.app")
                .join(name),
            &header,
        )
        .unwrap();
    }
    header[12..16].copy_from_slice(&8_u32.to_le_bytes());
    fs::write(config.products.join("Debug-iphonesimulator/SampleAppTests-Runner.app/PlugIns/SampleAppTests.xctest/SampleAppTests"), &header).unwrap();
    config.manifest = ProductsManifest::capture(&config.products, "SampleApp.xctestrun").unwrap();
    let source = f.root.path().join("seed");
    fs::create_dir_all(source.join("Documents")).unwrap();
    fs::write(source.join("Documents/state.json"), b"initial state").unwrap();
    let product =
        BundleInventory::capture(&config.products.join("Debug-iphonesimulator/SampleApp.app"))
            .unwrap();
    let recipe = Recipe {
        app_id: config.app.bundle.clone(),
        product_sha256: sha256_hex(&canonical_of(&product)),
        inventory: BundleInventory::capture(&source).unwrap(),
    };
    let prepared = f.root.path().join("prepared");
    recipe
        .construct(
            &source,
            &prepared,
            "10000000-0000-0000-0000-000000000001",
            &recipe.app_id,
            &recipe.product_sha256,
        )
        .unwrap();
    config.initial_data = Some(prepared);
    let bootstrap = f.root.path().join("bootstrap-source.app");
    fs::create_dir(&bootstrap).unwrap();
    fs::write(bootstrap.join("Bootstrap"), b"platform bootstrap").unwrap();
    fs::set_permissions(bootstrap.join("Bootstrap"), fs::Permissions::from_mode(0o700)).unwrap();
    plist::to_file_xml(bootstrap.join("Info.plist"), &BTreeMap::from([
        ("CFBundleIdentifier", "dev.frontcap.bootstrap"),
        ("CFBundleExecutable", "Bootstrap"), ("CFBundlePackageType", "APPL"),
    ])).unwrap();
    config.bootstrap_app = Some(BootstrapApp { inventory: BundleInventory::capture(&bootstrap).unwrap(), path: bootstrap });
    let container = f
        .root
        .path()
        .join("ABCD0000-0000-0000-0000-000000000001/data");
    fs::create_dir_all(&container).unwrap();
    fs::write(
        f.root.path().join("container-path"),
        container.to_str().unwrap(),
    )
    .unwrap();
    plist::to_file_xml(
        f.root.path().join("apps.plist"),
        &BTreeMap::<String, String>::new(),
    )
    .unwrap();
    let script = fs::read_to_string(&f.config.xcrun).unwrap();
    let extra = r#"
if args[1]=='install':
    import shutil
    app=p/'ABCD0000-0000-0000-0000-000000000001'/('runner' if args[3].endswith('-Runner.app') else 'app')
    shutil.copytree(args[3],app)
    for name in ['vclock.dylib','ui_schedule.dylib']:
        if (app/name).exists(): (app/name).chmod(0o600)
    bundle=app/'PlugIns/SampleAppTests.xctest/SampleAppTests'
    if bundle.exists(): bundle.chmod(0o600)
    if (p/'wrong-mode').exists() and (app/'SampleApp').exists(): (app/'SampleApp').chmod(0o600)
    if (p/'corrupt-product').exists() and (app/'SampleApp').exists(): (app/'SampleApp').write_text('changed installation')
    sys.exit(0)
if args[1]=='get_app_container' and args[-1]=='app':
    print(p/'ABCD0000-0000-0000-0000-000000000001'/('runner' if args[3].endswith('.xctrunner') else 'app'));sys.exit(0)
"#;
    fs::write(
        &f.config.xcrun,
        script.replacen("if args[:3]", &format!("{extra}\nif args[:3]"), 1),
    )
    .unwrap();
    fs::write(f.root.path().join("expected"), b"initial state").unwrap();
    fs::write(
        &config.xcodebuild,
        r#"#!/usr/bin/env python3
import pathlib,time,sys,plistlib,subprocess
p=pathlib.Path(__file__).parent
config=pathlib.Path(sys.argv[sys.argv.index('-xctestrun')+1])
value=plistlib.loads(config.read_bytes())['SampleAppTests']
assert 'UseDestinationArtifacts' not in value
assert value['TestingEnvironmentVariables']==dict(UNCHANGED='value',ROOT=str(config.parent/'products/Debug-iphonesimulator'))
assert value['UITargetAppPath']==str(config.parent/'bootstrap.app')
assert value['DependentProductPaths']==[str(config.parent/'bootstrap.app'),str(config.parent/'products/Debug-iphonesimulator/SampleAppTests-Runner.app')]
products=pathlib.Path(value['TestHostPath']).parent
# Model ordinary XCTest installation before publishing the runner endpoint.
for name in ['SampleAppTests-Runner.app']:
    destination=p/'ABCD0000-0000-0000-0000-000000000001'/('runner' if '-Runner' in name else 'app')
    if not destination.exists():
        subprocess.run([str(p/'xcrun'),'simctl','install','ABCD0000-0000-0000-0000-000000000001',str(products/name)],check=True)
(p/'runner-started').touch()
time.sleep(60)
"#,
    )
    .unwrap();
    fs::set_permissions(&config.xcodebuild, fs::Permissions::from_mode(0o700)).unwrap();
    (f, config, container.canonicalize().unwrap())
}
fn start(f: &Fixture, config: XctestLaunch) -> Result<Xctest, AcquisitionError> {
    Xctest::start_at(
        config,
        &f.root.path().join("session"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
}
fn started(f: &Fixture, owner: &mut Xctest) {
    let end = Instant::now() + Duration::from_secs(3);
    while !f.root.path().join("runner-started").exists() {
        owner.ensure_running().unwrap();
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn data_follows_installation_precedes_app_launch_and_reattachment_keeps_edits() {
    let (f, config, container) = fixture();
    let mut owner = start(&f, config.clone()).unwrap();
    started(&f, &mut owner);
    assert!(!container.join("Documents/state.json").exists());
    assert!(owner.apply_initial_data(Instant::now()+Duration::from_secs(3), &AtomicBool::new(false)).is_err());
    // Separate readiness tests verify endpoint/PID admission; model its completed result here.
    owner.ready_process = ProcessIdentity::of(owner.runner.as_ref().unwrap().id()).unwrap();
    fs::remove_dir_all(config.initial_data.as_ref().unwrap()).unwrap();
    owner.apply_initial_data(Instant::now()+Duration::from_secs(3), &AtomicBool::new(false)).unwrap();
    assert_eq!(fs::read(container.join("Documents/state.json")).unwrap(), b"initial state");
    assert!(owner.apply_initial_data(Instant::now()+Duration::from_secs(3), &AtomicBool::new(false)).is_err());
    config
        .manifest
        .verify(&f.root.path().join("session/products"))
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        owner
            .app_container(
                &config.app.bundle,
                Instant::now() + Duration::from_secs(3),
                &cancel
            )
            .unwrap(),
        container
    );
    let other = container.with_file_name("another-data");
    fs::create_dir(&other).unwrap();
    fs::write(
        f.root.path().join("container-path"),
        other.to_str().unwrap(),
    )
    .unwrap();
    assert!(
        owner
            .app_container(
                &config.app.bundle,
                Instant::now() + Duration::from_secs(3),
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("container changed")
    );
    fs::write(
        f.root.path().join("container-path"),
        container.to_str().unwrap(),
    )
    .unwrap();
    fs::write(container.join("Documents/state.json"), b"local edit").unwrap();
    fs::write(f.root.path().join("expected"), b"local edit").unwrap();
    // Model verified runner readiness; the separate readiness tests own endpoint/process admission.
    owner.ready_process = ProcessIdentity::of(owner.runner.as_ref().unwrap().id()).unwrap();
    let simulator = owner.release_simulator().unwrap();
    fs::remove_file(f.root.path().join("runner-started")).unwrap();
    let mut replacement = Xctest::on_simulator(
        config.into(),
        simulator,
        &f.root.path().join("replacement"),
        Instant::now() + Duration::from_secs(10),
        &cancel,
    )
    .unwrap();
    started(&f, &mut replacement);
    assert_eq!(
        fs::read(container.join("Documents/state.json")).unwrap(),
        b"local edit"
    );
    assert!(replacement.stop().passed);
}
#[test]
fn invalid_seed_test_target_or_bootstrap_rejects_before_simulator_acquisition() {
    for mode in ["seed", "test-target", "missing-bootstrap", "changed-bootstrap", "consumer-bootstrap"] {
        let (f, mut config, _) = fixture();
        if mode == "seed" {
            fs::write(
                config
                    .initial_data
                    .as_ref()
                    .unwrap()
                    .join("data/Documents/state.json"),
                b"changed input",
            )
            .unwrap();
        } else if mode == "test-target" {
            let path = config.products.join("SampleApp.xctestrun");
            let text = fs::read_to_string(&path)
                .unwrap()
                .replace("org.example.app.uitests.xctrunner", "dev.foreign.runner");
            fs::write(path, text).unwrap();
            config.manifest =
                ProductsManifest::capture(&config.products, "SampleApp.xctestrun").unwrap();
        }
        if mode == "missing-bootstrap" { config.bootstrap_app = None; }
        if mode == "changed-bootstrap" {
            fs::write(config.bootstrap_app.as_ref().unwrap().path.join("Bootstrap"), b"changed").unwrap();
        }
        if mode == "consumer-bootstrap" {
            let bootstrap = config.bootstrap_app.as_mut().unwrap();
            plist::to_file_xml(bootstrap.path.join("Info.plist"), &BTreeMap::from([
                ("CFBundleIdentifier", config.app.bundle.as_str()),
                ("CFBundleExecutable", "Bootstrap"), ("CFBundlePackageType", "APPL"),
            ])).unwrap();
            bootstrap.inventory = BundleInventory::capture(&bootstrap.path).unwrap();
        }
        assert!(start(&f, config).is_err());
        assert!(!f.root.path().join("calls").exists());
        assert!(!f.root.path().join("runner-started").exists());
    }
}

#[test]
fn existing_installation_collision_and_wrong_product_prevent_data_completion() {
    for mode in ["existing", "collision", "product", "executable"] {
        let (f, config, container) = fixture();
        match mode {
            "existing" => plist::to_file_xml(
                f.root.path().join("apps.plist"),
                &BTreeMap::from([(config.app.bundle.clone(), "already installed")]),
            )
            .unwrap(),
            "collision" => {
                fs::create_dir(container.join("Documents")).unwrap();
                fs::write(container.join("Documents/state.json"), b"existing data").unwrap();
            }
            "product" => fs::write(f.root.path().join("corrupt-product"), b"").unwrap(),
            _ => fs::write(f.root.path().join("wrong-mode"), b"").unwrap(),
        }
        match start(&f, config) {
            Err(error) => { assert_eq!(mode, "existing"); assert!(error.retirement.unwrap().passed); }
            Ok(mut owner) => {
                assert_ne!(mode, "existing");
                started(&f, &mut owner);
                owner.ready_process = ProcessIdentity::of(owner.runner.as_ref().unwrap().id()).unwrap();
                assert!(owner.apply_initial_data(Instant::now()+Duration::from_secs(3), &AtomicBool::new(false)).is_err());
                assert!(owner.stop().passed);
            }
        }
        assert!(f.root.path().join("deleted").exists());
        assert!(
            !f.root
                .path()
                .join("session/initial-data-applied.json")
                .exists()
        );
        if mode == "collision" {
            assert_eq!(
                fs::read(container.join("Documents/state.json")).unwrap(),
                b"existing data"
            );
        }
    }
}
