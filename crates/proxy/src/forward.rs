//! Requests through the proxy: absolute-form HTTP, `CONNECT` tunnels (TLS terminated, or relayed as
//! bytes), and upgrades relayed after `101`.
use crate::{ORDINAL_HEADER, Shared};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

type Body = BoxBody<Bytes, std::io::Error>;

fn text(status: StatusCode, message: String) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(message)).map_err(|never| match never {}).boxed());
    *r.status_mut() = status;
    r
}

fn empty() -> Body {
    Full::new(Bytes::new()).map_err(|never| match never {}).boxed()
}

/// Serves one client connection.
pub async fn serve(shared: Arc<Shared>, stream: TcpStream) {
    let service = hyper::service::service_fn(move |req| handle(shared.clone(), req, None));
    let _ = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

type Answer = Pin<Box<dyn std::future::Future<Output = Result<Response<Body>, Infallible>> + Send>>;

/// `tunnel` is the `host:port` of the `CONNECT` whose TLS this request arrived in. Boxed as `Send`:
/// a tunnel serves requests with this same function.
fn handle(shared: Arc<Shared>, req: Request<Incoming>, tunnel: Option<String>) -> Answer {
    Box::pin(handle_inner(shared, req, tunnel))
}

async fn handle_inner(shared: Arc<Shared>, req: Request<Incoming>, tunnel: Option<String>) -> Result<Response<Body>, Infallible> {
    if req.method() == Method::CONNECT && tunnel.is_none() {
        let Some(authority) = req.uri().authority().map(|a| a.to_string()) else {
            return Ok(text(StatusCode::BAD_REQUEST, "CONNECT without an authority".into()));
        };
        shared.log(format!("connect {authority}"));
        let s = shared.clone();
        shared.spawn(async move {
            if let Ok(upgraded) = hyper::upgrade::on(req).await {
                connected(s, TokioIo::new(upgraded), authority).await;
            }
        });
        return Ok(Response::new(empty()));
    }
    let (scheme, authority) = match &tunnel {
        Some(a) => ("https", a.clone()),
        None => match (req.uri().scheme_str(), req.uri().authority()) {
            (Some(s), Some(a)) => (if s == "https" { "https" } else { "http" }, a.to_string()),
            _ => return Ok(text(StatusCode::BAD_REQUEST, "a proxy request names its absolute URL".into())),
        },
    };
    Ok(forward(shared, req, scheme, authority).await)
}

/// A `CONNECT` tunnel: TLS (the first byte is a handshake record) is terminated and its requests are
/// served; anything else is relayed byte for byte to the authority.
async fn connected(shared: Arc<Shared>, mut io: TokioIo<hyper::upgrade::Upgraded>, authority: String) {
    let mut first = [0u8; 1];
    let Ok(n) = io.read(&mut first).await else { return };
    let io = Prefixed { prefix: first[..n].to_vec(), at: 0, inner: io };
    if n == 1 && first[0] == 0x16 {
        let host = host_of(&authority);
        let Ok(config) = shared.authority.server(&host) else { return };
        let Ok(tls) = tokio_rustls::TlsAcceptor::from(config).accept(io).await else { return };
        let s = shared.clone();
        let service = hyper::service::service_fn(move |req| handle(s.clone(), req, Some(authority.clone())));
        let _ = hyper::server::conn::http1::Builder::new()
            .preserve_header_case(true)
            .serve_connection(TokioIo::new(tls), service)
            .with_upgrades()
            .await;
    } else if let Ok(upstream) = TcpStream::connect(&authority).await {
        let (mut a, mut b) = (io, upstream);
        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
    }
}

/// The host of `host:port`, without an IPv6 address's brackets.
pub(crate) fn host_of(authority: &str) -> String {
    let host = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };
    host.trim_start_matches('[').trim_end_matches(']').to_string()
}

pub(crate) fn port_of(authority: &str, scheme: &str) -> u16 {
    match authority.rsplit_once(':') {
        Some((_, p)) if !p.contains(']') => p.parse().unwrap_or(0),
        _ => if scheme == "https" { 443 } else { 80 },
    }
}

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

async fn connect_upstream(shared: &Shared, scheme: &str, authority: &str) -> Result<(Box<dyn Io>, SocketAddr), String> {
    connect(&shared.client, scheme, authority).await
}

/// A connection to `authority`, over TLS (verified by `client`) for `https`/`wss`.
pub(crate) async fn connect(client: &Arc<rustls::ClientConfig>, scheme: &str, authority: &str) -> Result<(Box<dyn Io>, SocketAddr), String> {
    let scheme = if scheme == "wss" { "https" } else if scheme == "ws" { "http" } else { scheme };
    let host = host_of(authority);
    let port = port_of(authority, scheme);
    let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(|e| format!("cannot connect to {authority}: {e}"))?;
    let remote = tcp.peer_addr().map_err(|e| e.to_string())?;
    if scheme != "https" {
        return Ok((Box::new(tcp), remote));
    }
    let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| format!("{host}: {e}"))?;
    let tls = tokio_rustls::TlsConnector::from(client.clone())
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS to {authority} failed: {e}"))?;
    Ok((Box::new(tls), remote))
}

/// Hop-by-hop headers (RFC 9110 §7.6.1), besides those `Connection` names.
const HOP_BY_HOP: &[&str] = &["keep-alive", "te", "trailer", "proxy-connection", "proxy-authorization"];

/// The platform's own response field naming when a response the page's main thread waits for arrives;
/// the engine applies it, so no server may set it.
const ARRIVAL_HEADER: &str = "x-frontcap-arrival-us";

/// A response's fields for the page: without what was meant for this connection, nor a challenge
/// addressed to this proxy (`Proxy-Authenticate`), nor the platform's own arrival field.
pub(crate) fn strip_response(headers: &mut hyper::HeaderMap) {
    strip_hop_by_hop(headers, false);
    headers.remove(hyper::header::PROXY_AUTHENTICATE);
    headers.remove(ARRIVAL_HEADER);
}

/// Removes what is meant for this connection only (RFC 9110 §7.6.1): the fixed hop-by-hop fields, every
/// field `Connection` names (in any of its lines, any case), and `Connection` itself. An upgrade keeps
/// `Upgrade` and `Connection: upgrade`, which the next hop needs.
pub(crate) fn strip_hop_by_hop(headers: &mut hyper::HeaderMap, upgrading: bool) {
    let named: Vec<String> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    for name in named.iter().map(String::as_str).chain(HOP_BY_HOP.iter().copied()) {
        if upgrading && name == "upgrade" {
            continue;
        }
        headers.remove(name);
    }
    headers.remove(hyper::header::CONNECTION);
    if upgrading {
        headers.insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("upgrade"));
    }
}

async fn forward(shared: Arc<Shared>, mut req: Request<Incoming>, scheme: &str, authority: String) -> Response<Body> {
    let path = req.uri().path_and_query().map_or("/".to_string(), |p| p.to_string());
    let url = format!("{scheme}://{authority}{path}");
    let ordinal = req.headers_mut().remove(ORDINAL_HEADER).and_then(|v| v.to_str().ok().map(str::to_string));
    shared.log(match &ordinal {
        Some(o) => format!("{scheme} {url} {o}"),
        None => format!("{scheme} {url}"),
    });
    let upgrading = req.headers().contains_key(hyper::header::UPGRADE);
    strip_hop_by_hop(req.headers_mut(), upgrading);
    // The absolute-form target names the host (RFC 9112 §3.2.2).
    if let Ok(h) = authority.parse() {
        req.headers_mut().insert(hyper::header::HOST, h);
    }
    *req.uri_mut() = path.parse().unwrap_or_default();
    let io = match connect_upstream(&shared, scheme, &authority).await {
        Ok((io, _)) => io,
        Err(e) => return text(StatusCode::BAD_GATEWAY, e),
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(io)).await {
        Ok(c) => c,
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    shared.spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let client_upgrade = upgrading.then(|| hyper::upgrade::on(&mut req));
    let mut resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        // A WebSocket: both sides upgraded, relayed byte for byte.
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        if let Some(client) = client_upgrade {
            shared.spawn(async move {
                if let (Ok(a), Ok(b)) = (client.await, upstream_upgrade.await) {
                    let (mut a, mut b) = (TokioIo::new(a), TokioIo::new(b));
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        }
        let (mut parts, _) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers, true);
        return Response::from_parts(parts, empty());
    }
    // The body goes on to the page as it arrives: a stream stays a stream.
    let (mut parts, body) = resp.into_parts();
    strip_response(&mut parts.headers);
    Response::from_parts(parts, body.map_err(|e| std::io::Error::other(e.to_string())).boxed())
}

/// A stream whose first bytes were already read.
struct Prefixed<T> {
    prefix: Vec<u8>,
    at: usize,
    inner: T,
}

impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = (self.prefix.len() - self.at).min(buf.remaining());
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "forward_tests.rs"]
mod tests;
