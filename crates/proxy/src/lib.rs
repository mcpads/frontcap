//! The recorder's proxy (network-rules.md, Interception and transport): every connection the page opens
//! goes through it. HTTP requests are forwarded, `CONNECT` tunnels carrying TLS are terminated with a
//! certificate authority made for this proxy, and WebSocket upgrades are relayed. A request the
//! platform passed through carries `x-frontcap-request: <ordinal>`, which the proxy removes and logs.
//! Bodies stream through; what the page received is recorded from the engine, not here.
mod forward;
mod tls;
pub mod upstream;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub use rustls::pki_types::CertificateDer;

/// Certificates from PEM text (extra upstream roots).
pub fn certificates_from_pem(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem).filter_map(Result::ok).collect()
}

/// The header the platform adds to a request it passes through, naming its trace ordinal.
pub const ORDINAL_HEADER: &str = "x-frontcap-request";

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("certificate authority: {0}")]
    Certificate(String),
    #[error("tls: {0}")]
    Tls(String),
}

/// The keys of a proxy's authority, kept so a relaunched recorder's proxy presents what the browser
/// already trusts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityKeys {
    pub ca_key_pem: String,
    pub leaf_key_pem: String,
}

/// Where the proxy listens: an address it binds, or a listener the caller bound (one bound inside a
/// browser cell's network namespace keeps it).
pub enum Bind {
    Addr(SocketAddr),
    Listener(std::net::TcpListener),
}

impl From<SocketAddr> for Bind {
    fn from(addr: SocketAddr) -> Bind {
        Bind::Addr(addr)
    }
}

pub struct Options {
    pub bind: Bind,
    /// The authority of an earlier proxy of the same browser; a new one when `None`.
    pub authority: Option<AuthorityKeys>,
    /// Roots trusted for upstream TLS besides the system's (a test server's authority).
    pub extra_roots: Vec<CertificateDer<'static>>,
}

pub(crate) struct Shared {
    pub authority: tls::Authority,
    pub client: Arc<rustls::ClientConfig>,
    pub log: Mutex<std::collections::VecDeque<String>>,
    /// Every task serving a connection, ended with the proxy: a browser's kept-alive connection must not
    /// outlive it.
    pub tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Shared {
    pub fn log(&self, line: String) {
        let mut log = self.log.lock().unwrap();
        if log.len() >= LOG_KEPT {
            log.pop_front();
        }
        log.push_back(line);
    }

    pub fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, f: F) {
        let handle = tokio::spawn(f).abort_handle();
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }
}

/// Lines of the connection log kept (the latest).
const LOG_KEPT: usize = 4096;

pub struct Proxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    pub async fn start(o: Options) -> Result<Proxy, ProxyError> {
        let authority = tls::Authority::new(o.authority.as_ref())?;
        let client = Arc::new(tls::client_config(o.extra_roots)?);
        let shared = Arc::new(Shared { authority, client, log: Mutex::new(Default::default()), tasks: Mutex::new(Vec::new()) });
        let listener = match o.bind {
            Bind::Addr(addr) => tokio::net::TcpListener::bind(addr).await?,
            Bind::Listener(listener) => {
                listener.set_nonblocking(true)?;
                tokio::net::TcpListener::from_std(listener)?
            }
        };
        let addr = listener.local_addr()?;
        let s = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => s.spawn(forward::serve(s.clone(), stream)),
                    // A transient failure (out of descriptors, an aborted connection) must not end the
                    // proxy: the browser would have nowhere to go.
                    Err(e) => {
                        s.log(format!("accept failed: {e}"));
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        });
        Ok(Proxy { addr, shared, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base64 SHA-256 of the key every certificate this proxy presents uses, for
    /// `--ignore-certificate-errors-spki-list`.
    pub fn spki_sha256_base64(&self) -> String {
        self.shared.authority.spki_sha256_base64()
    }

    pub fn authority_keys(&self) -> AuthorityKeys {
        self.shared.authority.keys()
    }

    /// The authority's certificate, for a client that trusts it explicitly (tests; the browser trusts
    /// the leaf key's SPKI hash).
    pub fn authority_der(&self) -> CertificateDer<'static> {
        self.shared.authority.ca_der()
    }

    /// Every connection and request the proxy saw, `http <url>`, `https <url>`, or `connect <host:port>`.
    pub fn log(&self) -> Vec<String> {
        self.shared.log.lock().unwrap().iter().cloned().collect()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        for t in self.shared.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}
