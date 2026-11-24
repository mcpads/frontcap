use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
#[tokio::test]
async fn command_budget_refuses_before_send_and_preserves_post_effect_uncertainty() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    use std::sync::{Arc,atomic::AtomicUsize};
    for before_send in [true,false] {
        let root=tempfile::tempdir().unwrap();
        let budget=WriteBudget::create(&root.path().join("budget"),LaunchId::new("g-page").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        if before_send {budget.charge(Class::Ordinary,65536).unwrap();}
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port=listener.local_addr().unwrap().port();
        let effects=Arc::new(AtomicUsize::new(0));let observed=effects.clone();let producer_budget=budget.clone();
        let server=tokio::spawn(async move {
            let (stream,_)=listener.accept().await.unwrap();
            let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();
            if let Some(Ok(Message::Text(text)))=socket.next().await {
                let request:serde_json::Value=serde_json::from_str(text.as_str()).unwrap();
                observed.fetch_add(1,Ordering::SeqCst);
                assert_eq!(request["method"],"Page.enable");
                producer_budget.charge(Class::Ordinary,65536-producer_budget.charged().unwrap().ordinary).unwrap();
                socket.send(Message::Text(serde_json::json!({"id":request["id"],"result":{}}).to_string().into())).await.unwrap();
            }
        });
        let client=cdp::Client::connect(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let evidence=root.path().join("command");let cancel=AtomicBool::new(false);
        let result=command_once_with_budget(&client,&EnablePage {},&evidence,
            Instant::now()+std::time::Duration::from_secs(3),&cancel,Some(&budget)).await;
        assert!(result.is_err());client.close();
        tokio::time::timeout(std::time::Duration::from_secs(3),server).await.unwrap().unwrap();
        assert_eq!(effects.load(Ordering::SeqCst),usize::from(!before_send));
        assert_eq!(evidence.join("intent.json").exists(),!before_send);
        assert!(!evidence.join("reply.json").exists());
        assert_eq!(budget.charged().unwrap(),Limits {ordinary:65536,retirement:0});
    }
}
#[tokio::test]
async fn reads_preserve_native_reply_and_report_transport_protocol_and_timeout_failures() {
    for mode in ["ok", "protocol", "lost", "timeout", "cancel"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, seen) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            let request: serde_json::Value =
                serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Runtime.evaluate");
            assert_eq!(request["params"]["expression"], READ);
            sent.send(()).unwrap();
            if mode == "lost" {
                return;
            }
            if mode == "timeout" || mode == "cancel" {
                std::future::pending::<()>().await;
            }
            let reply = if mode == "protocol" {
                serde_json::json!({"id":request["id"],"error":{"code":-32601,"message":"unsupported"}})
            } else {
                serde_json::json!({"id":request["id"],"result":{"result":{"type":"string","value":serde_json::json!({"href":"about:blank","ready_state":"complete","visibility":"visible","focused":true,"performance_ms":12.5,"date_ms":12345.0,"canvas_kit":{"document":"abcd0000-0000-0000-0000-000000000001","published":true},"media":{"document":"abcd0000-0000-0000-0000-000000000001","sourceCount":0,"sourceLimitReached":false,"flow":{"unattributed":0,"sources":[]},"available":true,"controlled":false,"mutedOutput":false,"hostMs":20.0,"received":0,"dropped":0,"omitted":0,"faults":0,"players":[],"events":[]},"page_events":{"document":"abcd0000-0000-0000-0000-000000000001","received":1,"dropped":0,"events":[{"sequence":1,"elapsedMs":12.5,"kind":"message","origin":"local","originTruncated":false,"dataType":"string","data":"content","truncated":false}]}}).to_string()}}})
            };
            socket
                .send(Message::Text(reply.to_string().into()))
                .await
                .unwrap();
            while let Some(message) = socket.next().await {
                if matches!(message, Ok(Message::Text(_))) {
                    panic!("unexpected redispatch");
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
        let cancel_after_send = async {
            seen.await.unwrap();
            if mode == "cancel" {
                cancelled.store(true, Ordering::Release);
            }
        };
        let (answer, ()) = tokio::join!(
            read_once(
                &client,
                evidence.path(),
                Instant::now() + std::time::Duration::from_millis(250),
                &cancelled
            ),
            cancel_after_send
        );
        if mode == "ok" {
            let snapshot = answer.unwrap();
            assert_eq!(snapshot.href, "about:blank");
            assert_eq!(snapshot.performance_ms, 12.5);
            let library = snapshot.canvas_kit.unwrap();
            assert!(library.published);
            assert_eq!(library.document, "abcd0000-0000-0000-0000-000000000001");
            let media = snapshot.media.unwrap();
            assert_eq!(media.document.as_deref(), Some("abcd0000-0000-0000-0000-000000000001"));
            let flow = media.flow.unwrap();
            assert_eq!(flow.unattributed, 0);
            assert!(flow.sources.is_empty());
            assert_eq!(snapshot.page_events.unwrap().history.received, 1);
            assert!(evidence.path().join("reply.json").exists());
        } else {
            assert!(answer.is_err(), "{mode}");
            assert!(!evidence.path().join("snapshot.json").exists());
        }
        client.close();
        server.abort();
        let _ = server.await;
    }
}
#[tokio::test]
async fn preparation_journals_install_before_navigation_and_never_repeats_an_attempt() {
    for mode in ["ok", "same-document", "reload-lost", "install-error", "navigation-lost"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let methods = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = methods.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                let method = request["method"].as_str().unwrap().to_owned();
                recorded.lock().unwrap().push(method.clone());
                if (method == "Page.navigate" && mode == "navigation-lost")
                    || (method == "Page.reload" && mode == "reload-lost") {
                    return;
                }
                let reply = if mode == "install-error" && method == "Page.addScriptToEvaluateOnNewDocument" {
                    serde_json::json!({"id":request["id"],"error":{"code":-32601,"message":"unsupported"}})
                } else if method == "Page.addScriptToEvaluateOnNewDocument" {
                    assert_eq!(request["params"]["source"], prepared_script(Some(42)));
                    serde_json::json!({"id":request["id"],"result":{"identifier":"platform-script"}})
                } else if matches!(method.as_str(), "Page.reload" | "Page.enable" | "Runtime.enable") {
                    serde_json::json!({"id":request["id"],"result":{}})
                } else {
                    assert_eq!(request["params"]["url"], "http://127.0.0.1:18763/");
                    let result = if mode == "same-document" || mode == "reload-lost" {
                        serde_json::json!({"frameId":"frame"})
                    } else {
                        serde_json::json!({"frameId":"frame", "loaderId":"new-document"})
                    };
                    serde_json::json!({"id":request["id"],"result":result})
                };
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let client = cdp::Client::connect(&format!("ws://127.0.0.1:{port}/devtools/page/one"))
            .await
            .unwrap();
        let evidence = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let result = prepare_once(
            &client,
            "http://127.0.0.1:18763/",
            Some(42),
            evidence.path(),
            Instant::now() + std::time::Duration::from_secs(2),
            &cancel,
        )
        .await;
        assert_eq!(result.is_ok(), mode == "ok" || mode == "same-document");
        if let Ok(prepared) = &result {
            assert_eq!(prepared.random_seed, Some(42));
            assert_eq!(prepared.attachment_sha256, frontcap_core::canonical::sha256_bytes_hex(prepared_script(Some(42)).as_bytes()));
            assert_ne!(prepared.attachment_sha256, frontcap_core::canonical::sha256_bytes_hex(prepared_script(Some(43)).as_bytes()));
        }
        assert_eq!(
            methods.lock().unwrap().as_slice(),
            if mode == "install-error" {
                vec!["Page.enable", "Runtime.enable", "Page.addScriptToEvaluateOnNewDocument"]
            } else if mode == "same-document" || mode == "reload-lost" {
                vec!["Page.enable", "Runtime.enable", "Page.addScriptToEvaluateOnNewDocument", "Page.navigate", "Page.reload"]
            } else {
                vec!["Page.enable", "Runtime.enable", "Page.addScriptToEvaluateOnNewDocument", "Page.navigate"]
            }
        );
        assert!(evidence.path().join("install/intent.json").exists());
        if mode == "navigation-lost" {
            assert!(evidence.path().join("navigate/intent.json").exists());
            assert!(!evidence.path().join("navigate/reply.json").exists());
        }
        let count = methods.lock().unwrap().len();
        assert!(
            prepare_once(
                &client,
                "http://127.0.0.1:18763/",
                Some(42),
                evidence.path(),
                Instant::now() + std::time::Duration::from_secs(1),
                &cancel
            )
            .await
            .is_err()
        );
        assert_eq!(methods.lock().unwrap().len(), count);
        client.close();
        server.abort();
        let _ = server.await;
    }
}
