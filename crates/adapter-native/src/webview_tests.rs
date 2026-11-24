use super::*;
use crate::request::ClockOperation;
use std::{
    io::Write,
    net::{Ipv4Addr, TcpListener},
    thread,
    time::Duration,
};

fn request() -> ClockRequest {
    ClockRequest {
        generation: "87c86105-02ad-4d07-bc0c-58c922960225".into(),
        request_id: 4,
        operation: ClockOperation::Advance,
        delta_ns: 50_000_000,
    }
}
fn endpoint() -> Endpoint {
    Endpoint {
        generation: request().generation,
        pid: 12,
        port: 1234,
    }
}
fn state(ms: f64) -> State {
    State {
        timer_queue: Some(serde_json::from_value(serde_json::json!({"timers":[
            {"id":1000000000000_u64,"kind":"timeout","dueMs":ms+20.0}],"omitted":0,"failure":null})).unwrap()),
        frames: None,
        elapsed_ms: ms,
        performance_ms: ms,
        date_ms: 1700000000000.0 + ms,
        advancing: false,
        failure: None,
        pending_timers: 1,
        cancelled_on_failure: 0,
    }
}
fn completed() -> Reply {
    Reply {
        observation: Default::default(),
        generation: request().generation,
        pid: 12,
        port: 1234,
        request: request(),
        last_id: 4,
        now_ns: 100_000_000,
        replayed: false,
        status: Status::Ok,
        applied: Some(true),
        document: Some("23357f73-a68f-4271-a765-c8fb5538acc3".into()),
        state: Some(state(100.0)),
        progress: Some(Progress {
            before: state(50.0),
            requested_ms: 100.0,
            after: state(100.0),
        }),
        error: None,
    }
}

#[test]
fn page_effect_requires_bound_native_and_document_identity() {
    let mut reply = completed();
    let navigation = serde_json::json!({"document":reply.document,
        "current":{"href":"http://localhost/game", "hrefTruncated":false,"focused":true},
        "historyObserved":true,"failure":null,"received":0,"dropped":0,"events":[]});
    reply.observation.navigation = Some(serde_json::from_value(navigation).unwrap());
    reply
        .validate(&endpoint(), &request(), reply.document.as_deref())
        .unwrap();
    let mut replay = reply.clone();
    replay.replayed = true;
    replay
        .validate(&endpoint(), &request(), reply.document.as_deref())
        .unwrap();
    assert!(
        reply
            .validate(&endpoint(), &request(), Some("another-document"))
            .is_err()
    );
    let mut wrong = reply.clone();
    wrong.observation.navigation.as_mut().unwrap().document = "abcd0000-0000-0000-0000-000000000001".into();
    assert!(wrong.validate(&endpoint(), &request(), None).is_err());
    let mut wrong = reply.clone();
    wrong.pid += 1;
    assert!(wrong.validate(&endpoint(), &request(), None).is_err());
    let mut wrong = reply.clone();
    wrong.progress.as_mut().unwrap().before = state(40.0);
    assert!(wrong.validate(&endpoint(), &request(), None).is_err());
    let mut wrong = reply.clone();
    wrong.state.as_mut().unwrap().timer_queue.as_mut().unwrap().omitted = 1;
    assert!(wrong.validate(&endpoint(), &request(), None).is_err());
    let mut unavailable = reply.clone();
    let queue = unavailable.state.as_mut().unwrap().timer_queue.as_mut().unwrap();
    queue.timers.clear(); queue.omitted = 1; queue.failure = Some("Timer schedule unavailable".into());
    unavailable.progress.as_mut().unwrap().after = unavailable.state.clone().unwrap();
    unavailable.validate(&endpoint(), &request(), None).unwrap();
    let mut wrong = reply;
    wrong.state.as_mut().unwrap().failure = Some("timer fault".into());
    assert!(wrong.validate(&endpoint(), &request(), None).is_err());
}

#[test]
fn incomplete_outcomes_are_not_successful_clock_receipts() {
    let mut reply = completed();
    reply.status = Status::Unknown;
    reply.applied = None;
    reply.state = None;
    reply.progress = None;
    reply.document = None;
    reply.error = Some("webkit_completion_timeout".into());
    reply.validate(&endpoint(), &request(), None).unwrap();
    reply.applied = Some(false);
    assert!(reply.validate(&endpoint(), &request(), None).is_err());
    reply.status = Status::NotReady;
    reply.error = None;
    reply.validate(&endpoint(), &request(), None).unwrap();
    reply.status = Status::Rejected;
    reply.error = Some("sequence_gap".into());
    reply.last_id = 2;
    reply.validate(&endpoint(), &request(), None).unwrap();
}

#[test]
fn transport_preserves_reply_and_never_retries_an_incomplete_frame() {
    for truncated in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut endpoint = endpoint();
        endpoint.port = listener.local_addr().unwrap().port();
        let mut reply = completed();
        reply.port = endpoint.port;
        reply.observation.starting_randomness = Some(Box::new(serde_json::from_value(serde_json::json!({
            "document":reply.document,"seed":42,"math_calls":7,"crypto_byte_calls":2,
            "crypto_uuid_calls":1,"observation_errors":0,"uncontrolled_calls":1,
        })).unwrap()));
        reply.observation.canvas_kit = Some(Box::new(frontcap_core::native::android_webview::CanvasKitPublication {
            document: reply.document.clone().unwrap(), published: true,
        }));
        let publication = reply.observation.canvas_kit.clone();
        let randomness = reply.observation.starting_randomness.clone();
        let bytes = serde_json::to_vec(&reply).unwrap();
        let expected = bytes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut request_bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut request_bytes).unwrap();
            assert_eq!(
                serde_json::from_slice::<ClockRequest>(&request_bytes).unwrap(),
                request()
            );
            stream
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .unwrap();
            stream
                .write_all(if truncated { &bytes[..4] } else { &bytes })
                .unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            thread::sleep(Duration::from_millis(30));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        });
        let result = exchange(
            &endpoint,
            &request(),
            None,
            Instant::now() + Duration::from_secs(1),
        );
        if truncated {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        } else {
            let observed = result.unwrap();
            assert_eq!(observed.bytes, expected);
            assert_eq!(observed.reply.observation.starting_randomness, randomness);
            assert_eq!(observed.reply.observation.canvas_kit, publication);
        }
        server.join().unwrap();
    }
}

#[test]
fn clock_reply_preserves_observation_and_rejects_inconsistent_pointer_tail() {
    let mut raw = serde_json::to_value(completed()).unwrap();
    raw["visibility"] = serde_json::json!("hidden");
    raw["pointers"] = serde_json::json!({"received":1,"dropped":0,"active":1,"maximumActive":1,
        "events":[{"sequence":1,"kind":"pointerdown","id":1,"trusted":true,"x":10,"y":20,"pageMs":100,"active":1}]});
    let frames =
        serde_json::json!({"pending":2,"dispatching":false,"lastTimestamp":100,"delivered":3});
    raw["state"]["frames"] = frames.clone();
    raw["progress"]["after"]["frames"] = frames;
    let reply: Reply = serde_json::from_value(raw.clone()).unwrap();
    reply.validate(&endpoint(), &request(), None).unwrap();
    let encoded = serde_json::to_value(&reply).unwrap();
    assert_eq!(
        serde_json::from_value::<Reply>(encoded.clone()).unwrap(),
        reply
    );
    assert_eq!(encoded["state"]["frames"]["pending"], 2);
    assert_eq!(encoded["visibility"], "hidden");
    raw["pointers"]["dropped"] = serde_json::json!(1);
    let reply: Reply = serde_json::from_value(raw).unwrap();
    assert!(reply.validate(&endpoint(), &request(), None).is_err());
}

#[test]
fn media_flow_survives_ios_reply_and_rejects_foreign_document() {
    let mut reply = completed();
    reply.observation.media = Some(serde_json::from_value(serde_json::json!({
        "document":reply.document,"sourceCount":0,"sourceLimitReached":false,"flow":{"unattributed":0,"sources":[]},
        "available":true,"controlled":false,"mutedOutput":true,"hostMs":800.0,
        "received":0,"dropped":0,"omitted":0,"faults":0,"players":[],"events":[]
    })).unwrap());
    reply.validate(&endpoint(), &request(), reply.document.as_deref()).unwrap();
    let serialized = serde_json::to_vec(&reply).unwrap();
    let recovered: Reply = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(recovered.observation.media, reply.observation.media);
    reply.observation.media.as_mut().unwrap().document = Some("abcd0000-0000-0000-0000-000000000001".into());
    assert!(reply.validate(&endpoint(), &request(), reply.document.as_deref()).is_err());
}

#[test]
fn native_speech_reply_preserves_process_evidence_and_rejects_foreign_pid() {
    let mut reply = completed();
    reply.observation.native_speech = Some(serde_json::from_value(serde_json::json!({
        "process":"abcd0000-0000-0000-0000-000000000001","pid":reply.pid,
        "apiObserved":true,"delegateObserved":false,"received":0,"dropped":0,
        "truncated":false,"faults":0,"utteranceLimitReached":false,"events":[],"utterances":[],"synthesizers":[],"omittedSynthesizers":0
    })).unwrap());
    reply.validate(&endpoint(), &request(), reply.document.as_deref()).unwrap();
    let recovered: Reply = serde_json::from_slice(&serde_json::to_vec(&reply).unwrap()).unwrap();
    assert_eq!(recovered.observation.native_speech, reply.observation.native_speech);
    reply.observation.native_speech.as_mut().unwrap().pid += 1;
    assert!(reply.validate(&endpoint(), &request(), reply.document.as_deref()).is_err());
}
