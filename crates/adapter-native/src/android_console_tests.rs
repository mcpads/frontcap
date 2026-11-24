use super::*;
#[tokio::test]
async fn console_stream_preserves_context_without_app_evaluation() {
    use futures_util::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (send, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        ready.await.unwrap();
        socket.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::json!({
            "method":"Runtime.consoleAPICalled", "params": {
                "type":"log", "args":[{"type":"string","value":"entity accepted"},
                    {"type":"object","description":"Object"}],
                "executionContextId":17,"timestamp":123.5
            }
        }).to_string().into())).await.unwrap();
        socket.close(None).await.unwrap();
        // The observer must never send Runtime.evaluate or expand a logged object.
        while let Some(message) = socket.next().await {
            if let Ok(message) = message { assert!(!message.is_text()); }
        }
    });
    let client = cdp::Client::connect(&format!("ws://{address}")).await.unwrap();
    let observer = Observer::new(&client);
    send.send(()).unwrap();
    let history = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let history = observer.snapshot();
            if history.closed { break history; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(history.valid());
    assert_eq!(history.received, 1);
    assert_eq!(history.entries[0].context, 17);
    assert_eq!(history.entries[0].text, "entity accepted Object");
    assert_eq!(history.entries[0].timestamp_ms, 123.5);
    client.close();
    server.await.unwrap();
}
#[test]
fn console_retention_bounds_text_and_reports_discarded_history() {
    let mut history = History { connection: frontcap_core::ids::RequestId::generate().as_str().to_owned(), received: 0,
        dropped: 0, closed: false, failure: None, entries: Vec::new() };
    for i in 0..CAPACITY + 3 {
        retain(&mut history, Entry { sequence: 0, context: 7, timestamp_ms: i as f64,
            level: "log".into(), text: "한".repeat(TEXT_LIMIT), truncated: false });
    }
    assert!(history.valid());
    assert_eq!(history.dropped, 3);
    assert_eq!(history.entries[0].sequence, 4);
    assert!(history.entries.iter().all(|e| e.truncated && e.text.len() <= TEXT_LIMIT));
    let saved = serde_json::to_string(&history).unwrap();
    assert_eq!(serde_json::from_str::<History>(&saved).unwrap(), history);
    history.entries[0].sequence = 1;
    assert!(!history.valid());
}
