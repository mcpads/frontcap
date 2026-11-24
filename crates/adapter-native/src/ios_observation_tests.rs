use super::*;
use frontcap_core::native::ios::ClockIdentity;
#[test]
fn observation_files_reject_changes_links_and_path_escape() {
    let root = tempfile::tempdir().unwrap();
    let capture = "observations/abcd0000-0000-0000-0000-000000000004";
    std::fs::create_dir_all(root.path().join(capture)).unwrap();
    let artifacts: Vec<_> = ["screen.png", "hierarchy.txt"]
        .iter()
        .map(|name| {
            let raw = name.as_bytes();
            let path = format!("{capture}/{name}");
            std::fs::write(root.path().join(&path), raw).unwrap();
            Artifact {
                path,
                bytes: raw.len() as u64,
                sha256: Sha256::digest(raw)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            }
        })
        .collect();
    let mut outcome = UiOutcome::ObservationUnavailable {
        app: ClockIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 123,
        },
        now_ns: 0,
        reason: frontcap_core::native::ios_ui::UnavailableReason::CaptureChanged,
        artifacts,
    };
    verify(&outcome, root.path()).unwrap();
    let screen = root.path().join(format!("{capture}/screen.png"));
    std::fs::write(&screen, b"changed").unwrap();
    assert!(verify(&outcome, root.path()).is_err());
    std::fs::remove_file(&screen).unwrap();
    let other = root.path().join("other");
    std::fs::write(&other, b"screen.png").unwrap();
    std::os::unix::fs::symlink(&other, &screen).unwrap();
    assert!(verify(&outcome, root.path()).is_err());
    std::fs::remove_file(&screen).unwrap();
    let name = std::ffi::CString::new(screen.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(verify(&outcome, root.path()).is_err());
    if let UiOutcome::ObservationUnavailable { artifacts, .. } = &mut outcome {
        artifacts[0].path = "../other".into();
    }
    assert!(verify(&outcome, root.path()).is_err());
}
