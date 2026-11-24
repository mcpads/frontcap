use super::*;
use frontcap_core::native::ios_host::{Operation, Ready};

#[test]
fn host_url_acceptance_recovers_without_redelivery_and_fences_uncertainty() {
    for lose_reply in [false, true] {
        let (fixture, mut launch) = crate::simulator_app::tests::setup("normal");
        launch.script = None;
        let root = fixture.root.path().canonicalize().unwrap();
        let config = Config { lifetime_ms: 900_000, simulator: fixture.config.clone(), app: launch.clone(), contacts: true, system_ui: None };
        let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap()
            .replace("    saved=json.loads((p/'session/target/requests/requests.json').read_text())\n    if 'event_count' in saved:saved=json.loads((p/'session/target/requests/events'/(str(saved['event_count']-1)+'.json')).read_text())\n    assert saved['invalidated_through_ui']==saved['next_ui']-1\n", "")
            .replace("elif args[1]=='bootstatus':pass", "elif args[1]=='io':pathlib.Path(args[-1]).write_bytes((p/'pixel.png').read_bytes())\nelif args[1]=='bootstatus':pass");
        std::fs::write(&fixture.config.xcrun, script).unwrap();
        plist::to_file_xml(launch.app.join("Info.plist"), &serde_json::json!({
            "CFBundleIdentifier":launch.bundle,"CFBundleURLTypes":[{"CFBundleURLSchemes":["owned"]}]
        })).unwrap();
        std::fs::write(root.join("apps.plist"), format!(r#"{{ "{}" = {{ CFBundleIdentifier = "{}"; Path = "{}"; }}; }}"#,
            launch.bundle, launch.bundle, launch.app.display())).unwrap();
        let mut encoder = png::Encoder::new(std::fs::File::create(root.join("pixel.png")).unwrap(), 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.write_header().unwrap().write_image_data(&[0, 0, 0]).unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        let end = Instant::now() + Duration::from_secs(20); let cancel = AtomicBool::new(false);
        let simulator = Simulator::acquire_at(fixture.config.clone(), &root.join("target/simulator"), end, &cancel, &root.join("host")).unwrap();
        let app = App::launch(simulator, launch.clone(), &root.join("target/app"), end, &cancel).unwrap();
        let native = root.join("Documents/frontcap-hid"); std::fs::create_dir_all(&native).unwrap();
        let endpoint = contacts::Endpoint { generation:"550e8400-e29b-41d4-a716-446655440000".into(),
            pid:app.process().pid,available:true,short_tap: false, observed_contacts:true,request_capacity:24 };
        save_atomic(&native.join("endpoint.json"), &endpoint).unwrap();
        save_atomic(&native.join("window.json"), &contacts::Window {generation:endpoint.generation.clone(),pid:endpoint.pid,
            token:"window".into(),width:1.0,height:1.0,scale:1.0,fullscreen:true}).unwrap();
        let mut target = Target::connect(app, &root.join("target/host"), true, end, &cancel).unwrap();
        let owner = target.owner().clone();
        save_atomic(&root.join("launch.json"), &serde_json::json!({"host":true,"deadline_ns":1,"config":{
            "simulator":fixture.config,"app":launch,"contacts":true,"lifecycle":null,"service_url_environment":null
        }})).unwrap();
        save_atomic(&root.join("host-ready.json"), &Ready { owner:owner.clone(),contacts:Some(endpoint),pages:false,app_lifecycle:false,system_ui:None }).unwrap();
        let basis = RequestId::new("before").unwrap();
        target.capture(basis.clone(),end,&cancel).unwrap();
        assert!(target.input.input_basis().is_some());
        let id = RequestId::new("entry").unwrap();
        let operation = Operation::DispatchUrl { owner:Box::new(owner.clone()),url:"owned://history".into() };
        // Syntax and ownership failures have no OS delivery and preserve the current capture.
        let mut foreign = owner.clone(); foreign.process.started.push_str("foreign");
        for rejected in [Operation::DispatchUrl {owner:Box::new(foreign),url:"owned://history".into()},
            Operation::DispatchUrl {owner:Box::new(owner.clone()),url:"relative".into()},
            Operation::DispatchUrl {owner:Box::new(owner.clone()),url:"https://example.com".into()}] {
            assert!(crate::ios_host_session::execute(&mut target,&root,&RequestId::new("invalid").unwrap(),&rejected,end,&cancel).is_err());
            assert!(target.input.input_basis().is_some());
            assert!(!root.join("url-effects").exists());
        }
        if lose_reply { std::fs::write(root.join("lose-url"), b"yes").unwrap(); }
        let result = crate::ios_host_session::execute(&mut target,&root,&id,&operation,end,&cancel);
        assert!(target.input.input_basis().is_none());
        assert_eq!(std::fs::read_to_string(root.join("url-effects")).unwrap().lines().count(),1);
        if lose_reply {
            assert!(result.is_err()); assert!(target.retired.as_ref().unwrap().passed);
            assert!(read_url(&root,&root.join("target/host"),&id,&owner,"owned://history").unwrap().is_none());
            assert!(crate::ios_host_session::execute(&mut target,&root,&id,&operation,end,&cancel).is_err());
        } else {
            let completion = result.unwrap();
            assert!(target.input(RequestId::new("stale").unwrap(),basis,contacts::Operation::Cancel {expected_revision:0},end,&cancel)
                .unwrap_err().to_string().contains("latest unconsumed capture"));
            let fresh = RequestId::new("fresh").unwrap(); target.capture(fresh.clone(),end,&cancel).unwrap();
            let calls = std::fs::read(root.join("calls")).unwrap();
            assert_eq!(crate::ios_host_session::execute(&mut target,&root,&id,&operation,end,&cancel).unwrap(),completion);
            assert_eq!(target.input.input_basis().unwrap().observation,fresh);
            assert_eq!(std::fs::read(root.join("calls")).unwrap(),calls);
            // A missing enclosing result recovers from OS acceptance, including after retirement.
            std::fs::remove_file(root.join("host-requests/entry/result.json")).unwrap();
            assert!(target.stop().passed);
            let calls = std::fs::read(root.join("calls")).unwrap();
            assert!(crate::ios_host_session::read_result(&root,&config,&id,&operation).unwrap().is_some());
            assert_eq!(crate::ios_host_session::execute(&mut target,&root,&id,&operation,end,&cancel).unwrap(),completion);
            assert_eq!(std::fs::read(root.join("calls")).unwrap(),calls);
            let proof: Proof = load(&root.join("target/host/urls/entry/receipt.json")).unwrap().unwrap();
            let command = root.join("target/simulator").join(proof.commands.keys().next().unwrap());
            std::fs::write(command,b"changed").unwrap();
            assert!(crate::ios_host_session::read_result(&root,&config,&id,&operation).is_err());
        }
        assert_eq!(std::fs::read_to_string(root.join("url-effects")).unwrap().lines().count(),1);
    }
}
