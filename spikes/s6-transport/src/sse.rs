use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub async fn start_ws() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("[::]:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let _ = tokio_tungstenite::accept_async(s).await;
            });
        }
    });
    Ok(port)
}

pub async fn start() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("[::]:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if req.contains("/ping") {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\n\r\nok").await;
                    return;
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-cache\r\n\r\n").await;
                for i in 0..20 {
                    if s.write_all(format!("data: {i}\n\n").as_bytes()).await.is_err() { return; }
                    // Upstream timing disturbance under test.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });
        }
    });
    Ok(port)
}
