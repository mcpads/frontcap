use super::*;
#[test]
fn flutter_system_ui_requires_its_advertised_ios_runner_and_never_grants_app_clock_access() {
    use frontcap_core::native::{flutter::{Owner, Platform}, ios_ui::UiIdentity, ios_request::Operation};
    let mut ready = Ready::Flutter { restart: false, app_lifecycle: false, owner: Owner { platform: Platform::Ios,
        device: "ABCD0000-0000-0000-0000-000000000001".into(), generation: "a".repeat(48), source_sha256: "b".repeat(64) },
        external_url_target: None, system_ui: None };
    let observe = Op::ExecuteIos { operation: Operation::ObserveSystemAlerts };
    assert!(ready.validate_operation(&observe).is_err());
    if let Ready::Flutter { system_ui, .. } = &mut ready {
        *system_ui = Some(UiIdentity { simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(), pid: 123 });
    }
    assert!(ready.validate_operation(&observe).is_ok());
    assert!(ready.validate_operation(&Op::ExecuteIos { operation: Operation::ReadClock }).is_err());
    if let Ready::Flutter { owner, .. } = &mut ready { owner.platform = Platform::Android; }
    assert!(ready.validate_operation(&observe).is_err());
    if let Ready::Flutter { owner, .. } = &mut ready { owner.platform = Platform::Ios; owner.device = "abcd0000-0000-0000-0000-000000000009".into(); }
    assert!(ready.validate_operation(&observe).is_err());
}

#[test]
fn flutter_host_visibility_requires_capability_and_current_generation() {
    use frontcap_core::native::{app_state::State, flutter::{Owner, Platform}};
    for (platform, device) in [(Platform::Android, "emulator-5580"),
        (Platform::Ios, "ABCD0000-0000-0000-0000-000000000001")] {
        let owner = Owner { platform, device: device.into(), generation: "a".repeat(48), source_sha256: "b".repeat(64) };
        let mut ready = Ready::Flutter { owner: owner.clone(), restart: true, app_lifecycle: false,
            external_url_target: None, system_ui: None };
        let operation = Op::SetAppState {generation:owner.generation.clone(),state:State::Background};
        assert!(ready.validate_operation(&operation).is_err());
        if let Ready::Flutter {app_lifecycle,..} = &mut ready { *app_lifecycle = true; }
        ready.validate_operation(&operation).unwrap();
        assert!(ready.validate_operation(&Op::SetAppState {generation:"c".repeat(48),state:State::Foreground}).is_err());
        let legacy: Ready = serde_json::from_value(serde_json::json!({"platform":"flutter","owner":owner,"restart":true})).unwrap();
        assert!(legacy.validate_operation(&operation).is_err());
    }
}

#[test]
fn flutter_external_entry_requires_advertised_application_and_current_host_owner() {
    use frontcap_core::native::{external_entry::{Request, Target}, flutter::{Owner, Platform}};
    for (platform, device, target) in [
        (Platform::Android, "emulator-5580", Target::Android { serial: "emulator-5580".into(), package: "org.example.client".into() }),
        (Platform::Ios, "ABCD0000-0000-0000-0000-000000000001", Target::Ios { simulator: "abcd0000-0000-0000-0000-000000000001".into(), bundle: "org.example.client".into() }),
    ] {
        let owner = Owner { platform, device: device.into(), generation: "a".repeat(48), source_sha256: "b".repeat(64) };
        let ready = Ready::Flutter { restart: false, app_lifecycle: false, system_ui: None, owner: owner.clone(), external_url_target: Some(target.clone()) };
        let request = Request { target, generation: owner.generation.clone(), url: "client-test://item/0?query=true#missing".into() };
        assert!(ready.validate_operation(&Op::DispatchUrl { request: request.clone() }).is_ok());
        let legacy: Ready = serde_json::from_value(serde_json::json!({"platform":"flutter", "owner": owner})).unwrap();
        assert!(legacy.validate_operation(&Op::DispatchUrl { request: request.clone() }).is_err());
        let mut changed = request.clone(); changed.generation = "c".repeat(48);
        assert!(ready.validate_operation(&Op::DispatchUrl { request: changed }).is_err());
        let mut changed = request.clone(); changed.url = "relative".into();
        assert!(ready.validate_operation(&Op::DispatchUrl { request: changed }).is_err());
        let mut wrong = owner; wrong.device = "other".into();
        assert!(Ready::Flutter { restart: false, app_lifecycle: false, system_ui: None, owner: wrong, external_url_target: Some(request.target.clone()) }
            .validate_operation(&Op::DispatchUrl { request }).is_err());
    }
}
#[test]
fn external_entry_admission_binds_the_current_app_device_and_generation() {
    use frontcap_core::native::external_entry::{Request, Target};
    let owner = frontcap_core::native::android_app::Owner {
        package: "org.example.client".into(), activity: "org.example.client.Main".into(),
        serial: "emulator-5580".into(), generation: "abcd0000-0000-0000-0000-000000000001".into(),
        pid: 123, source_sha256: "a".repeat(64), apk_sha256: "b".repeat(64),
    };
    let ready = Ready::AndroidViews { owner: owner.clone(), restart: false, app_lifecycle: false };
    let restart = Op::RestartAndroidViews {generation:owner.generation.clone(),url:None};
    assert!(ready.validate_operation(&restart).is_err());
    let enabled=Ready::AndroidViews {owner:owner.clone(),restart:true,app_lifecycle:true};
    let visibility = Op::SetAppState { generation: owner.generation.clone(), state: frontcap_core::native::app_state::State::Background };
    assert!(ready.validate_operation(&visibility).is_err());
    assert!(enabled.validate_operation(&visibility).is_ok());
    assert!(enabled.validate_operation(&Op::SetAppState { generation: "abcd0000-0000-0000-0000-000000000099".into(), state: frontcap_core::native::app_state::State::Foreground }).is_err());
    assert!(enabled.validate_operation(&restart).is_ok());
    assert!(enabled.validate_operation(&Op::RestartAndroidViews {generation:owner.generation.clone(),url:Some("client-test://item/1".into())}).is_ok());
    assert!(enabled.validate_operation(&Op::RestartAndroidViews {generation:owner.generation.clone(),url:Some("relative/path".into())}).is_err());
    assert!(serde_json::to_value(&restart).unwrap().get("url").is_none());
    assert!(enabled.validate_operation(&Op::RestartAndroidViews {generation:"abcd0000-0000-0000-0000-000000000099".into(),url:None}).is_err());
    let legacy:Ready=serde_json::from_value(serde_json::json!({"platform":"android_views","owner":owner})).unwrap();
    assert!(legacy.validate_operation(&restart).is_err());
    let request = Request { target: Target::Android { serial: owner.serial, package: owner.package },
        generation: owner.generation, url: "client-test://item/0?edit=true#missing".into() };
    assert!(ready.validate_operation(&Op::DispatchUrl { request: request.clone() }).is_ok());
    let mut changes = vec![request.clone(); 4];
    changes[0].generation = "abcd0000-0000-0000-0000-000000000002".into();
    changes[1].target = Target::Android { serial: "emulator-5582".into(), package: "org.example.client".into() };
    changes[2].target = Target::Android { serial: "emulator-5580".into(), package: "dev.other.client".into() };
    changes[3].url = "relative/path".into();
    for request in changes { assert!(ready.validate_operation(&Op::DispatchUrl { request }).is_err()); }
    let flutter = Ready::Flutter { restart: false, app_lifecycle: false, system_ui: None, external_url_target: None, owner: frontcap_core::native::flutter::Owner {
        platform: frontcap_core::native::flutter::Platform::Android, device: "emulator-5580".into(),
        generation: "a".repeat(48), source_sha256: "b".repeat(64),
    } };
    assert!(flutter.validate_operation(&Op::DispatchUrl { request }).is_err());
}

#[test]
fn uikit_external_entry_requires_advertised_bundle_and_app_generation() {
    use frontcap_core::native::{external_entry::{Request, Target},
        ios::ClockIdentity, ios_ui::UiIdentity, ios_request::Owner};
    let simulator = "abcd0000-0000-0000-0000-000000000001";
    let owner = Owner {
        app: ClockIdentity { simulator: simulator.to_ascii_uppercase(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(), pid: 42 },
        runner: UiIdentity { simulator: simulator.to_ascii_uppercase(),
            generation: "abcd0000-0000-0000-0000-000000000003".into(), pid: 41 },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(), source_sha256: "a".repeat(64),
    };
    let lifecycle = Ready::Ios { app_lifecycle: true, cold_entry: false, restart: false, owner: owner.clone(), external_url_bundle: None, system_alerts: false };
    let change = Op::SetAppState { generation: owner.app.generation.clone(), state: frontcap_core::native::app_state::State::Background };
    lifecycle.validate_operation(&change).unwrap();
    assert!(lifecycle.validate_operation(&Op::SetAppState { generation: owner.runner.generation.clone(), state: frontcap_core::native::app_state::State::Foreground }).is_err());
    assert!(lifecycle.validate_operation(&Op::ExecuteIos { operation: frontcap_core::native::ios_request::Operation::SetAppState { generation: owner.app.generation.clone(), state: frontcap_core::native::app_state::State::Background } }).is_err());
    let legacy: Ready = serde_json::from_value(serde_json::json!({"platform":"ios", "owner": owner})).unwrap();
    assert!(legacy.validate_operation(&change).is_err());
    let bundle = "org.example.client".to_string();
    let request = Request { target: Target::Ios { simulator: simulator.into(), bundle: bundle.clone() },
        generation: owner.app.generation.clone(), url: "client-test://item/0?edit=true#missing".into() };
    let ready = Ready::Ios {app_lifecycle:false, cold_entry: false, restart: false, owner: owner.clone(), external_url_bundle: Some(bundle.clone()), system_alerts: false };
    assert!(ready.validate_operation(&Op::DispatchUrl { request: request.clone() }).is_ok());
    let restart = Op::RestartUikit { generation: owner.app.generation.clone(), url: None };
    assert!(ready.validate_operation(&restart).is_err());
    let enabled = Ready::Ios {app_lifecycle:false, cold_entry: false, restart: true, owner: owner.clone(), external_url_bundle: Some(bundle.clone()), system_alerts: false };
    assert!(enabled.validate_operation(&restart).is_ok());
    assert!(enabled.validate_operation(&Op::RestartUikit { generation: owner.runner.generation.clone(), url: None }).is_err());
    assert!(Ready::Ios {app_lifecycle:false, cold_entry: false, restart: true, owner: owner.clone(), external_url_bundle: None, system_alerts: false }.validate_operation(&restart).is_err());
    let cold=Op::RestartUikit {generation:owner.app.generation.clone(),url:Some(request.url.clone())};
    assert!(enabled.validate_operation(&cold).is_err());
    let enabled_entry=Ready::Ios {app_lifecycle:false,cold_entry:true,restart:true,owner:owner.clone(),external_url_bundle:Some(bundle.clone()),system_alerts:false};
    enabled_entry.validate_operation(&cold).unwrap();
    assert!(enabled_entry.validate_operation(&Op::RestartUikit {generation:owner.app.generation.clone(),url:Some("relative/path".into())}).is_err());
    let mut changes = vec![request.clone(); 4];
    changes[0].generation = owner.runner.generation.clone();
    changes[1].target = Target::Ios { simulator: "abcd0000-0000-0000-0000-000000000009".into(), bundle };
    changes[2].target = Target::Ios { simulator: simulator.into(), bundle: "dev.other.client".into() };
    changes[3].url = "relative/path".into();
    for request in changes { assert!(ready.validate_operation(&Op::DispatchUrl { request }).is_err()); }
    let legacy: Ready = serde_json::from_value(serde_json::json!({"platform":"ios", "owner": owner})).unwrap();
    assert!(legacy.validate_operation(&Op::DispatchUrl { request }).is_err());
    assert!(legacy.validate_operation(&Op::ExecuteIos { operation: frontcap_core::native::ios_request::Operation::ReadClock }).is_ok());
    let system = Op::ExecuteIos { operation: frontcap_core::native::ios_request::Operation::ObserveSystemAlerts };
    assert!(legacy.validate_operation(&system).is_err());
    let advertised = Ready::Ios {app_lifecycle:false, cold_entry: false, restart: false, owner, external_url_bundle: None, system_alerts: true };
    assert!(advertised.validate_operation(&system).is_ok());
}

#[test]
fn view_owner_admits_clock_and_observed_input_from_the_current_view() {
    let owner=frontcap_core::native::android_app::Owner {
        package:"org.example.client".into(),activity:"org.example.client.Main".into(),
        serial:"emulator-5580".into(),generation:"abcd0000-0000-0000-0000-000000000001".into(),
        pid:123,source_sha256:"a".repeat(64),apk_sha256:"b".repeat(64),
    };
    let ready=Ready::AndroidViews {owner:owner.clone(),restart:false,app_lifecycle:false};
    assert!(ready.validate_operation(&Op::Observe).is_ok());
    assert!(ready.validate_operation(&Op::Stop).is_ok());
    assert!(ready.validate_operation(&Op::Execute {operation:Operation::ReadClock {}}).is_ok());
    assert!(ready.validate_operation(&Op::Execute {operation:Operation::Advance {by_us:1000}}).is_ok());
    let edit=frontcap_core::native::android_edit::Edit {observation:frontcap_core::ids::RequestId::new("basis").unwrap(),
        source_observation:owner.generation.clone(),reference:owner.generation.clone(),now_ns:0,
        before:"한글😀abc".into(),start:4,end:7,text:"수정".into()};
    let operation=Operation::ReplaceViewText {edit:edit.clone()};
    assert!(ready.validate_operation(&Op::Execute {operation:operation.clone()}).is_ok());
    let mut split=edit;split.start=3;
    assert!(ready.validate_operation(&Op::Execute {operation:Operation::ReplaceViewText {edit:split}}).is_err());
    let tap=Operation::TapView {tap:frontcap_core::native::android_tap::Tap {
        observation:frontcap_core::ids::RequestId::new("basis").unwrap(),source_observation:owner.generation.clone(),
        reference:owner.generation.clone(),now_ns:0,point:[100,200]}};
    assert!(ready.validate_operation(&Op::Execute {operation:tap.clone()}).is_ok());
    let scroll=Operation::ScrollView {scroll:frontcap_core::native::android_scroll::Scroll {
        observation:frontcap_core::ids::RequestId::new("basis").unwrap(),source_observation:owner.generation.clone(),
        reference:owner.generation.clone(),now_ns:0,before_offset_px:[0,0],gesture:frontcap_core::native::android_scroll::Gesture {
            direction:frontcap_core::native::android_scroll::Direction::Down,start_px:[100,200],end_px:[100,100]}}};
    assert!(ready.validate_operation(&Op::Execute {operation:scroll.clone()}).is_ok());
    let mut invalid=owner;invalid.serial="emulator-5581".into();
    assert!(Ready::AndroidViews {owner:invalid,restart:false,app_lifecycle:false}.validate_operation(&Op::Observe).is_err());
}
