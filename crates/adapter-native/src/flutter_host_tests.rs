use super::*;
use crate::{
    flutter_app::Acquired,
    flutter_ios_entry::{Polled, Started, Waiting},
    flutter_ios_fixture::acquisition,
};
use frontcap_core::native::{external_entry::Accepted, ios_ui::UiIdentity};

fn fixture(
    root: &std::path::Path,
    ambiguous: bool,
    lose_begin_reply: bool,
) -> (Session, Ready, Acquired) {
    let (mut ready, app) = acquisition(root, &"a".repeat(48), 101);
    ready.system_ui = Some(UiIdentity {
        simulator: ready.owner.device.clone(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 303,
    });
    std::fs::create_dir(root.join("worker")).unwrap();
    save_atomic(&root.join("worker/ready.json"), &ready).unwrap();
    let mut declarations = String::from("{");
    for bundle in if ambiguous {
        vec!["org.example.client", "dev.other.client"]
    } else {
        vec!["org.example.client"]
    } {
        let path = root.join(format!("{bundle}.app"));
        std::fs::create_dir(&path).unwrap();
        let xml = format!(
            r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{bundle}</string><key>CFBundleURLTypes</key><array><dict>
<key>CFBundleURLSchemes</key><array><string>client-test</string></array></dict></array></dict></plist>"#
        );
        std::fs::write(path.join("Info.plist"), xml).unwrap();
        declarations.push_str(&format!(
            "\"{bundle}\" = {{ CFBundleIdentifier = \"{bundle}\"; Path = \"{}\"; }};",
            path.display()
        ));
    }
    declarations.push('}');
    std::fs::write(root.join("installed.plist"), declarations).unwrap();
    let started = Started {
        command_number: 2,
        previous: app.clone(),
        entry: Accepted::new(crate::flutter_ios_entry::request(&ready, "client-test://item/1").unwrap()),
    };
    save_atomic(&root.join("started.json"), &started).unwrap();
    let script = root.join("worker.py");
    std::fs::write(&script, r#"import json,pathlib,sys
root=pathlib.Path(sys.argv[1]);lose=sys.argv[2]=='true'
for line in sys.stdin:
 notification=json.loads(line);number=notification['number'];folder=root/'worker/commands'/str(number)
 request=json.loads((folder/'request.json').read_text())
 with (root/'seen.jsonl').open('a') as out:out.write(json.dumps(request)+'\n')
 operation=request['operation']
 if operation=='installed_apps':result=(root/'installed.plist').read_text()
 elif operation=='begin_entry':result=(root/'started.json').read_text()
 elif operation=='poll_entry':result=json.dumps(dict(state='waiting',begin_command=2,readiness=dict(phase='process')))
 else:raise RuntimeError('unexpected command')
 (folder/'command.log').write_text(result)
 if operation=='begin_entry' and lose:break
 temp=folder/'reply.tmp';temp.write_text(json.dumps(dict(number=number,error=None)));temp.replace(folder/'reply.json')
"#).unwrap();
    let mut child = std::process::Command::new(
        std::env::var_os("FRONTCAP_TEST_PYTHON").unwrap_or_else(|| "/usr/bin/python3".into()),
    )
    .arg(script)
    .arg(root)
    .arg(lose_begin_reply.to_string())
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .spawn()
    .unwrap();
    let control = child.stdin.take();
    let config = Config {
        flutter: None,
        project: None,
        platform: Platform::Ios,
        source_sha256: ready.owner.source_sha256.clone(),
        acquisition_timeout_s: 30,
        lifetime_s: 30,
        android_sdk: None,
        avd: None,
        runtime: None,
        app_defines: Default::default(),
        system_ui: None,
        initial_data: None,
        data_volume: None, resource_executable: None, android_resources: None,
        installed_app: Some(crate::flutter_app::Launch::Ios(
            crate::flutter_ios_app::Launch {
                product: root.join("org.example.client.app"),
                inventory: crate::flutter_ios_app::Inventory {
                    files: Default::default(),
                    directories: Default::default(),
                },
            },
        )),
    };
    (
        Session {
            child: Some(child),
            control,
            directory: root.into(),
            config,
            host_number: 0,
            initial_data: None,
            write_budget: None,
        },
        ready,
        Acquired::Ios(app),
    )
}
fn close(mut session: Session) {
    session.control.take();
    assert!(session.child.take().unwrap().wait().unwrap().success());
}
fn operations(root: &std::path::Path) -> Vec<String> {
    // Test-only transcript inspection; the production command boundary uses Operation/Started/Polled.
    #[derive(Deserialize)]
    struct Seen {
        operation: String,
    }
    std::fs::read_to_string(root.join("seen.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Seen>(line).unwrap().operation)
        .collect()
}

#[test]
fn cold_host_commands_admit_before_retirement_and_bind_poll_to_original_command() {
    let root = tempfile::tempdir().unwrap();
    let (mut session, ready, app) = fixture(root.path(), false, false);
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut wrong = ready.clone();
    wrong.owner.generation = "b".repeat(48);
    assert!(
        session
            .prepare_entry(&wrong, "client-test://item/1", deadline, &cancelled)
            .is_err()
    );
    assert!(!root.path().join("seen.jsonl").exists());
    let admitted = session
        .prepare_entry(&ready, "client-test://item/1", deadline, &cancelled)
        .unwrap();
    assert_eq!(admitted.command_number(), 2);
    assert_eq!(admitted.request().url, "client-test://item/1");
    assert!(root.path().join("worker/entry-admissions/2.json").is_file());
    let started = session
        .begin_entry(admitted, &app, &ready, deadline, &cancelled)
        .unwrap();
    let polled = session
        .poll_entry(&started, &ready, None, deadline, &cancelled)
        .unwrap();
    assert_eq!(
        polled,
        Polled::Waiting {
            begin_command: 2,
            readiness: Waiting::Process {}
        }
    );
    close(session);
    assert_eq!(
        operations(root.path()),
        ["installed_apps", "begin_entry", "poll_entry"]
    );
}

#[test]
fn ambiguous_or_consumed_admission_cannot_send_cold_url() {
    for ambiguous in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (mut session, ready, app) = fixture(root.path(), ambiguous, false);
        let cancelled = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = session.prepare_entry(&ready, "client-test://item/1", deadline, &cancelled);
        if ambiguous {
            assert!(result.is_err());
        } else {
            let admitted = result.unwrap();
            session.installed_apps(deadline, &cancelled).unwrap();
            assert!(
                session
                    .begin_entry(admitted, &app, &ready, deadline, &cancelled)
                    .is_err()
            );
        }
        close(session);
        assert!(
            operations(root.path())
                .iter()
                .all(|operation| operation == "installed_apps")
        );
    }
}

#[test]
fn missing_begin_reply_retains_original_evidence_without_repeating_delivery() {
    let root = tempfile::tempdir().unwrap();
    let (mut session, ready, app) = fixture(root.path(), false, true);
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(5);
    let admitted = session
        .prepare_entry(&ready, "client-test://item/1", deadline, &cancelled)
        .unwrap();
    assert!(
        session
            .begin_entry(admitted, &app, &ready, deadline, &cancelled)
            .is_err()
    );
    let started: Started = load(&root.path().join("worker/commands/2/command.log"))
        .unwrap()
        .unwrap();
    started
        .validate(
            &app,
            &ready,
            &crate::flutter_ios_entry::request(&ready, "client-test://item/1").unwrap(),
        )
        .unwrap();
    assert!(
        session
            .prepare_entry(&ready, "client-test://item/1", deadline, &cancelled)
            .is_err()
    );
    close(session);
    assert_eq!(operations(root.path()), ["installed_apps", "begin_entry"]);
}
