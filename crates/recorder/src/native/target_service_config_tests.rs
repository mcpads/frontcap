fn assert_resource_root(launch: &Launch, record: &frontcap_service_fixture::Record) {
    let mut resources = launch.clone();
    resources.configure_resources(Some(record)).unwrap();
    match resources {
        Launch::IosHost(_) | Launch::AndroidHost(_) | Launch::AndroidWebview(_) => panic!("Android WebView uses its frozen local-service profile"),
        Launch::Webview(config) => assert_eq!(config.app.environment["APP_SERVICE_URL"], "http://127.0.0.1:54321/"),
        Launch::Ios(config) => assert_eq!(config.app_environment["APP_SERVICE_URL"], "http://127.0.0.1:54321/"),
        Launch::Android(config) => assert_eq!(config.view_app.unwrap().launch_extras["app.service_url"], "http://10.0.2.2:54321/"),
        Launch::Flutter(config) => {
            let host = match config.worker.config.platform {
                frontcap_core::native::flutter::Platform::Android => "10.0.2.2",
                frontcap_core::native::flutter::Platform::Ios => "127.0.0.1",
            };
            assert_eq!(config.worker.config.app_defines["APP_SERVICE_URL"], format!("http://{host}:54321/"));
        }
    }
}

use super::*;
#[test]
fn webview_profile_separates_runtime_and_routes_owned_resources() {
    let config: frontcap_native::webview_session::Config = serde_json::from_str(include_str!("../../tests/fixtures/native/webview-launch.json")).unwrap();
    let launch = Launch::Webview(Box::new(config));
    let encoded = serde_json::to_string(&launch).unwrap();
    assert!(encoded.contains("\"runtime\":\"webview\""));
    assert!(serde_json::from_str::<Launch>(&encoded.replace("\"platform\":\"ios\"", "\"platform\":\"android\"")).is_err());
    let mut decoded: Launch = serde_json::from_str(&encoded).unwrap();
    let Launch::Webview(config) = &mut decoded else { panic!() };
    config.service_url_environment = Some("APP_SERVICE_URL".into());
    assert!(decoded.clone().configure_resources(None).is_err());
    let record = frontcap_service_fixture::Record { generation: frontcap_core::ids::LaunchId::new("g-service").unwrap(), request_path:None, address:"127.0.0.1:54321".parse().unwrap(),pid:1,stopping:false,requests:Default::default(),releases:Default::default() };
    assert_resource_root(&decoded, &record);
    decoded.configure_service(Some(&record)).unwrap();
    assert!(decoded.configure_service(Some(&record)).is_err());
}
#[test]
fn uikit_service_environment_uses_simulator_route_and_rejects_conflicts() {
    let mut launch: Launch = serde_json::from_value(serde_json::json!({
        "platform":"ios", "config": {
            "surface":"uikit", "service_url_environment":"APP_SERVICE_URL",
            "app_environment":{"LABEL":"한글"}, "python":"/python",
            "ui_script":"/ui.py", "clock_script":"/clock.py", "controller_id":"controller",
            "xctest": {"app":{"bundle": "org.example.client", "runner_bundle": "org.example.client.tests.xctrunner", "test_identifier": "ClientTests/Bridge/testSession", "ui_descriptor": "Documents/ui-control.json", "clock_descriptor": "Documents/clock-control.json"},"xcodebuild":"/xcodebuild", "products":"/products",
                "manifest":{"names":{"app": "Client", "tests": "ClientTests"},"xctestrun":"tests.xctestrun", "directories":[], "files":{}},
                "simulator":{"xcrun":"/xcrun", "runtime":"runtime", "device_type":"phone", "source_sha256":"a".repeat(64)}}
        }
    })).unwrap();
    assert!(launch.clone().configure_service(None).is_err());
    let record = frontcap_service_fixture::Record {
        generation: frontcap_core::ids::LaunchId::new("g-service").unwrap(),
        request_path:None, address:"127.0.0.1:54321".parse().unwrap(), pid:1, stopping:false,
        requests:Default::default(), releases:Default::default(),
    };
    assert_resource_root(&launch, &record);
    launch.configure_service(Some(&record)).unwrap();
    let Launch::Ios(config) = &launch else { panic!() };
    assert_eq!(config.app_environment["APP_SERVICE_URL"], "http://127.0.0.1:54321/g-service/requests/");
    assert_eq!(config.app_environment["LABEL"], "한글");
    assert!(launch.configure_service(Some(&record)).is_err());
}
#[test]
fn android_service_extra_requires_service_and_preserves_explicit_configuration() {
    let mut launch: Launch = serde_json::from_value(serde_json::json!({
        "platform":"android", "config": {
            "emulator":"/emulator", "adb":"/adb", "avd":"test", "port":5580,
            "apk":"/client.apk", "apk_sha256":"a".repeat(64), "python":"/python",
            "bridge_script":"/bridge.py", "source_sha256":"b".repeat(64),
            "view_app": {
                "package":"org.example.client", "activity":"org.example.client.Main",
                "capture_script":"/capture.py", "clock_script":"/clock.py",
                "edit_script":"/edit.py", "tap_script":"/tap.py", "scroll_script":"/scroll.py",
                "service_url_extra":"app.service_url", "launch_extras":{"label":"한글"}
            }
        }
    })).unwrap();
    assert!(launch.clone().configure_service(None).is_err());
    let record = frontcap_service_fixture::Record {
        generation: frontcap_core::ids::LaunchId::new("g-service").unwrap(),
        request_path:None, address: "127.0.0.1:54321".parse().unwrap(), pid: 1, stopping: false,
        requests: Default::default(), releases: Default::default(),
    };
    assert_resource_root(&launch, &record);
    launch.configure_service(Some(&record)).unwrap();
    let Launch::Android(config) = &launch else { panic!() };
    let profile = config.view_app.as_ref().unwrap();
    assert_eq!(profile.launch_extras["app.service_url"], "http://10.0.2.2:54321/g-service/requests/");
    assert_eq!(profile.launch_extras["label"], "한글");
    profile.validate().unwrap();
    let before = serde_json::to_value(&launch).unwrap();
    assert!(launch.configure_service(Some(&record)).is_err());
    assert_eq!(serde_json::to_value(&launch).unwrap(), before);
}
#[test]
fn service_definition_uses_owned_generation_and_platform_host_route() {
    for (platform, host) in [("android", "10.0.2.2"), ("ios", "127.0.0.1")] {
        let mut launch: Launch = serde_json::from_value(serde_json::json!({
            "runtime":"flutter", "platform":platform, "config": {
                "http_script":"/bridge.py", "service_url_define":"APP_SERVICE_URL",
                "worker": {"python":"/python", "worker_script":"/worker.py", "config": {
                    "flutter":"/flutter", "project":"/client", "platform":platform,
                    "source_sha256":"a".repeat(64), "acquisition_timeout_s":30, "lifetime_s":30
                }}
            }
        })).unwrap();
        assert!(launch.clone().configure_service(None).is_err());
        let record = frontcap_service_fixture::Record {
            generation: frontcap_core::ids::LaunchId::new("g-service").unwrap(),
            request_path:None, address: "127.0.0.1:54321".parse().unwrap(), pid: 1, stopping: false,
            requests: Default::default(), releases: Default::default(),
        };
        assert_resource_root(&launch, &record);
    launch.configure_service(Some(&record)).unwrap();
        let Launch::Flutter(config) = &launch else { panic!() };
        assert_eq!(config.worker.config.app_defines["APP_SERVICE_URL"], format!("http://{host}:54321/g-service/requests/"));
        assert!(launch.configure_service(Some(&record)).is_err(), "never overwrite an explicit app definition");
    }
}
