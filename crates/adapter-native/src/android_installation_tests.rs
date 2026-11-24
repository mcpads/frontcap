use super::*;

#[test]
fn installation_requires_the_frozen_apk_and_actual_app_data_identity() {
    let package = "org.example.client";
    let activity = "org.example.client.Main";
    let hash = "a".repeat(64);
    let apk = "/data/app/~~random==/org.example.client-digest==/base.apk";
    for changed in ["none", "split", "hash", "digest-path", "data"] {
        let result = installation(package, activity, &hash, |args| {
            Ok(match args {
                ["shell", "pm", "path", _] => format!(
                    "package:{apk}\n{}",
                    if changed == "split" {
                        "package:/data/app/split.apk\n"
                    } else {
                        ""
                    }
                ),
                ["shell", "sha256sum", _] => format!(
                    "{}  {}\n",
                    if changed == "hash" {
                        "b".repeat(64)
                    } else {
                        hash.clone()
                    },
                    if changed == "digest-path" {
                        "/other.apk"
                    } else {
                        apk
                    }
                ),
                ["shell", "run-as", _, "pwd"] => format!(
                    "/data/user/0/{}\n",
                    if changed == "data" {
                        "dev.other.client"
                    } else {
                        package
                    }
                ),
                ["shell", "run-as", _, "stat", "-c", "'%d %i'", "."] => "2049 5511\n".into(),
                _ => panic!("unexpected SDK command {args:?}"),
            })
        });
        if changed == "none" {
            let value = result.unwrap();
            assert_eq!(value.apk, PathBuf::from(apk));
            assert_eq!((value.data_device, value.data_inode), (2049, 5511));
        } else {
            assert!(result.is_err(), "{changed}");
        }
    }
}

#[test]
fn process_readback_rejects_reuse_foreign_commands_and_multiple_app_processes() {
    let package = "org.example.client";
    let mut stats = 0;
    for changed in ["none", "multiple", "command", "reused", "dead"] {
        let result = process(package, "org.example.client.Main", 123, |args| {
            Ok(match args {
                ["shell", "pidof", _] => if changed == "multiple" {
                    "123 124\n"
                } else {
                    "123\n"
                }
                .into(),
                ["shell", "cat", "/proc/sys/kernel/random/boot_id"] => {
                    "abcd0000-0000-0000-0000-000000000001\n".into()
                }
                ["shell", "run-as", _, "cat", "/proc/123/cmdline"] => format!(
                    "{}\0",
                    if changed == "command" {
                        "dev.other.client"
                    } else {
                        package
                    }
                ),
                ["shell", "run-as", _, "cat", "/proc/123/stat"] => {
                    stats += 1;
                    let ticks = if changed == "reused" { stats } else { 400 };
                    // comm can contain spaces and parentheses; fields 4–21 precede starttime.
                    format!(
                        "123 (client (main)) {} {} {ticks} 0 0\n",
                        if changed == "dead" { "Z" } else { "S" },
                        vec!["0"; 18].join(" ")
                    )
                }
                _ => panic!("unexpected SDK command {args:?}"),
            })
        });
        if changed == "none" {
            assert_eq!(result.unwrap().start_ticks, 400);
        } else {
            assert!(result.is_err(), "{changed}");
        }
    }
}

#[test]
fn replacement_keeps_device_installation_and_source_while_changing_the_app_generation() {
    use frontcap_core::native::{android_app::Owner, android_lifecycle::Replacement};
    let previous = Owner {
        package: "org.example.client".into(),
        activity: "org.example.client.Main".into(),
        serial: "emulator-5580".into(),
        generation: "abcd0000-0000-0000-0000-000000000001".into(),
        pid: 123,
        source_sha256: "a".repeat(64),
        apk_sha256: "b".repeat(64),
    };
    let old = Process {
        pid: 123,
        boot_id: "abcd0000-0000-0000-0000-000000000002".into(),
        start_ticks: 50,
        command: previous.package.clone(),
    };
    let result = Replacement {
        entry: None,
        previous: previous.clone(),
        owner: Owner {
            generation: "abcd0000-0000-0000-0000-000000000003".into(),
            ..previous
        },
        previous_process: old.clone(),
        process: Process {
            start_ticks: 60,
            ..old
        },
        installation: Installation {
            package: "org.example.client".into(),
            apk: "/data/app/base.apk".into(),
            apk_sha256: "b".repeat(64),
            data: "/data/user/0/org.example.client".into(),
            data_device: 1,
            data_inode: 2,
        },
    };
    result.validate().unwrap(); // PID reuse is safe only with independently different birth evidence.
    let mut cold=result.clone();
    cold.entry=frontcap_core::native::android_lifecycle::restart_entry(&cold.previous,Some("client-test://item/1"))
        .unwrap().map(frontcap_core::native::external_entry::Accepted::new);
    cold.validate().unwrap();
    cold.entry.as_mut().unwrap().request.generation=cold.owner.generation.clone();
    assert!(cold.validate().is_err());
    let mut changes = vec![result.clone(); 5];
    changes[0].process.start_ticks = result.previous_process.start_ticks;
    changes[1].process.boot_id = "abcd0000-0000-0000-0000-000000000009".into();
    changes[2].owner.generation = result.previous.generation.clone();
    changes[3].owner.source_sha256 = "c".repeat(64);
    changes[4].installation.apk_sha256 = "d".repeat(64);
    for changed in changes {
        assert!(changed.validate().is_err());
    }
}
