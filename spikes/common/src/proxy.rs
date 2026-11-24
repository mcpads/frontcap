use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct ProxyLogEntry {
    pub target: String,
    pub kind: &'static str,
}

#[derive(Default)]
struct State {
    log: Mutex<Vec<ProxyLogEntry>>,
    holds: Mutex<HashSet<String>>,
    released: Notify,
}

pub struct Proxy {
    addr: std::net::SocketAddr,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    pub async fn start(bind: &str) -> anyhow::Result<Proxy> {
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State::default());
        let s = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else { break };
                let s = s.clone();
                tokio::spawn(async move {
                    let _ = handle(client, s).await;
                });
            }
        });
        Ok(Proxy { addr, state, task })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn log(&self) -> Vec<ProxyLogEntry> {
        self.state.log.lock().unwrap().clone()
    }

    pub fn hold(&self, path_contains: &str) {
        self.state.holds.lock().unwrap().insert(path_contains.to_string());
    }

    pub fn release(&self, path_contains: &str) {
        self.state.holds.lock().unwrap().remove(path_contains);
        self.state.released.notify_waiters();
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

async fn handle(client: TcpStream, state: Arc<State>) -> anyhow::Result<()> {
    let mut reader = BufReader::new(client);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        headers.push(line);
    }
    if method == "CONNECT" {
        state.log.lock().unwrap().push(ProxyLogEntry { target: target.clone(), kind: "connect" });
        let mut upstream = TcpStream::connect(&target).await?;
        let mut client = reader.into_inner();
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }
    state.log.lock().unwrap().push(ProxyLogEntry { target: target.clone(), kind: "http" });
    let url = target.strip_prefix("http://").unwrap_or(&target);
    let (host, path) = url.split_once('/').map(|(h, p)| (h.to_string(), format!("/{p}"))).unwrap_or((url.to_string(), "/".into()));
    let host_port = if host.contains(':') && !host.ends_with(']') { host.clone() } else { format!("{host}:80") };
    let mut upstream = TcpStream::connect(&host_port).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    for h in &headers {
        if !h.to_ascii_lowercase().starts_with("proxy-connection") && !h.to_ascii_lowercase().starts_with("connection") {
            head.push_str(h);
        }
    }
    head.push_str("Connection: close\r\n\r\n");
    upstream.write_all(head.as_bytes()).await?;
    let mut client = reader.into_inner();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        loop {
            let held = state.holds.lock().unwrap().iter().any(|h| path.contains(h.as_str()));
            if !held {
                break;
            }
            state.released.notified().await;
        }
        let n = upstream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        client.write_all(&buf[..n]).await?;
    }
    Ok(())
}
