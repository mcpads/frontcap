use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
#[tokio::test]
async fn completion_requires_both_acknowledgement_and_budget_event_without_redispatch() {
    for mode in [
        "pause",
        "event-first",
        "reply-first",
        "lost-event",
        "lost-reply",
        "unsupported",
        "cancel",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request: serde_json::Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "Emulation.setVirtualTimePolicy");
            assert_eq!(
                request["params"]["policy"],
                if mode == "pause" { "pause" } else { "advance" }
            );
            if mode != "pause" {
                assert_eq!(request["params"]["budget"], 100);
            }
            sent.send(()).unwrap();
            let reply = if mode == "unsupported" {
                serde_json::json!({"id":request["id"],"error":{"code":-32601,"message":"unsupported"}})
            } else {
                serde_json::json!({"id":request["id"],"result":{"virtualTimeTicksBase":1.25}})
            };
            let event =
                serde_json::json!({"method":"Emulation.virtualTimeBudgetExpired","params":{}});
            if mode == "event-first" || mode == "lost-reply" {
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            if mode != "lost-reply" && mode != "cancel" {
                ws.send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
            if mode == "reply-first" {
                ws.send(Message::Text(event.to_string().into()))
                    .await
                    .unwrap();
            }
            while let Some(message) = ws.next().await {
                if matches!(message, Ok(Message::Text(_))) {
                    panic!("timer effect was resent");
                }
                if message.is_err() {
                    break;
                }
            }
        });
        let client = cdp::Client::connect(&format!("ws://127.0.0.1:{port}/devtools/page/one"))
            .await
            .unwrap();
        let evidence = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let cancel = async {
            received.await.unwrap();
            if mode == "cancel" {
                cancelled.store(true, Ordering::Release);
            }
        };
        let action = if mode == "pause" {
            Action::Pause
        } else {
            Action::Advance { milliseconds: 100 }
        };
        let (result, ()) = tokio::join!(
            execute(
                &client,
                &action,
                evidence.path(),
                Instant::now() + std::time::Duration::from_millis(150),
                &cancelled
            ),
            cancel
        );
        assert_eq!(
            result.is_ok(),
            matches!(mode, "pause" | "event-first" | "reply-first"),
            "{mode}: {result:?}"
        );
        assert_eq!(
            evidence.path().join("budget-expired.json").exists(),
            matches!(mode, "event-first" | "reply-first")
        );
        if mode == "lost-event" {
            assert!(evidence.path().join("policy-reply.json").exists());
        }
        client.close();
        server.abort();
        let _ = server.await;
    }
}
