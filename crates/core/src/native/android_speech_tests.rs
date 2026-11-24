use super::*;

#[test]
fn android_speech_submission_rejection_is_not_playback_or_callback_failure() {
    let mut raw = header().replace("\"flow_only\":true", "\"flow_only\":true,\"submission_observed\":true");
    raw += &event(1, 1, Kind::Request, 1);
    let rejection = "{\"event\":\"submission\",\"request_sequence\":1,\"identity\":1,\"returned\":-1,\"threw\":false,\"observer_error\":0}\n";
    raw += rejection;
    // Same consumer token, separate call: the second accepted submission does not
    // erase the first rejection or imply a native start callback.
    raw += &event(2, 1, Kind::Request, 2);
    let pending = History::from_jsonl(&raw, 123).unwrap();
    assert_eq!(pending.pending_submissions, [2]);
    assert_eq!(pending.submissions[0].returned, Some(-1));
    assert_eq!(pending.tokens[0].starts, 0);
    assert_eq!(pending.tokens[0].failures, 0);
    assert_eq!(pending.tokens[0].correlation, Correlation::ReusedToken);
    raw += "{\"event\":\"submission\",\"request_sequence\":2,\"identity\":1,\"returned\":0,\"threw\":false,\"observer_error\":0}\n";
    let accepted = History::from_jsonl(&raw, 123).unwrap();
    assert!(accepted.pending_submissions.is_empty());
    assert_eq!(accepted.tokens[0].starts, 0);
    assert_eq!(accepted.submissions.len(), 2);
    for changed in [
        raw.clone() + rejection,
        raw.replace("\"request_sequence\":2", "\"request_sequence\":3"),
        raw.replace("\"identity\":1,\"returned\":0", "\"identity\":2,\"returned\":0"),
        raw.replace("\"returned\":-1", "\"returned\":null"),
        raw.replace("\"submission_observed\":true", "\"submission_observed\":false"),
    ] {
        assert!(History::from_jsonl(&changed, 123).is_err());
    }
    let unavailable = raw.replace("\"returned\":-1,\"threw\":false,\"observer_error\":0",
        "\"returned\":null,\"threw\":false,\"observer_error\":99");
    assert_eq!(History::from_jsonl(&unavailable, 123).unwrap().faults, 1);
    let exception = raw.replace("\"returned\":-1,\"threw\":false", "\"returned\":null,\"threw\":true");
    assert!(History::from_jsonl(&exception, 123).unwrap().submissions[0].threw);
    let legacy = History::from_jsonl(&(header() + &event(1, 1, Kind::Request, 1)), 123).unwrap();
    assert!(!legacy.submission_observed);
    assert!(legacy.pending_submissions.is_empty());
}

#[test]
fn android_speech_samples_preserve_unknown_state_and_original_request_identity() {
    let identity = Attachment {
        id: "12345678-1234-1234-1234-123456789abc".into(),
        start_ticks: 456,
    };
    let mut raw = header()
        .replacen(
            "\"get_env\":0",
            &format!(
                "\"get_env\":0,\"attachment\":\"{}\",\"start_ticks\":456",
                identity.id
            ),
            1,
        )
        .replace("\"flow_only\":true", "\"flow_only\":true,\"sampling\":true");
    raw += &event(1, 1, Kind::Request, 1)
        .replace("\"sequence\":1", "\"sequence\":1,\"synthesizer\":1");
    raw += &event(2, 1, Kind::Started, 1);
    let mut history = History::from_bound_jsonl(&raw, 123, &identity).unwrap();
    assert_eq!(history.tokens[0].synthesizer, Some(1));
    let request = "a".repeat(32);
    let sample = Sample {
        request: request.clone(),
        attachment: identity.id.clone(),
        pid: 123,
        start_ticks: 456,
        request_index: 1,
        sequence_before: 2,
        sequence_after: 2,
        began_ns: 5,
        ended_ns: 6,
        faults: 0,
        reference_limit_reached: false,
        synthesizers: vec![Synthesizer {
            id: 1,
            state: SampleState::Sampled,
            speaking: Some(true),
        }],
    };
    history
        .apply_sample(&serde_json::to_string(&sample).unwrap(), &request, 1)
        .unwrap();
    assert_eq!(
        history.sample.as_ref().unwrap().synthesizers[0].speaking,
        Some(true)
    );
    for changed in [
        Sample {
            pid: 124,
            ..sample.clone()
        },
        Sample {
            start_ticks: 457,
            ..sample.clone()
        },
        Sample {
            request: "b".repeat(32),
            ..sample.clone()
        },
        Sample {
            sequence_after: 3,
            ..sample.clone()
        },
        Sample {
            request_index: 129,
            ..sample.clone()
        },
    ] {
        assert!(history
            .apply_sample(&serde_json::to_string(&changed).unwrap(), &request, 1)
            .is_err());
    }
    let mut unavailable = sample;
    unavailable.synthesizers[0].state = SampleState::Collected;
    assert!(history
        .apply_sample(&serde_json::to_string(&unavailable).unwrap(), &request, 1)
        .is_err());
    unavailable.synthesizers[0].speaking = None;
    history
        .apply_sample(&serde_json::to_string(&unavailable).unwrap(), &request, 1)
        .unwrap();
    assert_eq!(
        history.sample.as_ref().unwrap().synthesizers[0].speaking,
        None
    );
    raw += &event(3, 1, Kind::Request, 2)
        .replace("\"sequence\":3", "\"sequence\":3,\"synthesizer\":1");
    assert_eq!(
        History::from_bound_jsonl(&raw, 123, &identity)
            .unwrap()
            .tokens[0]
            .synthesizer,
        None
    );
}

#[test]
fn android_speech_stop_exposes_receiver_without_inventing_an_utterance() {
    let raw = header() + &event(1, 0, Kind::StopRequest, 0)
        .replace("\"sequence\":1", "\"sequence\":1,\"synthesizer\":1");
    let history = History::from_jsonl(&raw, 123).unwrap();
    assert!(history.tokens.is_empty());
    assert_eq!(history.events[0].synthesizer, 1);
    assert_eq!(history.events[0].kind, Kind::StopRequest);
    assert_eq!(history.faults, 0);
    // Callback receivers are connection objects, not TTS synthesizers.
    let callback = header() + &event(1, 1, Kind::Failed, 0)
        .replace("\"sequence\":1", "\"sequence\":1,\"synthesizer\":1");
    assert!(History::from_jsonl(&callback, 123).is_err());
}

#[test]
fn android_speech_requires_the_original_attachment_and_process_birth() {
    let attachment = Attachment {
        id: "12345678-1234-1234-1234-123456789abc".into(),
        start_ticks: 456,
    };
    let legacy = header();
    assert!(History::from_bound_jsonl(&legacy, 123, &attachment).is_err());
    let raw = legacy.replacen(
        "\"get_env\":0",
        "\"get_env\":0,\"attachment\":\"12345678-1234-1234-1234-123456789abc\",\"start_ticks\":456",
        1,
    );
    assert!(History::from_bound_jsonl(&raw, 123, &attachment).is_ok());
    let wrong_birth = Attachment {
        start_ticks: 457,
        ..attachment.clone()
    };
    assert!(History::from_bound_jsonl(&raw, 123, &wrong_birth).is_err());
    let wrong_attachment = Attachment {
        id: "22345678-1234-1234-1234-123456789abc".into(),
        ..attachment
    };
    assert!(History::from_bound_jsonl(&raw, 123, &wrong_attachment).is_err());
}

fn header() -> String {
    let mut raw = "{\"event\":\"attach\",\"pid\":123,\"get_env\":0}\n".to_owned();
    for kind in ["speak", "stop", "onStart", "onSuccess", "onError", "onStop"] {
        raw += &serde_json::json!({"event":"bind", "kind":kind, "local_table_error":0,
            "identity_slot":if kind == "stop" { -1 } else { 2 }, "breakpoint_error":0,
            "rate_slot":-1,"format_slot":-1,"channels_slot":-1,"audio_slot":-1,
            "text_slot":-1,"params_slot":-1})
        .to_string();
        raw.push('\n');
    }
    raw + "{\"event\":\"ready\",\"pid\":123,\"boundaries\":6,\"flow_only\":true}\n"
}
fn event(sequence: u32, identity: u32, kind: Kind, requests: u32) -> String {
    let value = Event {
        sequence,
        identity,
        kind,
        request_count: requests,
        correlation: Correlation::for_requests(requests),
        local_error: 0,
        observer_exception: false,
        monotonic_ns: u64::from(sequence),
        limit_reached: sequence == 256,
        synthesizer: 0,
    };
    let mut json = serde_json::to_value(value).unwrap();
    json["event"] = "boundary".into();
    json.to_string() + "\n"
}

#[test]
fn android_speech_keeps_reused_tokens_ambiguous_and_rejects_false_correlation() {
    let mut raw = header();
    raw += &event(1, 1, Kind::Request, 1);
    raw += &event(2, 1, Kind::Started, 1);
    raw += &event(3, 1, Kind::Cancelled, 1);
    raw += &event(4, 1, Kind::Request, 2);
    raw += &event(5, 1, Kind::Finished, 2);
    let history = History::from_jsonl(&raw, 123).unwrap();
    let token = &history.tokens[0];
    assert_eq!(
        (
            token.requests,
            token.starts,
            token.cancellations,
            token.finishes
        ),
        (2, 1, 1, 1)
    );
    assert_eq!(token.correlation, Correlation::ReusedToken);
    assert!(
        History::from_jsonl(&raw.replace("reused-token", "single-observed-request"), 123).is_err()
    );
    assert!(History::from_jsonl(&raw, 124).is_err());
    assert!(History::from_jsonl(raw.trim_end(), 123).is_err());
    assert!(History::from_jsonl(
        &raw.replace("\"flow_only\":true", "\"flow_only\":false"),
        123
    )
    .is_err());
    assert!(History::from_jsonl(&raw.replace("onStart", "onAudioAvailable"), 123).is_err());
    assert!(History::from_jsonl(&raw.replace("\"sequence\":3", "\"sequence\":4"), 123).is_err());
}

#[test]
fn android_speech_retains_occurrence_after_tail_eviction_and_marks_capacity() {
    let mut raw = header();
    raw += &event(1, 1, Kind::Request, 1);
    raw += &event(2, 1, Kind::Started, 1);
    raw += &event(3, 1, Kind::Finished, 1);
    for n in 4..=256 {
        raw += &event(n, 0, Kind::StopRequest, 0);
    }
    let history = History::from_jsonl(&raw, 123).unwrap();
    assert!(history.truncated);
    assert_eq!(
        (history.received, history.dropped, history.events.len()),
        (256, 224, 32)
    );
    assert_eq!(history.tokens[0].finishes, 1);
    assert_eq!(history.tokens[0].last_kind, Kind::Finished);
    assert_eq!(history.unattributed, 253);
    assert!(history.events.iter().all(|e| e.identity == 0));
    raw += &event(257, 0, Kind::StopRequest, 0);
    assert!(History::from_jsonl(&raw, 123).is_err());
}

/// Installed producer evidence is an opt-in cross-language check, not a CI environment dependency.
#[test]
#[ignore = "requires FRONTCAP_ANDROID_SPEECH_FIXTURE and FRONTCAP_ANDROID_SPEECH_PID"]
fn android_speech_decodes_installed_observer() {
    let raw =
        std::fs::read_to_string(std::env::var("FRONTCAP_ANDROID_SPEECH_FIXTURE").unwrap()).unwrap();
    let pid = std::env::var("FRONTCAP_ANDROID_SPEECH_PID")
        .unwrap()
        .parse()
        .unwrap();
    let history = History::from_jsonl(&raw, pid).unwrap();
    assert_eq!(history.faults, 0);
    assert!(!history.truncated);
    assert!(history
        .tokens
        .iter()
        .any(|t| t.requests > 0 && t.starts > 0 && t.finishes > 0));
}

#[test]
#[ignore = "requires installed submission fixture, PID and FRONTCAP_ANDROID_SPEECH_ORACLE"]
fn android_speech_matches_installed_framework_returns() {
    let raw = std::fs::read_to_string(std::env::var("FRONTCAP_ANDROID_SPEECH_FIXTURE").unwrap()).unwrap();
    let pid = std::env::var("FRONTCAP_ANDROID_SPEECH_PID").unwrap().parse().unwrap();
    let oracle = std::fs::read_to_string(std::env::var("FRONTCAP_ANDROID_SPEECH_ORACLE").unwrap()).unwrap();
    #[derive(serde::Deserialize)]
    struct Return { operation: String, returned: i32 }
    let all: Vec<Return> = oracle.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(all.iter().any(|row| row.operation == "initialization"));
    assert!(all.iter().filter(|row| row.operation == "initialization").all(|row| row.returned == 0));
    let oracle: Vec<_> = all.into_iter().filter(|row| row.operation != "initialization").collect();
    assert_eq!(oracle.iter().map(|row| row.operation.as_str()).collect::<Vec<_>>(),
        ["ordinary", "oversized", "after-shutdown"]);
    assert_eq!(oracle.iter().map(|row| row.returned).collect::<Vec<_>>(), [0, -1, -1]);
    let history = History::from_jsonl(&raw, pid).unwrap();
    assert!(history.submission_observed);
    assert_eq!(history.faults, 0);
    assert!(!history.truncated);
    assert!(history.pending_submissions.is_empty());
    assert_eq!(history.submissions.iter().map(|row| row.returned).collect::<Vec<_>>(),
        oracle.iter().map(|row| Some(row.returned)).collect::<Vec<_>>());
    assert!(history.submissions.iter().all(|row| !row.threw && row.observer_error == 0));
    assert_eq!(history.tokens.len(), 1);
    assert_eq!(history.tokens[0].requests, 3);
    assert_eq!(history.tokens[0].correlation, Correlation::ReusedToken);
    println!("three original framework returns match; reused token remains ambiguous for callbacks");
}
