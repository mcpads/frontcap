//! Native dialog inventory distinguishes a removed session from a crashed or lost browser.
use frontcap_chromium::{dialog_control::DialogControl, session::Session};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy)]
enum Loss { Detached, MissingSession, Crashed, Transport }

async fn inspect_after_loss(loss: Loss, gone: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for method in ["Page.frontcapSetDialogControl", "Page.frontcapGetDialogs"] {
                let message = socket.next().await.unwrap().unwrap();
                let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(request["method"], method);
                let result = if method == "Page.frontcapGetDialogs" {
                    json!({"controlled":true,"dialogs":[],"receipts":[]})
                } else { json!({}) };
                socket.send(Message::Text(json!({"id":request["id"],"result":result}).to_string().into())).await.unwrap();
            }
            let message = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Page.frontcapGetDialogs");
            let event = match loss {
                Loss::Detached => Some(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"page-session"}})),
                Loss::MissingSession => Some(json!({"id":request["id"],"error":{"code":-32001,"message":"Session with given id not found."}})),
                Loss::Crashed => Some(json!({"method":"Inspector.targetCrashed","sessionId":"page-session","params":{}})),
                Loss::Transport => None,
            };
            if let Some(event) = event {
                socket.send(Message::Text(event.to_string().into())).await.unwrap();
                let _ = released.await;
            } else {
                socket.close(None).await.unwrap();
            }
        });
        let client = cdp::Client::connect(&endpoint).await.unwrap();
        let session = Session::new(client, "page-session".into());
        let control = DialogControl::attach(&session).await.unwrap();
        assert!(!control.is_gone());
        assert!(control.inspect().await.is_err(), "loss must not invent an empty successful inventory");
        assert_eq!(control.is_gone(), gone, "only a removed session is normal page retirement");
        assert!(control.subscribe().borrow().is_err(), "inventory loss remains visible to subscribers");
        let _ = release.send(());
        server.await.unwrap();
    }).await.expect("dialog lifecycle fixture timed out");
}

#[tokio::test]
async fn detached_dialog_session_is_gone() { inspect_after_loss(Loss::Detached, true).await; }

#[tokio::test]
async fn missing_dialog_session_is_gone() { inspect_after_loss(Loss::MissingSession, true).await; }

#[tokio::test]
async fn crashed_dialog_renderer_is_not_normal_retirement() { inspect_after_loss(Loss::Crashed, false).await; }

#[tokio::test]
async fn lost_dialog_transport_is_not_normal_retirement() { inspect_after_loss(Loss::Transport, false).await; }
