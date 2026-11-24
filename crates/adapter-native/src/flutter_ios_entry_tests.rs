use super::*;
use crate::flutter_ios_fixture::acquisition;
use frontcap_core::native::ios_ui::UiIdentity;

#[test]
fn cold_readiness_preserves_one_url_process_and_independent_system_owner() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().join("installed apps");
    let (mut ready, mut previous) = acquisition(&app_root, &"a".repeat(48), 101);
    previous.process.command.push_str(" --enable-dart-profiling --verify-entry-points");
    ready.system_ui = Some(UiIdentity {
        simulator: ready.owner.device.clone(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 303,
    });
    let request = request(&ready, "client-test://item/1").unwrap();
    let started = Started {
        command_number: 4,
        previous: previous.clone(),
        entry: Accepted::new(request.clone()),
    };
    started
        .validate(&Acquired::Ios(previous.clone()), &ready, &request)
        .unwrap();
    let (mut next, current) = acquisition(&app_root, &"b".repeat(48), 102);
    next.system_ui = ready.system_ui.clone();
    let absent = Polled::Waiting {
        begin_command: 4,
        readiness: Waiting::Process {},
    };
    let appeared = Polled::Waiting {
        begin_command: 4,
        readiness: Waiting::Endpoint {
            process: current.process.clone(),
        },
    };
    let result = Polled::Ready {
        begin_command: 4,
        replacement: Box::new(Restarted {
            previous: Acquired::Ios(previous),
            current: Acquired::Ios(current.clone()),
            ready: next,
            entry: Some(started.entry.clone()),
        }),
    };
    assert!(absent.validate(&started, &ready, None).unwrap().is_none());
    assert!(
        appeared
            .validate(&started, &ready, Some(&absent))
            .unwrap()
            .is_none()
    );
    let Replacement::Ios(replacement) = result
        .validate(&started, &ready, Some(&appeared))
        .unwrap()
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(replacement.entry, Some(started.entry.clone()));
    assert_eq!(replacement.previous, ready.owner);
    assert_eq!(replacement.process, current.process);
    assert!(absent.validate(&started, &ready, Some(&appeared)).is_err());
    assert!(result.validate(&started, &ready, Some(&result)).is_err());
    let mut changed = appeared.clone();
    let Polled::Waiting {
        readiness: Waiting::Endpoint { process },
        ..
    } = &mut changed
    else {
        panic!()
    };
    process.pid += 1;
    assert!(changed.validate(&started, &ready, Some(&appeared)).is_err());
    for command in [
        app_root.join("Client.app/Other").display().to_string(),
        app_root.join("Client.app.other/Runner").display().to_string(),
        app_root.join("Client.app/../Runner").display().to_string(),
    ] {
        let foreign = Polled::Waiting {
            begin_command: 4,
            readiness: Waiting::Endpoint {
                process: Process { command, ..current.process.clone() },
            },
        };
        assert!(foreign.validate(&started, &ready, Some(&absent)).is_err());
    }
    for mutation in [
        "url",
        "process",
        "executable",
        "container",
        "runner",
        "generation",
        "missing-entry",
    ] {
        let mut changed = result.clone();
        let Polled::Ready { replacement, .. } = &mut changed else {
            panic!()
        };
        let Acquired::Ios(app) = &mut replacement.current else {
            panic!()
        };
        match mutation {
            "url" => replacement.entry.as_mut().unwrap().request.url = "client-test://item/2".into(),
            "process" => app.process.pid += 1,
            "executable" => app.process.command.push_str("-other"),
            "container" => app.containers.data_inode += 1,
            "runner" => replacement.ready.system_ui.as_mut().unwrap().pid += 1,
            "generation" => replacement.ready.owner.generation = ready.owner.generation.clone(),
            "missing-entry" => replacement.entry = None,
            _ => unreachable!(),
        }
        assert!(
            changed.validate(&started, &ready, Some(&appeared)).is_err(),
            "{mutation}"
        );
    }
    // Ordinary restart may add launch arguments again after a cold URL launch.
    let mut ordinary = replacement;
    std::mem::swap(&mut ordinary.previous_process.command, &mut ordinary.process.command);
    ordinary.entry = None;
    ordinary.validate().unwrap();
}

#[test]
fn cold_phase_rejects_missing_system_owner_and_conflicting_request_or_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let (mut ready, app) = acquisition(root.path(), &"a".repeat(48), 101);
    assert!(request(&ready, "client-test://item/1").is_err());
    ready.system_ui = Some(UiIdentity {
        simulator: ready.owner.device.clone(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 303,
    });
    assert!(request(&ready, "relative").is_err());
    let expected = request(&ready, "client-test://item/1").unwrap();
    let original = Started {
        command_number: 4,
        previous: app.clone(),
        entry: Accepted::new(expected.clone()),
    };
    for mutation in ["number", "url", "generation", "process"] {
        let mut changed = original.clone();
        match mutation {
            "number" => changed.command_number = 0,
            "url" => changed.entry.request.url = "client-test://item/2".into(),
            "generation" => changed.entry.request.generation = "b".repeat(48),
            "process" => changed.previous.process.pid += 1,
            _ => unreachable!(),
        }
        assert!(
            changed
                .validate(&Acquired::Ios(app.clone()), &ready, &expected)
                .is_err(),
            "{mutation}"
        );
    }
    let wrong = Polled::Waiting {
        begin_command: 5,
        readiness: Waiting::Process {},
    };
    assert!(wrong.validate(&original, &ready, None).is_err());
    let old = Polled::Waiting {
        begin_command: 4,
        readiness: Waiting::Endpoint {
            process: app.process,
        },
    };
    assert!(old.validate(&original, &ready, None).is_err());
    let mut old_with_arguments = old.clone();
    let Polled::Waiting { readiness: Waiting::Endpoint { process }, .. } = &mut old_with_arguments else { panic!() };
    process.command.push_str(" --verify-entry-points");
    assert!(old_with_arguments.validate(&original, &ready, None).is_err());
    assert!(
        serde_json::from_str::<Polled>(
            r#"{"state":"waiting","begin_command":4,"readiness":{"phase":"endpoint"}}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<Polled>(r#"{"state":"waiting","begin_command":4,"readiness":{"phase":"process","process":null}}"#).is_err());
    ready.system_ui.as_mut().unwrap().simulator = "abcd0000-0000-0000-0000-000000000003".into();
    assert!(request(&ready, "client-test://item/1").is_err());
}
