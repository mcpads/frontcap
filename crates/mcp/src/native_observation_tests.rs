use super::*;
#[test]
fn unavailable_read_returns_structured_result_without_requiring_or_inlining_a_png() {
    use frontcap_core::native::{ios::ClockIdentity,ios_ui::{UiCompletion,UiRequest,UiOperation,UnavailableReason},observation::Artifact};
    let temp=tempfile::tempdir().unwrap();
    let server=Frontcap { config:crate::config::Config { workspace:frontcap_recorder::launcher::Workspace { root:temp.path().into() },
        recorder_bin:"unused".into(),frontcap_home:"unused".into() },connections:Default::default() };
    let response=Response::Recorder {launch_id:LaunchId::new("g-test").unwrap(),result:Box::new(Reply::Native {
        body:NativeReply::Done {request_id:RequestId::new("observe-view").unwrap(),outcome:Box::new(Outcome::AndroidViewsUnavailable {
            observation:frontcap_core::native::android_capture::Unavailable {capture_id:"abcd0000-0000-0000-0000-000000000001".into(),
                generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:123,now_ns:0,started_monotonic_ns:1,finished_monotonic_ns:2,
                manifest_sha256:"a".repeat(64),reason:frontcap_core::native::android_capture::UnavailableReason::SourceChanged},
        })},
    })};
    let expected=serde_json::to_value(&response).unwrap();
    let rendered=serde_json::to_value(server.native_render_response(response,true)).unwrap();
    assert_eq!(rendered["isError"],false);assert_eq!(rendered["structuredContent"],expected);
    assert!(rendered["content"].as_array().unwrap().iter().all(|item|item["type"]!="image"));
    for reason in [UnavailableReason::ScrollInMotion,UnavailableReason::CaptureChanged] {
        let artifacts=if reason==UnavailableReason::CaptureChanged {
            ["screen.png","hierarchy.txt"].into_iter().map(|name| Artifact {
                path:format!("observations/abcd0000-0000-0000-0000-000000000009/{name}"),bytes:1,sha256:"a".repeat(64),
            }).collect()
        } else { vec![] };
        let generation="abcd0000-0000-0000-0000-000000000002".to_string();
        let completion=UiCompletion { request:UiRequest { generation:generation.clone(),request_id:2,operation:UiOperation::ObserveTargets {} },
            generation,pid:123,last_id:2,receipt_sha256:"a".repeat(64),outcome:UiOutcome::ObservationUnavailable {
                app:ClockIdentity { simulator:"abcd0000-0000-0000-0000-000000000001".into(),
                    generation:"abcd0000-0000-0000-0000-000000000003".into(),pid:124 },now_ns:0,reason,artifacts } };
        let response=Response::Recorder { launch_id:LaunchId::new("g-test").unwrap(),result:Box::new(Reply::Native {
            body:NativeReply::Done { request_id:RequestId::new("observe").unwrap(),outcome:Box::new(Outcome::IosCompleted {
                completion:IosCompletion::Ui(Box::new(completion)),
            }) },
        }) };
        let expected=serde_json::to_value(&response).unwrap();
        let rendered=serde_json::to_value(server.native_render_response(response,true)).unwrap();
        assert_eq!(rendered["isError"],false);
        assert_eq!(rendered["structuredContent"],expected);
        assert!(rendered["content"].as_array().unwrap().iter().all(|item| item["type"]!="image"));
    }
}

#[test]
fn view_inline_screen_is_bound_to_request_and_rejects_mutation_or_linked_paths() {
    use frontcap_core::native::{android_capture::{Capture,FILES},android_views::{Catalog,View,Role},observation::Artifact};
    let temp=tempfile::tempdir().unwrap();
    let server=Frontcap { config:crate::config::Config { workspace:frontcap_recorder::launcher::Workspace { root:temp.path().into() },
        recorder_bin:"unused".into(),frontcap_home:"unused".into() },connections:Default::default() };
    let launch=LaunchId::new("g-test").unwrap();let request=RequestId::new("view").unwrap();
    let generation="abcd0000-0000-0000-0000-000000000001";
    let bytes=b"bound screen bytes";
    let capture=Capture {capture_id:generation.into(),package:"org.example.client".into(),activity:"org.example.client.Main".into(),
        generation:generation.into(),pid:123,source_sha256:"a".repeat(64),now_ns:0,before_request:generation.into(),
        after_request:"abcd0000-0000-0000-0000-000000000002".into(),size:[1,1],started_monotonic_ns:1,finished_monotonic_ns:2,
        catalog:Catalog {window_focused:true,ime_visible:Some(false),targets:vec![View {token:generation.into(),
            class_name:"android.widget.Button".into(),role:Role::Button,resource_id:String::new(),label:String::new(),text:Some("Open".into()),
            secure:false,enabled:true,visible:true,focused:false,clickable:true,selected:false,transformed:false,error: None, scroll: None, tap_point_screen_px: None, edit_limit_utf16: None, layout_bounds_screen_px:[0,0,1,1]}]},
        artifacts:FILES.iter().map(|name|Artifact {path:(*name).into(),bytes:bytes.len() as u64,sha256:sha256_bytes_hex(bytes)}).collect()};
    let root=server.config.workspace.runtime_dir(&launch).join("native-session/views/view/collection/capture");
    std::fs::create_dir_all(&root).unwrap();let path=root.join("screen.png");std::fs::write(&path,bytes).unwrap();
    let owner=frontcap_core::native::android_app::Owner {
        package:capture.package.clone(),activity:capture.activity.clone(),serial:"emulator-5554".into(),
        generation:generation.into(),pid:capture.pid,source_sha256:capture.source_sha256.clone(),apk_sha256:"b".repeat(64),
    };
    frontcap_core::state::save_atomic(&server.config.workspace.runtime_dir(&launch).join("native-session/view-ready.json"),&owner).unwrap();
    assert_eq!(server.native_view_png(&launch,&request,&capture).unwrap(),bytes);
    assert!(server.native_view_png(&launch,&RequestId::new("another").unwrap(),&capture).is_err());
    std::fs::write(&path,b"changed").unwrap();assert!(server.native_view_png(&launch,&request,&capture).is_err());
    std::fs::remove_file(&path).unwrap();let outside=temp.path().join("outside");std::fs::write(&outside,bytes).unwrap();
    std::os::unix::fs::symlink(&outside,&path).unwrap();assert!(server.native_view_png(&launch,&request,&capture).is_err());
}

#[test]
fn native_host_inline_capture_rejects_foreign_requests_changed_bytes_and_links() {
    let temp=tempfile::tempdir().unwrap();
    let server=Frontcap { config:crate::config::Config { workspace:frontcap_recorder::launcher::Workspace { root:temp.path().into() },
        recorder_bin:"unused".into(),frontcap_home:"unused".into() },connections:Default::default() };
    let launch=LaunchId::new("g-host").unwrap(); let request=RequestId::new("host-capture").unwrap();
    let root=server.config.workspace.runtime_dir(&launch).join("native-session/requests/host-capture/capture");
    std::fs::create_dir_all(&root).unwrap(); let root=root.canonicalize().unwrap(); let path=root.join("screen.png");
    let bytes=b"\x89PNG\r\n\x1a\nretained screen"; let hash=sha256_bytes_hex(bytes);
    std::fs::write(&path,bytes).unwrap();
    assert_eq!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,true).unwrap(),bytes);
    assert!(server.native_page_png(&launch,&RequestId::new("foreign").unwrap(),&path,bytes.len() as u64,&hash,true).is_err());
    std::fs::write(&path,b"changed").unwrap();
    assert!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,true).is_err());
    std::fs::remove_file(&path).unwrap(); let outside=temp.path().join("outside.png"); std::fs::write(&outside,bytes).unwrap();
    std::os::unix::fs::symlink(outside,&path).unwrap();
    assert!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,true).is_err());
}

#[test]
fn ios_host_inline_capture_rejects_foreign_requests_changed_bytes_and_links() {
    let temp=tempfile::tempdir().unwrap();
    let server=Frontcap { config:crate::config::Config { workspace:frontcap_recorder::launcher::Workspace { root:temp.path().into() },
        recorder_bin:"unused".into(),frontcap_home:"unused".into() },connections:Default::default() };
    let launch=LaunchId::new("g-host").unwrap(); let request=RequestId::new("host-capture").unwrap();
    for relative in ["native-session/target/host/capture-host-capture", "native-session/host-epochs/restart/host/capture-host-capture"] {
    let root=server.config.workspace.runtime_dir(&launch).join(relative);
    std::fs::create_dir_all(&root).unwrap(); let root=root.canonicalize().unwrap(); let path=root.join("screen.png");
    let bytes=b"\x89PNG\r\n\x1a\nretained screen"; let hash=sha256_bytes_hex(bytes);
    std::fs::write(&path,bytes).unwrap();
    assert_eq!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,false).unwrap(),bytes);
    assert!(server.native_page_png(&launch,&RequestId::new("foreign").unwrap(),&path,bytes.len() as u64,&hash,false).is_err());
    std::fs::write(&path,b"changed").unwrap();
    assert!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,false).is_err());
    std::fs::remove_file(&path).unwrap(); let outside=temp.path().join("outside.png"); std::fs::write(&outside,bytes).unwrap();
    std::os::unix::fs::symlink(outside,&path).unwrap();
    assert!(server.native_page_png(&launch,&request,&path,bytes.len() as u64,&hash,false).is_err());
    }
}

#[test]
fn unresolved_native_operation_marks_tool_error_and_preserves_original_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let server = Frontcap {
        config: crate::config::Config {
            workspace: frontcap_recorder::launcher::Workspace { root: temp.path().into() },
            recorder_bin: "unused".into(),
            frontcap_home: "unused".into(),
        },
        connections: Default::default(),
    };
    for inline in [false, true] {
        for outcome in [
            Outcome::IosHostUnresolved { error: "attached page is no longer available".into() },
            Outcome::AndroidHostUnresolved { error: "replacement completion is unknown".into() },
            Outcome::AndroidWebviewUnresolved { error: "transition completion is unknown".into(), lifecycle: None },
            Outcome::WebviewUnresolved { error: "page completion is unknown".into() },
        ] {
            let response = Response::Recorder {
                launch_id: LaunchId::new("g-unresolved").unwrap(),
                result: Box::new(Reply::Native {
                    body: NativeReply::Done {
                        request_id: RequestId::new("original-command").unwrap(),
                        outcome: Box::new(outcome),
                    },
                }),
            };
            let expected = serde_json::to_value(&response).unwrap();
            let rendered = serde_json::to_value(server.native_render_response(response, inline)).unwrap();
            assert_eq!(rendered["isError"], true);
            assert_eq!(rendered["structuredContent"], expected);
            assert!(rendered["content"].as_array().unwrap().iter().all(|item| item["type"] != "image"));
        }
    }
}
