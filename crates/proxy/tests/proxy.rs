//! The proxy against loopback servers: forwarding, TLS termination, upstream verification, tunnels.
use frontcap_proxy::{CertificateDer, ORDINAL_HEADER, Options, Proxy};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An upstream that answers every request with the request's headers as its body, plain or over TLS.
async fn upstream(tls: Option<Arc<rustls::ServerConfig>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tls = tls.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                    let names: Vec<String> = req.headers().keys().map(|k| k.to_string()).collect();
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from(format!("{} {}", req.uri(), names.join(","))))))
                });
                let builder = hyper::server::conn::http1::Builder::new();
                match tls {
                    Some(c) => {
                        if let Ok(s) = tokio_rustls::TlsAcceptor::from(c).accept(stream).await {
                            let _ = builder.serve_connection(hyper_util::rt::TokioIo::new(s), service).await;
                        }
                    }
                    None => {
                        let _ = builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
                    }
                }
            });
        }
    });
    addr
}

/// A TLS server identity for 127.0.0.1 under its own authority: (server config, that authority).
fn test_server_identity() -> (Arc<rustls::ServerConfig>, CertificateDer<'static>) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap().signed_by(&key, &issuer).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()))
        .unwrap();
    (Arc::new(config), ca_cert.der().clone())
}

async fn proxy(extra_roots: Vec<CertificateDer<'static>>) -> Proxy {
    Proxy::start(Options { bind: "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap().into(), authority: None, extra_roots }).await.unwrap()
}

/// A listener the caller bound (inside a browser cell's network namespace) serves as the proxy's own.
#[tokio::test]
async fn a_proxy_serves_on_a_listener_the_caller_bound() {
    let up = upstream(None).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let p = Proxy::start(Options { bind: frontcap_proxy::Bind::Listener(listener), authority: None, extra_roots: vec![] }).await.unwrap();
    assert_eq!(p.addr(), at);
    let mut s = TcpStream::connect(at).await.unwrap();
    s.write_all(format!("GET http://{up}/b HTTP/1.1\r\nhost: {up}\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
    assert!(read_response(&mut s).await.starts_with("HTTP/1.1 200"));
}

async fn read_response(stream: &mut (impl AsyncReadExt + Unpin)) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf)).await.unwrap().unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&out).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n")
            && let Some(len) = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").and_then(|v| v.trim().parse::<usize>().ok()))
            && body.len() >= len
        {
            break;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test]
async fn a_request_is_forwarded_without_the_ordinal_and_hop_by_hop_headers() {
    let up = upstream(None).await;
    let p = proxy(vec![]).await;
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    let req = format!("GET http://{up}/a?x=1 HTTP/1.1\r\nhost: elsewhere.test\r\n{ORDINAL_HEADER}: req-3\r\nproxy-connection: keep-alive\r\nte: trailers\r\nconnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let resp = read_response(&mut s).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("/a?x=1 host") && !resp.contains(ORDINAL_HEADER), "the upstream never sees the header: {resp}");
    assert!(!resp.contains("proxy-connection") && !resp.contains("te,") && !resp.ends_with(",te"), "nor hop-by-hop headers: {resp}");
    assert_eq!(p.log(), vec![format!("http http://{up}/a?x=1 req-3")]);
}

/// A TLS client through `CONNECT`, trusting only the proxy's authority.
async fn tls_through(p: &Proxy, target: std::net::SocketAddr) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nhost: {target}\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(p.authority_der()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(target.ip().to_string()).unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, s).await.unwrap()
}

#[tokio::test]
async fn tls_is_terminated_and_the_upstream_verified() {
    let (identity, root) = test_server_identity();
    let up = upstream(Some(identity.clone())).await;
    let p = proxy(vec![root]).await;
    let mut tls = tls_through(&p, up).await;
    tls.write_all(format!("GET /secure HTTP/1.1\r\nhost: {up}\r\n{ORDINAL_HEADER}: req-1\r\n\r\n").as_bytes()).await.unwrap();
    let resp = read_response(&mut tls).await;
    assert!(resp.starts_with("HTTP/1.1 200") && resp.contains("/secure"), "{resp}");
    // Two requests in one tunnel: the connection is kept for the next.
    tls.write_all(format!("GET /again HTTP/1.1\r\nhost: {up}\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let again = read_response(&mut tls).await;
    assert!(again.starts_with("HTTP/1.1 200") && again.contains("/again"), "{again}");
    assert_eq!(p.log(), vec![format!("connect {up}"), format!("https https://{up}/secure req-1"), format!("https https://{up}/again")]);
}

#[tokio::test]
async fn an_upstream_that_cannot_be_verified_is_a_bad_gateway() {
    let (identity, _) = test_server_identity();
    let up = upstream(Some(identity)).await;
    let p = proxy(vec![]).await;
    let mut tls = tls_through(&p, up).await;
    tls.write_all(format!("GET /secure HTTP/1.1\r\nhost: {up}\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let resp = read_response(&mut tls).await;
    assert!(resp.starts_with("HTTP/1.1 502") && resp.contains("TLS"), "{resp}");
}

#[tokio::test]
async fn a_tunnel_without_tls_is_relayed() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = echo.accept().await.unwrap();
        let mut buf = [0u8; 64];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    let p = proxy(vec![]).await;
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    s.write_all(format!("CONNECT {at} HTTP/1.1\r\nhost: {at}\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    s.write_all(b"ping").await.unwrap();
    let mut got = [0u8; 4];
    s.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"ping");
}
