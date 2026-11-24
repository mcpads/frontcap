use cdp::protocol::{emulation, fetch, input, target};

#[test]
fn generated_protocol_compiles() {
    // Compiling this file at all proves the full protocol generated and type-checked.
    let _ = emulation::VirtualTimePolicy::Pause;
}

#[test]
fn serializes_set_virtual_time_policy() {
    let params = emulation::SetVirtualTimePolicyParams {
        budget: Some(1000.0),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    };
    let v = serde_json::to_value(&params).unwrap();
    assert_eq!(v, serde_json::json!({ "policy": "advance", "budget": 1000.0 }));
    assert_eq!(<emulation::SetVirtualTimePolicyParams as cdp::Command>::METHOD, "Emulation.setVirtualTimePolicy");
}

#[test]
fn deserializes_request_paused_event() {
    // Shape taken from a real Fetch.requestPaused message (headers trimmed).
    let raw = serde_json::json!({
        "requestId": "interception-job-1.0",
        "request": { "url": "http://127.0.0.1:8080/api", "method": "GET", "headers": {},
                     "initialPriority": "High", "referrerPolicy": "strict-origin-when-cross-origin" },
        "frameId": "F1", "resourceType": "Fetch"
    });
    let e: fetch::RequestPausedEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(e.request.url, "http://127.0.0.1:8080/api");
    assert_eq!(<fetch::RequestPausedEvent as cdp::Event>::NAME, "Fetch.requestPaused");
}

#[test]
fn dispatch_mouse_event_uses_raw_type_field() {
    let p = input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MousePressed, 10.0, 20.0);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["type"], "mousePressed");
    assert_eq!(v["x"], 10.0);
}

#[test]
fn create_target_constructor_takes_url() {
    let p = target::CreateTargetParams {
        enable_begin_frame_control: Some(true),
        ..target::CreateTargetParams::new("about:blank".into())
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v, serde_json::json!({ "url": "about:blank", "enableBeginFrameControl": true }));
}
