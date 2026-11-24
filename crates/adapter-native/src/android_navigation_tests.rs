use super::*;
#[tokio::test]
async fn document_navigation_preserves_loader_lineage_without_commands() {
    use futures_util::{SinkExt,StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address=listener.local_addr().unwrap();
    let (send,ready)=tokio::sync::oneshot::channel();
    let server=tokio::spawn(async move {
        let (stream,_)=listener.accept().await.unwrap();
        let mut socket=tokio_tungstenite::accept_async(stream).await.unwrap();ready.await.unwrap();
        for n in 0..CAPACITY+3 {
            let child=n==CAPACITY+1;
            let mut frame=serde_json::json!({"id":if child {"child"} else {"root"},"loaderId":format!("loader-{n}"),
                "url":if n==CAPACITY+2 {format!("http://localhost/{}","한".repeat(URL_LIMIT))} else {format!("http://localhost/{n}")},
                "domainAndRegistry":"","securityOrigin":"http://localhost","mimeType":"text/html",
                "secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]});
            if child {frame["parentId"]="root".into();}
            socket.send(Message::Text(serde_json::json!({"method":"Page.frameNavigated","params":{"frame":frame,"type":if child {"BackForwardCacheRestore"} else {"Navigation"}}}).to_string().into())).await.unwrap();
        }
        socket.close(None).await.unwrap();
        while let Some(message)=socket.next().await {
            if let Ok(message)=message {assert!(!message.is_text(),"observer dispatched a command");}
        }
    });
    let client=cdp::Client::connect(&format!("ws://{address}")).await.unwrap();
    let observer=Observer::new(&client);let before=observer.snapshot();send.send(()).unwrap();
    let after=tokio::time::timeout(std::time::Duration::from_secs(3),async {
        loop {let history=observer.snapshot();if history.closed {break history;}tokio::task::yield_now().await;}
    }).await.unwrap();
    assert!(after.valid());assert!(after.failure.is_none());assert_eq!(before.received,0);
    assert_eq!(after.received,(CAPACITY+3) as u64);assert_eq!(after.dropped,3);
    let last=&after.entries[CAPACITY-1];let prior=&after.entries[CAPACITY-3];
    assert_eq!(last.frame,prior.frame);assert_ne!(last.loader,prior.loader);assert!(last.url_truncated);
    let child=&after.entries[CAPACITY-2];assert_eq!(child.parent.as_deref(),Some("root"));assert!(child.restored_from_cache);
    let saved=serde_json::to_vec(&after).unwrap();assert_eq!(serde_json::from_slice::<History>(&saved).unwrap(),after);
    let mut invalid=after;invalid.entries[0].sequence=1;assert!(!invalid.valid());
    client.close();server.await.unwrap();
}
