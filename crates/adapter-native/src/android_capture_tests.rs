use super::*;
use frontcap_core::native::{
    android_capture::FILES,
    android_views::{Catalog, Role, View},
    observation::Artifact,
};
use serde_json::json;
const GENERATION: &str = "abcd0000-0000-0000-0000-000000000001";
const BEFORE: &str = "abcd0000-0000-0000-0000-000000000002";
const AFTER: &str = "abcd0000-0000-0000-0000-000000000003";
const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn owner() -> Owner<'static> {
    Owner {
        package: "org.example.client",
        activity: "org.example.client.Main",
        generation: GENERATION,
        pid: 123,
        source_sha256: SOURCE,
    }
}
fn seal(root: &Path, capture: &mut Capture) -> String {
    capture.artifacts = FILES
        .iter()
        .map(|name| {
            let raw = std::fs::read(root.join(name)).unwrap();
            Artifact {
                path: (*name).into(),
                bytes: raw.len() as u64,
                sha256: sha256_bytes_hex(&raw),
            }
        })
        .collect();
    let raw = serde_json::to_vec(capture).unwrap();
    std::fs::write(root.join("manifest.json"), &raw).unwrap();
    sha256_bytes_hex(&raw)
}
pub(crate) fn fixture(root: &Path) -> (Capture, String) {
    let catalog = Catalog {
        window_focused: true,
        ime_visible: Some(false),
        targets: vec![View {
            token: BEFORE.into(),
            class_name: "android.widget.Button".into(),
            role: Role::Button,
            resource_id: String::new(),
            label: String::new(),
            text: Some("Open".into()),
            secure: false,
            enabled: true,
            visible: true,
            focused: false,
            clickable: true,
            selected: false,
            transformed: false,
            error: None, scroll: None, tap_point_screen_px: None, edit_limit_utf16: None, layout_bounds_screen_px: [0, 0, 1, 1],
        }],
    };
    for (side, request) in [("before", BEFORE), ("after", AFTER)] {
        let observed = Observation::Observed {
            request: request.into(),
            generation: GENERATION.into(),
            pid: 123,
            now_ns: 0,
            catalog: catalog.clone(),
        };
        let files=[(format!("views-{side}.json"),serde_json::to_vec(&observed).unwrap()),
            (format!("views-{side}-request.json"),serde_json::to_vec(&json!({"request":request,"generation":GENERATION,"pid":123})).unwrap()),
            (format!("views-{side}-submission.txt"),b"Result: Bundle[{submitted=true}]\n".to_vec()),
            (format!("clock-{side}.json"),serde_json::to_vec(&json!({"generation":GENERATION,"pid":123,"now_ns":0,"status":"ready","last_id":0})).unwrap()),
            (format!("pid-{side}.txt"),b"123\n".to_vec()),
            (format!("window-{side}.txt"),b"  mCurrentFocus=Window{abc u0 org.example.client/.Main}\n".to_vec())];
        for (name, raw) in files {
            std::fs::write(root.join(name), raw).unwrap();
        }
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[1, 2, 3]).unwrap();
    }
    std::fs::write(root.join("screen.png"), png).unwrap();
    let mut capture = Capture {
        capture_id: GENERATION.into(),
        package: owner().package.into(),
        activity: owner().activity.into(),
        generation: GENERATION.into(),
        pid: 123,
        source_sha256: SOURCE.into(),
        now_ns: 0,
        before_request: BEFORE.into(),
        after_request: AFTER.into(),
        catalog,
        size: [1, 1],
        started_monotonic_ns: 1,
        finished_monotonic_ns: 2,
        artifacts: vec![],
    };
    let hash = seal(root, &mut capture);
    (capture, hash)
}
#[test]
fn complete_capture_requires_independent_owner_digest_and_unchanged_files() {
    let temp = tempfile::tempdir().unwrap();
    let (_, hash) = fixture(temp.path());
    verify(temp.path(), &owner(), &hash).unwrap();
    let mut wrong = owner();
    wrong.pid = 124;
    assert!(verify(temp.path(), &wrong, &hash).is_err());
    assert!(verify(temp.path(), &owner(), SOURCE).is_err());
    std::fs::write(temp.path().join("screen.png"), b"not PNG").unwrap();
    assert!(verify(temp.path(), &owner(), &hash).is_err());
}
#[test]
fn self_consistent_hashes_do_not_hide_changed_clock_foreground_or_invalid_png() {
    for name in ["clock-after.json", "window-after.txt", "screen.png"] {
        let temp = tempfile::tempdir().unwrap();
        let (mut capture, _) = fixture(temp.path());
        let bytes = match name {
            "clock-after.json" => serde_json::to_vec(
                &json!({"generation":GENERATION,"pid":123,"now_ns":1,"status":"ready","last_id":0}),
            )
            .unwrap(),
            "window-after.txt" => b"mCurrentFocus=Window{abc u0 dev.other/.Main}\n".to_vec(),
            _ => b"not a PNG".to_vec(),
        };
        std::fs::write(temp.path().join(name), bytes).unwrap();
        let hash = seal(temp.path(), &mut capture);
        assert!(verify(temp.path(), &owner(), &hash).is_err(), "{name}");
    }
}
#[test]
fn symlinked_artifacts_are_not_valid_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let (_, hash) = fixture(temp.path());
    let from = temp.path().join("screen.png");
    let target = temp.path().join("other.png");
    std::fs::rename(&from, &target).unwrap();
    std::os::unix::fs::symlink(&target, &from).unwrap();
    assert!(verify(temp.path(), &owner(), &hash).is_err());
}

#[test]
fn supervised_collector_verifies_real_python_output_and_keeps_failed_attempts() {
    use crate::android_collection::Collector;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    fixture(&source);
    let adb = root.join("adb");
    std::fs::write(&adb, r#"#!/usr/bin/env python3
import json,sys
from pathlib import Path
root=Path(__file__).parent
args=sys.argv[1:]
assert args[:2]==['-s','emulator-5580']
args=args[2:]
with (root/'calls').open('a') as log: log.write(json.dumps(args)+'\n')
source=root/'source'
if args==['shell','pidof','org.example.client']: raw=(source/'pid-before.txt').read_bytes()
elif args==['shell','run-as','org.example.client','cat','files/clock-control.json']: raw=(source/'clock-before.json').read_bytes()
elif args[:3]==['shell','content','call']:
 assert args[args.index('--uri')+1]=='content://org.example.client.views'
 request=next(a.split(':',2)[2] for a in args if a.startswith('request:s:'))
 (root/'request').write_text(request)
 raw=b'Result: Bundle[{submitted=true}]\n'
elif args==['shell','run-as','org.example.client','cat','files/view-observation.json']:
 value=json.loads((source/'views-before.json').read_bytes())
 value['request']=(root/'request').read_text()
 raw=json.dumps(value).encode()
elif args==['shell','dumpsys','window']: raw=(source/'window-before.txt').read_bytes()
elif args==['exec-out','screencap','-p']: raw=(source/'screen.png').read_bytes()
else: raise RuntimeError(args)
sys.stdout.buffer.write(raw)
"#).unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_android_view_capture.py");
    let collector = Collector {
        python: Path::new("/usr/bin/python3"),
        script: &script,
        adb: &adb,
        serial: "emulator-5580",
    };
    let cancelled = AtomicBool::new(false);
    let result = collector
        .collect(
            &owner(),
            &root.join("success"),
            Instant::now() + Duration::from_secs(10),
            &cancelled,
        )
        .unwrap();
    assert_eq!(result.catalog.targets[0].text.as_deref(), Some("Open"));
    assert_eq!(result.size, [1, 1]);
    assert!(root.join("success/exchange/exchange.json").is_file());
    std::fs::write(source.join("pid-before.txt"), b"456\n").unwrap();
    let failed = root.join("failed");
    let error = collector
        .collect(
            &owner(),
            &failed,
            Instant::now() + Duration::from_secs(10),
            &cancelled,
        )
        .unwrap_err();
    assert!(error.cleanup_failure.is_none());
    assert!(failed.join("capture/progress.json").is_file());
    assert!(!failed.join("capture/manifest.json").exists());
    let calls = std::fs::read(root.join("calls")).unwrap();
    assert!(
        collector
            .collect(
                &owner(),
                &failed,
                Instant::now() + Duration::from_secs(10),
                &cancelled
            )
            .is_err()
    );
    assert_eq!(std::fs::read(root.join("calls")).unwrap(), calls);
}

#[test]
fn changed_source_is_verifiable_diagnostic_evidence_but_never_an_actionable_capture() {
    let temp=tempfile::tempdir().unwrap();let (mut capture,hash)=fixture(temp.path());
    assert!(verify_collection(temp.path(),&owner(),&hash,true).is_err());
    let path=temp.path().join("views-after.json");
    let mut value:Observation=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Observation::Observed {catalog,..}=&mut value {catalog.ime_visible=Some(true);}
    std::fs::write(&path,serde_json::to_vec(&value).unwrap()).unwrap();let hash=seal(temp.path(),&mut capture);
    assert!(verify(temp.path(),&owner(),&hash).is_err());
    assert!(matches!(verify_collection(temp.path(),&owner(),&hash,true).unwrap(),Collection::Unavailable(_)));
    let mut foreign=owner();foreign.pid+=1;
    assert!(verify_collection(temp.path(),&foreign,&hash,true).is_err());
    std::fs::write(temp.path().join("clock-after.json"),br#"{"generation":"abcd0000-0000-0000-0000-000000000001","pid":123,"now_ns":1,"status":"ready"}"#).unwrap();
    let hash=seal(temp.path(),&mut capture);
    assert!(verify_collection(temp.path(),&owner(),&hash,true).is_err());
}
