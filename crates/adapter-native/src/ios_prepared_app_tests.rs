use frontcap_core::state::save_atomic;
use super::*;
use crate::{simulator::tests::Fixture, simulator_app::{App, AttachmentFile}};
use frontcap_core::canonical::sha256_bytes_hex;
use std::{os::unix::fs::PermissionsExt, time::Duration};

fn image(kind: u32) -> Vec<u8> {
    let mut bytes = vec![0; 32]; bytes[..4].copy_from_slice(&[0xcf,0xfa,0xed,0xfe]);
    bytes[12..16].copy_from_slice(&kind.to_le_bytes()); bytes
}
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir(to).unwrap();
    let inventory = BundleInventory::capture(from).unwrap();
    for name in inventory.directories { std::fs::create_dir_all(to.join(name)).unwrap(); }
    for name in inventory.files.keys() { std::fs::copy(from.join(name), to.join(name)).unwrap(); }
}
pub(crate) fn fixture() -> (Fixture, Launch, PathBuf) {
    let (f, mut config) = crate::simulator_app::tests::setup("normal");
    let original = config.app.clone();
    plist::to_file_xml(original.join("Info.plist"), &serde_json::json!({
        "CFBundleIdentifier":config.bundle,"CFBundleExecutable":"Consumer",
        "CFBundleSupportedPlatforms":["iPhoneSimulator"]
    })).unwrap();
    std::fs::write(original.join("Consumer"), image(2)).unwrap();
    std::fs::set_permissions(original.join("Consumer"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(original.join("asset"), b"consumer data").unwrap();
    let staged = f.root.path().join("Prepared.app"); copy_tree(&original, &staged);
    std::fs::create_dir(staged.join(FOLDER)).unwrap();
    for name in [LOADER, LIBRARY] {
        std::fs::write(staged.join(name), image(6)).unwrap();
        std::fs::set_permissions(staged.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(staged.join(SCRIPT), b"timeline").unwrap();
    let mut executable = image(2); executable.extend_from_slice(b"platform dependency");
    std::fs::write(staged.join("Consumer"), executable).unwrap();
    config.app = staged.clone(); config.environment.clear(); config.page_registry = true;
    config.library = AttachmentFile { path:staged.join(LIBRARY), sha256:sha256_bytes_hex(&image(6)) };
    config.script = Some(AttachmentFile {path:staged.join(SCRIPT),sha256:sha256_bytes_hex(b"timeline")});
    config.prepared = Some(Box::new(Prepared { original:original.clone(), original_inventory:BundleInventory::capture(&original).unwrap(),
        inventory:BundleInventory::capture(&staged).unwrap(),executable:"Consumer".into(),
        shared_libraries:BTreeSet::from([LOADER.into(),LIBRARY.into()]) }));
    let installed=f.root.path().join("Installed.app");copy_tree(&staged,&installed);
    std::fs::write(f.root.path().join("installed-path"),installed.to_str().unwrap()).unwrap();
    let script=std::fs::read_to_string(&f.config.xcrun).unwrap().replace("pointer=p/'container-path'", "pointer=p/('installed-path' if args[4]=='app' else 'container-path')");
    std::fs::write(&f.config.xcrun,script).unwrap();
    (f,config,installed)
}

#[test]
fn prepared_app_admission_preserves_consumer_content_and_rejects_double_injection() {
    let (_f, config, _) = fixture(); config.validate_attachment().unwrap();
    for mode in ["no-admission","environment","page-mode","mute-mode","foreign-bundle","forged-library-kind","changed-content"] {
        let mut changed=config.clone();
        match mode {
            "no-admission"=>changed.prepared=None,
            "environment"=>{changed.environment.insert("EXTRA".into(),"value".into());},
            "page-mode"=>changed.page_registry=false,
            "mute-mode"=>changed.mute_audio=false,
            "foreign-bundle"=>changed.bundle="dev.other".into(),
            "forged-library-kind"=>{changed.prepared.as_mut().unwrap().shared_libraries.insert("asset".into());},
            "changed-content"=>changed.prepared.as_mut().unwrap().inventory.files.get_mut("asset").unwrap().sha256="0".repeat(64),
            _=>unreachable!(),
        }
        assert!(changed.validate_attachment().is_err(),"{mode}");

    }
    let original=config.prepared.as_ref().unwrap().original.clone();
    std::fs::write(original.join("asset"),b"changed original").unwrap();
    assert!(config.validate_attachment().is_err());
    std::fs::write(original.join("asset"),b"consumer data").unwrap();
    std::fs::remove_file(config.app.join("asset")).unwrap();
    std::os::unix::fs::symlink(original.join("asset"),config.app.join("asset")).unwrap();
    assert!(config.validate_attachment().is_err());
}

#[test]
fn prepared_launch_verifies_installed_bytes_and_retains_offline_identity_without_injection() {
    for corrupt in [false,true] {
        let (f,config,installed)=fixture();
        // Recorded CoreSimulator behavior: only a verified shared library may lose execute bits.
        std::fs::set_permissions(installed.join(LIBRARY),std::fs::Permissions::from_mode(0o644)).unwrap();
        if corrupt {std::fs::write(installed.join("asset"),b"foreign installed data").unwrap();}
        let root=f.root.path().join("app");
        let app=App::launch(crate::simulator_app::tests::acquire(&f),config.clone(),&root,
            Instant::now()+Duration::from_secs(10),&AtomicBool::new(false));
        if corrupt {
            assert!(app.err().unwrap().retirement.unwrap().passed);
            assert!(!f.root.path().join("environment.json").exists());
            continue;
        }
        let mut app=app.unwrap();
        let environment:BTreeMap<String,String>=load(&f.root.path().join("environment.json")).unwrap().unwrap();
        assert!(environment.is_empty());
        let simulator=app.simulator_id().to_owned();
        let expected=read_installed(&root,&config,&simulator).unwrap().unwrap();
        assert_eq!(expected.container,installed);
        // Input archives are evidence, not live prerequisites for original-result lookup.
        std::fs::remove_dir_all(&config.app).unwrap();
        std::fs::remove_dir_all(&config.prepared.as_ref().unwrap().original).unwrap();
        assert_eq!(read_installed(&root,&config,&simulator).unwrap(),Some(expected.clone()));
        let mut changed=expected.clone();changed.inventory.files.get_mut("asset").unwrap().sha256="0".repeat(64);
        save_atomic(&root.join("installed-app.json"),&changed).unwrap();
        assert!(read_installed(&root,&config,&simulator).is_err());
        changed=expected;changed.inventory.files.get_mut("Consumer").unwrap().executable=false;
        save_atomic(&root.join("installed-app.json"),&changed).unwrap();
        assert!(read_installed(&root,&config,&simulator).is_err());
        assert!(app.stop().passed);
    }
}

#[test]
fn shared_library_headers_distinguish_executables_and_all_universal_slices() {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("image");
    for kind in [2,6,8] {
        std::fs::write(&path,image(kind)).unwrap();assert_eq!(shared_library(&path).unwrap(),kind==6);assert_eq!(test_bundle(&path).unwrap(),kind==8);
    }
    let mut fat=vec![0;128];fat[..4].copy_from_slice(&[0xca,0xfe,0xba,0xbe]);
    fat[4..8].copy_from_slice(&2_u32.to_be_bytes());fat[16..20].copy_from_slice(&64_u32.to_be_bytes());
    fat[36..40].copy_from_slice(&96_u32.to_be_bytes());fat[64..96].copy_from_slice(&image(6));fat[96..128].copy_from_slice(&image(6));
    std::fs::write(&path,&fat).unwrap();assert!(shared_library(&path).unwrap());
    fat[96..128].copy_from_slice(&image(2));std::fs::write(&path,&fat).unwrap();assert!(!shared_library(&path).unwrap());
    fat[64..96].copy_from_slice(&image(8));fat[96..128].copy_from_slice(&image(8));
    std::fs::write(&path,&fat).unwrap();assert!(test_bundle(&path).unwrap());
    fat[96..128].copy_from_slice(&image(2));std::fs::write(&path,&fat).unwrap();assert!(!test_bundle(&path).unwrap());
}
