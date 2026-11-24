use super::*;
use crate::{app_data::Recipe, products::BundleInventory};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

struct Guest {
    root: tempfile::TempDir,
    prepared: Prepared,
    corrupt: bool,
}
impl Guest {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir_all(source.join("files/sub dir")).unwrap();
        fs::write(source.join("files/a.bin"), [0, 255, 13, 10]).unwrap();
        fs::write(
            source.join("files/sub dir/quote' $(touch escaped).txt"),
            b"literal",
        )
        .unwrap();
        fs::set_permissions(
            source.join("files/a.bin"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let recipe = Recipe {
            app_id: "org.example.client".into(),
            product_sha256: "a".repeat(64),
            inventory: BundleInventory::capture(&source).unwrap(),
        };
        let prepared = recipe
            .construct(
                &source,
                &root.path().join("prepared"),
                "10000000-0000-0000-0000-000000000001",
                &recipe.app_id,
                &recipe.product_sha256,
            )
            .unwrap();
        fs::create_dir(root.path().join("app")).unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        let wrapper = root.path().join("bin/run-as");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nshift\ncd {} || exit\nexec \"$@\"\n",
                quote(root.path().join("app").to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            prepared,
            corrupt: false,
        }
    }
    fn shell(&self, script: &str) -> io::Result<String> {
        let output = Command::new("/bin/bash")
            .args(["-c", script])
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()),
            )
            .current_dir(self.root.path().join("app"))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        String::from_utf8(output.stdout).map_err(io::Error::other)
    }
    fn command(&self, args: &[&str]) -> io::Result<String> {
        if args[0] == "push" {
            fs::copy(args[1], args[2])?;
            return Ok(String::new());
        }
        if args.get(1) == Some(&"run-as")
            && matches!(args.get(3), Some(&"sha256sum") | Some(&"stat"))
        {
            let name = self.shell(&format!("printf %s {}", args.last().unwrap()))?;
            let path = self.root.path().join("app").join(&name);
            if args[3] == "sha256sum" {
                let bytes = if self.corrupt {
                    b"corrupt".to_vec()
                } else {
                    fs::read(&path)?
                };
                return Ok(format!(
                    "{}  {name}\n",
                    Sha256::digest(&bytes)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                ));
            }
            let meta = fs::metadata(path)?;
            return Ok(format!(
                "{} {:o}\n",
                meta.len(),
                meta.permissions().mode() & 0o777
            ));
        }
        // Execute actual POSIX redirection/preflight scripts; replace Android's sh only
        // to support pipefail on host shells. This is not Android/toybox qualification.
        self.shell(&args[1..].join(" ").replacen("sh -c", "bash -c", 1))
    }
    fn apply(&self) -> io::Result<()> {
        apply_files(
            &self.prepared,
            &self.root.path().join("prepared/data"),
            &self.prepared.recipe.app_id,
            self.root.path().join("upload").to_str().unwrap(),
            |args| self.command(args),
        )
    }
}
#[test]
fn copies_binary_and_literal_names_with_private_modes() {
    let guest = Guest::new();
    guest.apply().unwrap();
    guest
        .prepared
        .recipe
        .inventory
        .verify(&guest.root.path().join("app"))
        .unwrap();
    assert!(!guest.root.path().join("app/escaped").exists());
    assert!(!guest.root.path().join("upload").exists());
    assert_eq!(
        fs::metadata(guest.root.path().join("app/files/a.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(
            guest
                .root
                .path()
                .join("app/files/sub dir/quote' $(touch escaped).txt")
        )
        .unwrap()
        .permissions()
        .mode()
            & 0o777,
        0o600
    );
}
#[test]
fn preflights_all_destinations_before_writing_and_rejects_links() {
    let guest = Guest::new();
    let app = guest.root.path().join("app");
    fs::create_dir_all(app.join("files/sub dir")).unwrap();
    let collision = app.join("files/sub dir/quote' $(touch escaped).txt");
    fs::write(&collision, b"existing").unwrap();
    assert!(guest.apply().is_err());
    assert_eq!(fs::read(&collision).unwrap(), b"existing");
    assert!(!app.join("files/a.bin").exists());
    assert!(!guest.root.path().join("upload").exists());
    fs::remove_dir_all(app.join("files")).unwrap();
    std::os::unix::fs::symlink(guest.root.path().join("source/files"), app.join("files")).unwrap();
    assert!(guest.apply().is_err());
    guest
        .prepared
        .recipe
        .inventory
        .verify(&guest.root.path().join("source"))
        .unwrap();
}
#[test]
fn changed_source_and_corrupt_destination_cannot_complete() {
    let mut guest = Guest::new();
    guest.corrupt = true;
    assert!(
        guest
            .apply()
            .unwrap_err()
            .to_string()
            .contains("digest differs")
    );
    assert!(guest.root.path().join("upload").exists());
    assert!(
        !guest
            .root
            .path()
            .join("app/files/sub dir/quote' $(touch escaped).txt")
            .exists()
    );
    let guest = Guest::new();
    fs::write(
        guest.root.path().join("prepared/data/files/a.bin"),
        b"changed",
    )
    .unwrap();
    assert!(guest.apply().is_err());
    assert_eq!(
        fs::read_dir(guest.root.path().join("app")).unwrap().count(),
        0
    );
}

#[test]
fn sdk_application_reserves_staging_and_app_payload_before_writes() {
    use frontcap_core::{ids::LaunchId, write_budget::{Class, Limits, WriteBudget}};
    const MODE: &str = "FRONTCAP_ANDROID_DATA_ADMISSION";
    const ORDINARY: u64 = 4096;
    if let Ok(mode) = std::env::var(MODE) {
        let guest = Guest::new();
        let budget = WriteBudget::from_environment().unwrap().unwrap();
        let payload: u64 = guest.prepared.recipe.inventory.files.values().map(|file| file.bytes).sum();
        let application = payload.checked_mul(2).unwrap();
        let remaining = if mode == "refused" { application - 1 } else { application };
        budget.charge(Class::Ordinary, ORDINARY - budget.charged().unwrap().ordinary - remaining).unwrap();
        let before = budget.charged().unwrap();
        let mut commands = 0;
        let result = apply_files(&guest.prepared, &guest.root.path().join("prepared/data"),
            &guest.prepared.recipe.app_id, guest.root.path().join("upload").to_str().unwrap(),
            |args| { commands += 1; guest.command(args) });
        if mode == "refused" {
            assert!(result.unwrap_err().to_string().contains("write allowance exhausted"));
            assert_eq!(commands, 0, "no SDK command may precede complete application admission");
            assert_eq!(budget.charged().unwrap(), before);
            assert_eq!(fs::read_dir(guest.root.path().join("app")).unwrap().count(), 0);
            assert!(!guest.root.path().join("upload").exists());
        } else {
            result.unwrap();
            assert!(commands > 0);
            guest.prepared.recipe.inventory.verify(&guest.root.path().join("app")).unwrap();
            assert_eq!(budget.charged().unwrap().ordinary - before.ordinary, application);
            assert!(!guest.root.path().join("upload").exists());
        }
        assert_eq!(budget.charged().unwrap().retirement, 0);
        budget.save_atomic(Class::Retirement, &guest.root.path().join("retirement-proof.json"), &"original cleanup still writable").unwrap();
        guest.prepared.recipe.inventory.verify(&guest.root.path().join("source")).unwrap();
        return;
    }
    for mode in ["refused", "accepted"] {
        let root = tempfile::tempdir().unwrap();
        let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::generate(),
            Limits { ordinary: ORDINARY, retirement: 4096 }).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "android_app_data::tests::sdk_application_reserves_staging_and_app_payload_before_writes", "--nocapture"])
            .env(MODE, mode);
        WriteBudget::configure_child(&mut child, Some(&budget)).unwrap();
        assert!(child.status().unwrap().success(), "isolated application {mode}");
    }
}
