use super::*;

#[test]
fn speech_receipts_recover_without_dispatch_and_reject_changed_evidence_or_owner() {
    for sampling in [false, true] {
    let mut fixture = crate::device::tests::Fixture::new();
    fixture.config.host_app = Some(crate::android_webview::Profile {
        package: "org.example.app".into(), activity: "org.example.app.ClientActivity".into(), local_service: None,
    });
    let root = fixture.root().join("speech-session");
    let attached_id = RequestId::new("speech").unwrap();
    let read_id = RequestId::new("speech-read").unwrap();
    let attach_dir = root.join("requests/speech");
    let read_dir = root.join("requests/speech-read");
    std::fs::create_dir_all(&attach_dir).unwrap();
    std::fs::create_dir(&read_dir).unwrap();
    let config = Config { device:fixture.config.clone(), entry_url:String::new(), lifetime_ms:30_000, input:None };
    let owner = AppProcess { emulator:ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package:"org.example.app".into(), activity:"org.example.app.ClientActivity".into(),
        serial:format!("emulator-{}", config.device.port), pid:123, process_start_ticks:42,
        apk_sha256:config.device.apk_sha256.clone(), source_sha256:config.device.source_sha256.clone() };
    let identity = android_speech::Attachment { id:"12345678-1234-1234-1234-123456789abc".into(), start_ticks:42 };
    let library = b"retained fixture library";
    let digest = frontcap_core::canonical::sha256_bytes_hex(library);
    let attachment = host::SpeechAttachment { id:attached_id.clone(), owner:owner.clone(), identity:identity.clone(),
        directory:format!("/data/user/0/{}/files/frontcap-speech-{}",owner.package,identity.id), library_sha256:digest.clone() };
    let attach_op = host::Operation::AttachSpeech { owner:Box::new(owner.clone()), library:root.join("source.so"), sha256:digest };
    let read_op = host::Operation::ReadSpeech { attachment:attached_id.clone() };
    save_atomic(&root.join("launch.json"),&Launch { config, deadline_ns:1 }).unwrap();
    save_atomic(&root.join("host-ready.json"),&host::Ready { owner:owner.clone(), contacts:None }).unwrap();
    for (path, operation) in [(&attach_dir,&attach_op),(&read_dir,&read_op)] {
        save_atomic(&path.join("operation.json"),&Operation::Host { request:operation.clone() }).unwrap();
        save_atomic(&path.join("host-binding.json"),&epochs::Binding { owner:owner.clone(), epoch:None }).unwrap();
    }
    let mut raw = format!("{{\"event\":\"attach\",\"pid\":123,\"get_env\":0,\"attachment\":\"{}\",\"start_ticks\":42}}\n",identity.id);
    raw += "{\"event\":\"bind\",\"kind\":\"speak\",\"local_table_error\":0,\"identity_slot\":2,\"breakpoint_error\":0,\"rate_slot\":-1,\"format_slot\":-1,\"channels_slot\":-1,\"audio_slot\":-1,\"text_slot\":8,\"params_slot\":10}\n";
    raw += "{\"event\":\"ready\",\"pid\":123,\"boundaries\":1,\"flow_only\":true}\n";
    if sampling { raw = raw.replace("\"flow_only\":true", "\"flow_only\":true,\"sampling\":true"); }
    let history = android_speech::History::from_bound_jsonl(&raw,123,&identity).unwrap();
    for path in [&attach_dir,&read_dir] {
        std::fs::write(path.join("speech-read.jsonl"),&raw).unwrap();
        let mut history = history.clone();
        if sampling {
            let key = frontcap_core::canonical::sha256_bytes_hex(path.file_name().unwrap().to_str().unwrap().as_bytes())[..32].to_owned();
            let sample = android_speech::Sample { request:key.clone(), attachment:identity.id.clone(), pid:owner.pid,
                start_ticks:owner.process_start_ticks, request_index:1, sequence_before:0, sequence_after:0,
                began_ns:1, ended_ns:2, faults:0, reference_limit_reached:false, synthesizers:vec![] };
            let bytes = serde_json::to_string(&sample).unwrap();
            std::fs::write(path.join("speech-state.json"),&bytes).unwrap();
            save_atomic(&path.join("speech-sample-request.json"),&(key.clone(),1_u64)).unwrap();
            history.apply_sample(&bytes,&key,1).unwrap();
        }
        save_atomic(&path.join("speech-read.json"),&(&attachment,&history)).unwrap();
    }
    std::fs::write(attach_dir.join("speech-observer.so"),library).unwrap();
    // A read alone does not establish that attachment success was published durably.
    assert!(super::super::read_result(&root,&attached_id,&attach_op).unwrap().is_none());
    save_atomic(&attach_dir.join("speech-attached.json"),&attachment).unwrap();
    for (id, operation) in [(&attached_id,&attach_op),(&read_id,&read_op)] {
        let first = super::super::read_result(&root,id,operation).unwrap().unwrap();
        assert!(matches!(&first,Reply::Completed { .. }));
        let second = super::super::read_result(&root,id,operation).unwrap().unwrap();
        assert_eq!(serde_json::to_value(first).unwrap(),serde_json::to_value(second).unwrap());
        assert!(!root.join("requests").join(id.as_str()).join("result.json").exists());
    }
    let replacement = AppProcess { process_start_ticks:43, ..owner.clone() };
    assert!(super::attachment(&root,&attached_id,&replacement).is_err());
    if sampling {
        let intent = read_dir.join("speech-sample-request.json");
        let original: (String,u64) = load(&intent).unwrap().unwrap();
        for changed in [(original.0.clone(), original.1 + 1), ("0".repeat(32), original.1)] {
            save_atomic(&intent,&changed).unwrap();
            assert!(super::super::read_result(&root,&read_id,&read_op).is_err());
        }
        save_atomic(&intent,&original).unwrap();
        assert!(super::super::read_result(&root,&read_id,&read_op).unwrap().is_some());
    }
    let (_, history): (host::SpeechAttachment, android_speech::History) = load(&read_dir.join("speech-read.json")).unwrap().unwrap();
    let mut changed = history.clone(); changed.faults += 1;
    save_atomic(&read_dir.join("speech-read.json"),&(&attachment,&changed)).unwrap();
    assert!(super::super::read_result(&root,&read_id,&read_op).is_err());
    save_atomic(&read_dir.join("speech-read.json"),&(&attachment,&history)).unwrap();
    std::fs::write(read_dir.join("speech-read.jsonl"),raw.replace("start_ticks\":42", "start_ticks\":43")).unwrap();
    assert!(super::super::read_result(&root,&read_id,&read_op).is_err());
    std::fs::write(attach_dir.join("speech-observer.so"),b"changed").unwrap();
    assert!(super::super::read_result(&root,&attached_id,&attach_op).is_err());
    }
}
