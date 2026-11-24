//! Criterion 11: every connection goes through the recorder's proxy — loopback names included — and
//! none takes a direct path when the proxy is gone.
mod support;

use frontcap_chromium::generation::{Generation, ProxyRoute};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;
use frontcap_proxy::{Options, Proxy};
use std::sync::Arc;

async fn proxy(extra_roots: Vec<frontcap_proxy::CertificateDer<'static>>) -> (Proxy, ProxyRoute) {
    let p = Proxy::start(Options { bind: "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap().into(), authority: None, extra_roots }).await.unwrap();
    let route = ProxyRoute { addr: p.addr(), spki: p.spki_sha256_base64() };
    (p, route)
}

fn taints(g: &Generation) -> String {
    std::fs::read_to_string(g.trace_path()).unwrap().lines().filter(|l| l.contains("\"taint\"")).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn loopback_names_go_through_the_proxy() {
    let addr = support::serve_on("[::]:0");
    let port = addr.port();
    let (p, route) = proxy(vec![]).await;
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), "tr-1", format!("http://localhost:{port}/transport.html?port={port}"), RuleSet::default());
    o.passthrough_origins = Some(vec![format!("http://localhost:{port}"), format!("http://127.0.0.1:{port}"), format!("http://[::1]:{port}")]);
    o.proxy = Some(route);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.eval_main("probe(); 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    let results: serde_json::Value = serde_json::from_str(g.eval_main("JSON.stringify(results)").await.unwrap().as_str().unwrap()).unwrap();
    assert_eq!(results, serde_json::json!({"localhost": "ok", "127.0.0.1": "ok", "[::1]": "ok"}));
    let log = p.log();
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        assert!(log.iter().any(|l| l.contains(&format!("http://{host}:{port}/probe.txt"))), "{host} at the proxy: {log:?}");
    }
    assert!(log.iter().any(|l| l.contains("/transport.html")), "the page itself: {log:?}");
    assert!(!taints(&g).contains("transport_bypass"), "{}", taints(&g));
}

#[tokio::test]
async fn without_the_proxy_nothing_goes_direct() {
    // A server that counts what reaches it.
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = req.respond(tiny_http::Response::from_string("<!doctype html><p>here</p>").with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
        }
    });
    let (p, route) = proxy(vec![]).await;
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), "tr-2", format!("http://{addr}/first.html"), RuleSet::default());
    o.proxy = Some(route);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    assert!(before >= 1, "the first page came through the proxy");
    drop(p);
    let to = frontcap_core::action::Action::Navigate { url: format!("http://{addr}/second.html") };
    let _ = g.navigate_action(&frontcap_core::ids::RequestId::new("n-1").unwrap(), &to).await;
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.eval_main("fetch('/third.txt').catch(() => 0); 0").await.ok();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before, "no request reached the server by a direct path");
}

/// A TLS server for 127.0.0.1 under its own authority, answering every request with `body`.
async fn tls_server(body: &'static str) -> (std::net::SocketAddr, frontcap_proxy::CertificateDer<'static>) {
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
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else { return };
                let service = hyper::service::service_fn(move |_req| async move {
                    let mut r = hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from(body)));
                    r.headers_mut().insert("content-type", "text/html".parse().unwrap());
                    Ok::<_, std::convert::Infallible>(r)
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tls), service).await;
            });
        }
    });
    (addr, ca_cert.der().clone())
}

#[tokio::test]
async fn an_https_page_is_served_through_the_proxy_authority() {
    let (addr, root) = tls_server("<!doctype html><title>secure</title><p id=ok>secure ok</p>").await;
    let (p, route) = proxy(vec![root]).await;
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), "tr-3", format!("https://{addr}/"), RuleSet::default());
    o.proxy = Some(route);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('ok').textContent").await.unwrap(), "secure ok");
    assert!(p.log().iter().any(|l| l == &format!("connect {addr}")), "{:?}", p.log());
    assert!(!taints(&g).contains("transport_bypass"), "{}", taints(&g));
}

/// A frame's document the platform answers came over no connection: no transport taint (its response
/// reports an empty remote address).
#[tokio::test]
async fn a_frames_mocked_document_is_no_transport_bypass() {
    let addr = support::serve();
    let (_p, route) = proxy(vec![]).await;
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"frames","match":{"url":"https://w.test/*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p>frame</p>"}},
        {"id":"seq","match":{"url":"https://api.test/seq"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"ok"}}]"#).unwrap();
    let mut o = support::options(runs.path(), "tr-4", format!("http://{addr}/same_instant.html"), rules);
    o.proxy = Some(route);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(trace.contains("https://w.test/markup"), "{trace}");
    assert!(!taints(&g).contains("transport_bypass"), "{}", taints(&g));
}
