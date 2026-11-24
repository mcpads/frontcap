//! The platform's own connections to stream servers (network-rules.md, Timing): a passed-through
//! Server-Sent Events or WebSocket stream is opened by the platform, not the page; what the server sends
//! is buffered here, in arrival order, until the platform delivers it at a slot.
use crate::forward::connect;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What a stream's server did, in the order it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamEvent {
    /// The handshake succeeded.
    Open,
    /// A message: for Server-Sent Events with its event name and id.
    Message { data: String, event: Option<String>, id: Option<String> },
    /// The server ended the stream (a WebSocket's close code).
    Closed { code: u16 },
    /// The handshake or the connection failed.
    Failed(String),
}

/// Connections to stream servers, verified against the system's roots and `extra_roots`.
#[derive(Clone)]
pub struct Upstream {
    client: Arc<rustls::ClientConfig>,
}

/// Events buffered per stream before the server is pushed back (TCP flow control): a fast server while
/// virtual time is frozen fills this, then waits.
pub const BUFFERED_EVENTS: usize = 1024;

/// An open stream: what arrives, and (a WebSocket) where to send.
pub struct UpstreamStream {
    pub events: mpsc::Receiver<UpstreamEvent>,
    pub send: Option<mpsc::UnboundedSender<String>>,
    task: tokio::task::AbortHandle,
}

impl Drop for UpstreamStream {
    fn drop(&mut self) {
        // A WebSocket ends with its closing handshake (the task sends the close frame once nothing more can
        // be sent); an event stream's connection is dropped.
        if self.send.take().is_none() {
            self.task.abort();
        }
    }
}

impl Upstream {
    pub fn new(extra_roots: Vec<crate::CertificateDer<'static>>) -> Result<Upstream, crate::ProxyError> {
        Ok(Upstream { client: Arc::new(crate::tls::client_config(extra_roots)?) })
    }

    /// Opens a Server-Sent Events stream (`http(s)`), with `Last-Event-ID` when given.
    pub fn open_sse(&self, url: &str, last_event_id: Option<&str>) -> UpstreamStream {
        let (tx, rx) = mpsc::channel(BUFFERED_EVENTS);
        let (client, url, last) = (self.client.clone(), url.to_string(), last_event_id.map(str::to_string));
        let task = tokio::spawn(async move {
            if let Err(e) = sse(client, &url, last, &tx).await {
                let _ = tx.send(UpstreamEvent::Failed(e)).await;
            }
        });
        UpstreamStream { events: rx, send: None, task: task.abort_handle() }
    }

    /// Opens a WebSocket (`ws(s)`).
    pub fn open_ws(&self, url: &str) -> UpstreamStream {
        let (tx, rx) = mpsc::channel(BUFFERED_EVENTS);
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (client, url) = (self.client.clone(), url.to_string());
        let task = tokio::spawn(async move {
            if let Err(e) = ws(client, &url, &tx, out_rx).await {
                let _ = tx.send(UpstreamEvent::Failed(e)).await;
            }
        });
        UpstreamStream { events: rx, send: Some(out_tx), task: task.abort_handle() }
    }
}

fn split(url: &str) -> Result<(String, String, String), String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("{url} is not absolute"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = path.split('#').next().unwrap_or("/").to_string();
    Ok((scheme.to_string(), authority.to_string(), path))
}

async fn sse(client: Arc<rustls::ClientConfig>, url: &str, last: Option<String>, tx: &mpsc::Sender<UpstreamEvent>) -> Result<(), String> {
    let (scheme, authority, path) = split(url)?;
    let (io, _) = connect(&client, &scheme, &authority).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let mut req = hyper::Request::get(path).header("host", authority.clone()).header("accept", "text/event-stream").header("cache-control", "no-cache");
    if let Some(id) = last {
        req = req.header("last-event-id", id);
    }
    let req = req.body(http_body_util::Empty::<bytes::Bytes>::new()).map_err(|e| e.to_string())?;
    let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    let is_stream = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/event-stream"));
    if resp.status() != 200 || !is_stream {
        return Err(format!("{url} answered {} ({}), not an event stream", resp.status(), resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("no content type")));
    }
    let _ = tx.send(UpstreamEvent::Open).await;
    let mut body = resp.into_body();
    let mut parser = SseParser::default();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Some(d) = frame.data_ref() {
            for e in parser.feed(d) {
                let _ = tx.send(e).await;
            }
        }
    }
    let _ = tx.send(UpstreamEvent::Closed { code: 1000 }).await;
    Ok(())
}

async fn ws(client: Arc<rustls::ClientConfig>, url: &str, tx: &mpsc::Sender<UpstreamEvent>, mut out: mpsc::UnboundedReceiver<String>) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::Message;
    let (scheme, authority, _) = split(url)?;
    let (io, _) = connect(&client, &scheme, &authority).await?;
    let (socket, _) = tokio_tungstenite::client_async(url, io).await.map_err(|e| format!("{url}: {e}"))?;
    let _ = tx.send(UpstreamEvent::Open).await;
    let (mut write, mut read) = socket.split();
    loop {
        tokio::select! {
            m = read.next() => match m {
                Some(Ok(Message::Text(t))) => { let _ = tx.send(UpstreamEvent::Message { data: t.to_string(), event: None, id: None }).await; }
                // The page's wrapper carries text: a binary message is its bytes as (lossy) UTF-8.
                Some(Ok(Message::Binary(b))) => { let _ = tx.send(UpstreamEvent::Message { data: String::from_utf8_lossy(&b).into_owned(), event: None, id: None }).await; }
                Some(Ok(Message::Close(c))) => { let _ = tx.send(UpstreamEvent::Closed { code: c.map_or(1005, |c| u16::from(c.code)) }).await; return Ok(()); }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.to_string()),
                None => { let _ = tx.send(UpstreamEvent::Closed { code: 1006 }).await; return Ok(()); }
            },
            o = out.recv() => match o {
                Some(text) => write.send(Message::Text(text.into())).await.map_err(|e| e.to_string())?,
                None => {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), write.close()).await;
                    return Ok(());
                }
            },
        }
    }
}

/// The `text/event-stream` format (HTML, Server-sent events): UTF-8 with an optional leading byte order
/// mark, lines ended by CR LF, LF or CR, fields until a blank line, `data` lines joined by line feeds,
/// comments ignored. Lines are decoded whole, so a character split across reads stays intact. A CR ends
/// its line at once; an LF right after it, in this read or the next, is the same line ending.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    started: bool,
    /// The last line ended with CR: a leading LF is its second half.
    after_cr: bool,
    data: Vec<String>,
    event: Option<String>,
    id: Option<String>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<UpstreamEvent> {
        self.buffer.extend_from_slice(bytes);
        if !self.started {
            if self.buffer.len() < 3 && b"\xEF\xBB\xBF".starts_with(&self.buffer) {
                return Vec::new();
            }
            if self.buffer.starts_with(b"\xEF\xBB\xBF") {
                self.buffer.drain(..3);
            }
            self.started = true;
        }
        let mut out = Vec::new();
        loop {
            if self.after_cr && !self.buffer.is_empty() {
                self.after_cr = false;
                if self.buffer[0] == b'\n' {
                    self.buffer.drain(..1);
                }
            }
            let Some(i) = self.buffer.iter().position(|b| *b == b'\n' || *b == b'\r') else { break };
            let cr = self.buffer[i] == b'\r';
            let end = if cr && self.buffer.get(i + 1) == Some(&b'\n') { i + 2 } else { i + 1 };
            // A CR with nothing after it yet: an LF that follows belongs to it.
            self.after_cr = cr && end == i + 1;
            let raw: Vec<u8> = self.buffer.drain(..end).collect();
            let line = String::from_utf8_lossy(&raw[..i]).into_owned();
            let line = line.as_str();
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(UpstreamEvent::Message { data: self.data.join("\n"), event: self.event.take(), id: self.id.clone() });
                }
                self.data.clear();
                self.event = None;
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "data" => self.data.push(value.to_string()),
                "event" => self.event = Some(value.to_string()),
                "id" => self.id = Some(value.to_string()),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
