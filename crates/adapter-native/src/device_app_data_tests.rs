use super::*;
use crate::{app_data::Recipe, products::BundleInventory};
use std::fs;

#[test]
fn refuses_existing_installation_or_running_app_before_launch_and_retires() {
    for existing in [true, false] {
        let mut fixture = crate::device::tests::Fixture::view(false);
        let source = fixture.root().join("seed");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("state.json"), b"seed").unwrap();
        let recipe = Recipe {
            app_id: "org.example.client".into(),
            product_sha256: fixture.config.apk_sha256.clone(),
            inventory: BundleInventory::capture(&source).unwrap(),
        };
        let prepared = fixture.root().join("prepared");
        recipe
            .construct(
                &source,
                &prepared,
                "10000000-0000-0000-0000-000000000001",
                &recipe.app_id,
                &recipe.product_sha256,
            )
            .unwrap();
        fixture.config.initial_data = Some(prepared);
        let adb = fs::read_to_string(&fixture.config.adb).unwrap();
        let marker = "if args[:3] == ['shell','ps','-A']:";
        let extra = if existing {
            "if args == ['shell','pm','list','packages','--user','0','org.example.client']: print('package:org.example.client'); sys.exit(0)\n".to_owned()
        } else {
            "if args == ['shell','cat','/proc/sys/kernel/random/boot_id']: print('10000000-0000-0000-0000-000000000002'); sys.exit(0)\nif args == ['shell','ps','-A','-o','NAME']: print('NAME\\norg.example.client:background'); sys.exit(0)\n".to_owned()
        };
        assert!(adb.contains(marker));
        fs::write(
            &fixture.config.adb,
            adb.replacen(marker, &format!("{extra}{marker}"), 1),
        )
        .unwrap();
        let result = Device::acquire_at(
            fixture.config.clone(),
            &fixture.root().join("session"),
            Instant::now() + Duration::from_secs(8),
            &AtomicBool::new(false),
            &fixture.root().join("host"),
        );
        let error = match result {
            Ok(_) => panic!("unsafe acquisition succeeded"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(if existing {
                "fresh installation"
            } else {
                "unstarted package"
            }),
            "{error}"
        );
        let calls = fs::read_to_string(fixture.root().join("adb-calls.jsonl")).unwrap();
        let calls: Vec<Vec<String>> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!calls.iter().any(|call| call.starts_with(&[
            "shell".into(),
            "am".into(),
            "start".into()
        ])));
        assert!(
            !calls
                .iter()
                .any(|call| call[0] == "push" || call[0] == "uninstall")
        );
        if existing {
            assert!(!calls.iter().any(|call| call[0] == "install"));
        }
        let identity: ProcessIdentity = load(&fixture.root().join("session/emulator-process.json"))
            .unwrap()
            .unwrap();
        assert!(identity.has_exited().unwrap());
        assert!(
            load::<HostSession>(&fixture.root().join("host/native-session.json"))
                .unwrap()
                .unwrap()
                .retired
        );
        assert!(
            !fixture
                .root()
                .join("session/initial-data-applied.json")
                .exists()
        );
    }
}
