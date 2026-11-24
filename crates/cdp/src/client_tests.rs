use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Fake browser: answers each request with the closure's reply; may emit events first.
async fn fake<F>(reply: F) -> String
where
    F: Fn(serde_json::Value) -> Vec<serde_json::Value> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let req: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
            for out in reply(req) {
                ws.send(Message::Text(out.to_string().into()))
                    .await
                    .unwrap();
            }
        }
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn call_returns_typed_result() {
    let url = fake(|req| {
        vec![serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 5.0 } })]
    })
    .await;
    let client = Client::connect(&url).await.unwrap();
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    let r = client.call(None, &params).await.unwrap();
    assert_eq!(r.virtual_time_ticks_base, 5.0);
}

/// A connection the caller opened (inside another network namespace, for a browser in a cell) carries
/// the same protocol.
#[tokio::test]
async fn a_client_runs_over_a_connection_the_caller_opened() {
    let url = fake(|req| {
        vec![serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 7.0 } })]
    })
    .await;
    let stream = tokio::net::TcpStream::connect(url.trim_start_matches("ws://")).await.unwrap();
    let client = Client::connect_over(stream, &url).await.unwrap();
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    assert_eq!(client.call(None, &params).await.unwrap().virtual_time_ticks_base, 7.0);
}

#[tokio::test]
async fn error_response_maps_to_protocol_error() {
    let url = fake(|req| {
        vec![serde_json::json!({ "id": req["id"], "error": { "code": -32601, "message": "nope" } })]
    })
    .await;
    let client = Client::connect(&url).await.unwrap();
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    match client.call(None, &params).await {
        Err(CdpError::Protocol { code, message, .. }) => {
            assert_eq!(code, -32601);
            assert_eq!(message, "nope");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn event_before_response_is_not_lost() {
    let url = fake(|req| vec![
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {}, "sessionId": "S1" }),
        serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 }, "sessionId": "S1" }),
    ]).await;
    let client = Client::connect(&url).await.unwrap();
    let mut sub =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(Some("S1"));
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Advance,
    );
    client.call(Some("S1"), &params).await.unwrap();
    sub.next_timeout(std::time::Duration::from_secs(2))
        .await
        .unwrap()
        .expect("event delivered");
}

/// Events delivered to different subscriptions keep their order in the connection's stream.
#[tokio::test]
async fn arrivals_order_events_across_subscriptions() {
    let url = fake(|req| vec![
        serde_json::json!({ "method": "Page.frameDetached", "params": { "frameId": "F1", "reason": "remove" } }),
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {} }),
        serde_json::json!({ "method": "Page.frameDetached", "params": { "frameId": "F2", "reason": "remove" } }),
        serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } }),
    ]).await;
    let client = Client::connect(&url).await.unwrap();
    let mut detached = client.subscribe::<crate::protocol::page::FrameDetachedEvent>(None);
    let mut expired = client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(crate::protocol::emulation::VirtualTimePolicy::Advance);
    client.call(None, &params).await.unwrap();
    let (a, first) = detached.try_next_stamped().unwrap().unwrap();
    let (c, second) = detached.try_next_stamped().unwrap().unwrap();
    let (b, _) = expired.try_next_stamped().unwrap().unwrap();
    assert_eq!((first.frame_id.0.as_str(), second.frame_id.0.as_str()), ("F1", "F2"));
    assert!(a < b && b < c, "{a:?} {b:?} {c:?}");
}

#[tokio::test]
async fn events_are_filtered_by_session() {
    let url = fake(|req| vec![
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {}, "sessionId": "OTHER" }),
        serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } }),
    ]).await;
    let client = Client::connect(&url).await.unwrap();
    let mut sub =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(Some("S1"));
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Advance,
    );
    client.call(None, &params).await.unwrap();
    let got = sub
        .next_timeout(std::time::Duration::from_millis(200))
        .await
        .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn routed_events_keep_their_session() {
    let url = fake(|req| vec![
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {}, "sessionId": "S1" }),
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {}, "sessionId": "S2" }),
        serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {} }),
        serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } }),
    ]).await;
    let client = Client::connect(&url).await.unwrap();
    let mut sub =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(Some("S1"));
    client.caller().route("S2", &sub.feed());
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Advance,
    );
    client.call(None, &params).await.unwrap();
    let first = sub.next_tagged().await.unwrap();
    let second = sub.next_tagged().await.unwrap();
    assert_eq!(first.session.as_deref(), Some("S1"));
    assert_eq!(second.session.as_deref(), Some("S2"));
    assert!(first.arrival < second.arrival);
    // The browser's own event is not this subscription's.
    assert!(sub.next_timeout(std::time::Duration::from_millis(200)).await.unwrap().is_none());
}

/// Fake browser that accepts one connection and closes it immediately.
async fn closing_fake() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = ws.close(None).await;
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn call_after_close_returns_closed() {
    let client = Client::connect(&closing_fake().await).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.call(None, &params),
    )
    .await;
    assert!(matches!(r, Ok(Err(CdpError::Closed))), "{r:?}");
}

#[tokio::test]
async fn subscription_ends_with_closed() {
    let client = Client::connect(&closing_fake().await).await.unwrap();
    let mut sub =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    let r = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next()).await;
    assert!(matches!(r, Ok(Err(CdpError::Closed))), "{r:?}");
}

#[tokio::test]
async fn unread_subscriptions_never_lag() {
    let url = fake(|req| {
        let mut out: Vec<serde_json::Value> = (0..10_000)
            .map(|_| serde_json::json!({ "method": "Emulation.virtualTimeBudgetExpired", "params": {} }))
            .collect();
        out.push(serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } }));
        out
    })
    .await;
    let client = Client::connect(&url).await.unwrap();
    let mut expired =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    // A subscription for another event that is never read must not affect the first one.
    let _other = client.subscribe::<crate::protocol::page::FrameNavigatedEvent>(None);
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    client.call(None, &params).await.unwrap();
    let mut n = 0;
    while expired.try_next().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 10_000);
}

#[tokio::test]
async fn try_next_returns_at_once_when_empty() {
    let url = fake(|req| {
        vec![serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } })]
    })
    .await;
    let client = Client::connect(&url).await.unwrap();
    let mut sub =
        client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        assert!(sub.try_next().unwrap().is_none());
    }
    assert!(
        start.elapsed() < std::time::Duration::from_millis(50),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn close_ends_the_connection_for_the_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(_)) = ws.next().await {}
        let _ = tx.send(());
    });
    let client = Client::connect(&format!("ws://{addr}")).await.unwrap();
    let _sub = client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    let keep = client.clone();
    client.close();
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("the peer saw the connection end")
        .unwrap();
    drop(keep);
}

#[tokio::test]
async fn dropping_the_last_client_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        // The stream ends (None or a close frame) once the client side is gone.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(Ok(m)) = socket.next().await {
                if m.is_close() {
                    break;
                }
            }
        })
        .await
    });
    let client = Client::connect(&format!("ws://{addr}")).await.unwrap();
    let clone = client.clone();
    let sub = client.subscribe::<crate::protocol::emulation::VirtualTimeBudgetExpiredEvent>(None);
    drop(client);
    drop(clone);
    assert!(
        peer.await.unwrap().is_ok(),
        "the peer saw the connection end"
    );
    drop(sub);
}

#[tokio::test]
async fn a_clone_keeps_the_connection_open() {
    let url = fake(|req| {
        vec![serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 } })]
    })
    .await;
    let client = Client::connect(&url).await.unwrap();
    let clone = client.clone();
    drop(client);
    let params = crate::protocol::emulation::SetVirtualTimePolicyParams::new(
        crate::protocol::emulation::VirtualTimePolicy::Pause,
    );
    clone.call(None, &params).await.expect("still connected");
}

/// A renderer that crashed answers none of its session's calls: the crash fails the waiting ones, and
/// later calls on that session fail at once, while other sessions' calls are answered as before.
#[tokio::test]
async fn a_crashed_targets_calls_fail_instead_of_waiting() {
    let url = fake(|req| match req["sessionId"].as_str() {
        Some("S1") if req["method"] == "Runtime.evaluate" => vec![serde_json::json!({ "method": "Inspector.targetCrashed", "params": {}, "sessionId": "S1" })],
        _ => vec![serde_json::json!({ "id": req["id"], "result": { "virtualTimeTicksBase": 1.0 }, "sessionId": req["sessionId"] })],
    }).await;
    let client = Client::connect(&url).await.unwrap();
    let evaluate = crate::protocol::runtime::EvaluateParams::new("0".into());
    let waited = tokio::time::timeout(std::time::Duration::from_secs(2), client.call(Some("S1"), &evaluate)).await.expect("not left waiting");
    assert!(matches!(waited, Err(CdpError::TargetCrashed)), "{waited:?}");
    let pause = crate::protocol::emulation::SetVirtualTimePolicyParams::new(crate::protocol::emulation::VirtualTimePolicy::Pause);
    assert!(matches!(client.call(Some("S1"), &pause).await, Err(CdpError::TargetCrashed)));
    assert!(client.send(Some("S1"), &pause).is_err());
    client.call(Some("S2"), &pause).await.expect("another session is answered");
}

/// A target can detach without answering a command already sent to it. The
/// detached child is named in params, not the event's parent-session envelope.
#[tokio::test]
async fn a_detached_targets_pending_and_later_calls_fail_without_ending_its_parent() {
    let url = fake(|req| match req["sessionId"].as_str() {
        Some("child") if req["method"] == "Runtime.evaluate" => vec![serde_json::json!({
            "method": "Target.detachedFromTarget", "sessionId": "parent",
            "params": {"sessionId": "child", "targetId": "popup"}
        })],
        _ => vec![serde_json::json!({"id": req["id"], "result": {"virtualTimeTicksBase": 1.0}, "sessionId": req["sessionId"]})],
    }).await;
    let client = Client::connect(&url).await.unwrap();
    let mut detached = client.subscribe::<crate::protocol::target::DetachedFromTargetEvent>(Some("parent"));
    let evaluate = crate::protocol::runtime::EvaluateParams::new("0".into());
    let waited = tokio::time::timeout(Duration::from_secs(2), client.call(Some("child"), &evaluate))
        .await.expect("a detached session must not leave its command waiting");
    assert!(matches!(waited, Err(CdpError::SessionDetached)), "{waited:?}");
    assert_eq!(detached.next().await.unwrap().session_id.0, "child");
    let pause = crate::protocol::emulation::SetVirtualTimePolicyParams::new(crate::protocol::emulation::VirtualTimePolicy::Pause);
    assert!(matches!(client.call(Some("child"), &pause).await, Err(CdpError::SessionDetached)));
    assert!(client.send(Some("child"), &pause).is_err());
    client.call(Some("parent"), &pause).await.expect("the parent session is still usable");
    client.call(Some("sibling"), &pause).await.expect("a sibling session is still usable");
    client.call(None, &pause).await.expect("the browser connection is still usable");
}
