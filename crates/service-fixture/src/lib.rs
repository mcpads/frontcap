//! An explicitly released local HTTP fixture. Transport completion never proves application delivery.
pub mod assets;
pub mod owned;
use bytes::Bytes;
use frontcap_core::{
    ids::{LaunchId, RequestId},
    state::save_atomic,
    write_budget::{Class, WriteBudget},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{
    collections::BTreeMap, convert::Infallible, io, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, Semaphore, oneshot},
    task::JoinSet,
};

const MAX_BODY: usize = 65536;
// Even worst-case JSON escaping must fit the recorder's bounded response frame.
const MAX_RETAINED_BODY: usize = 1 << 20;
const MAX_REQUESTS: usize = 128;
const CONNECTION_BUDGET: Duration = Duration::from_secs(120);

pub use frontcap_core::native::service::*;

#[derive(Clone)]
struct ReleasedBody {
    status: u16,
    content_type: String,
    content_security_policy: Option<String>,
    body: Bytes,
}

struct Shared {
    record: Record,
    path: PathBuf,
    pending: BTreeMap<RequestId, oneshot::Sender<ReleasedBody>>,
    poisoned: bool,
    assets: BTreeMap<String, assets::FrozenAsset>,
    budget: Option<WriteBudget>,
}
impl Shared {
    fn charge(&self, class: Class, bytes: u64) -> io::Result<()> {
        if let Some(budget) = &self.budget {
            budget.charge(class, bytes)?;
        }
        Ok(())
    }
    fn commit(&mut self, record: Record) -> io::Result<()> {
        self.charge(Class::Ordinary, record_bytes(&record))?;
        self.persist(record)
    }
    // All payload bytes must already be charged before entering this publication path.
    fn persist(&mut self, record: Record) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("fixture persistence is uncertain"));
        }
        if let Err(error) = save_atomic(&self.path, &record) {
            self.poisoned = true;
            return Err(error);
        }
        self.record = record;
        Ok(())
    }
    fn receive(&mut self, id: RequestId) -> io::Result<oneshot::Receiver<ReleasedBody>> {
        self.receive_resource(id, None)
    }
    fn receive_resource(
        &mut self,
        id: RequestId,
        resource: Option<ResourceRequest>,
    ) -> io::Result<oneshot::Receiver<ReleasedBody>> {
        if self.record.stopping || self.poisoned {
            return Err(io::Error::other("fixture is not accepting requests"));
        }
        if self.record.requests.contains_key(&id) {
            return Err(io::Error::other("request identity already received"));
        }
        if self.record.requests.len() >= MAX_REQUESTS {
            return Err(io::Error::other("request budget exhausted"));
        }
        let mut next = self.record.clone();
        next.requests.insert(
            id.clone(),
            Received {
                resource,
                release: None,
                transport: Transport::Open,
            },
        );
        self.commit(next)?;
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        Ok(rx)
    }
    fn control(&mut self, command: Control) -> io::Result<Reply> {
        let generation = match &command {
            Control::Inspect { generation }
            | Control::Release { generation, .. }
            | Control::ReleaseAsset { generation, .. }
            | Control::Stop { generation } => generation,
        };
        if generation != &self.record.generation {
            return Err(io::Error::other("fixture generation differs"));
        }
        if self.poisoned {
            return Err(io::Error::other(
                "fixture persistence is uncertain; inspect retained state offline",
            ));
        }
        match &command {
            Control::Inspect { .. } => Ok(Reply::Snapshot {
                record: self.record.clone(),
            }),
            Control::Stop { .. } => {
                if self.record.stopping {
                    return Ok(Reply::Stopping);
                }
                let mut next = self.record.clone();
                next.stopping = true;
                for row in next
                    .requests
                    .values_mut()
                    .filter(|r| r.transport == Transport::Open)
                {
                    row.transport = Transport::Stopped;
                }
                self.charge(Class::Retirement, record_bytes(&next))?;
                self.persist(next)?;
                self.pending.clear();
                Ok(Reply::Stopping)
            }
            Control::Release { id, request, .. } | Control::ReleaseAsset { id, request, .. } => {
                if let Some(previous) = self.record.releases.get(id) {
                    if previous.command != command {
                        return Err(io::Error::other("release identity conflict"));
                    }
                    return Ok(Reply::Released {
                        receipt: previous.clone(),
                    });
                }
                if self.record.stopping {
                    return Err(io::Error::other("fixture is stopping"));
                }
                let body = match &command {
                    Control::Release { response, .. } => {
                        if self
                            .record
                            .requests
                            .get(request)
                            .is_some_and(|row| row.resource.is_some())
                        {
                            return Err(io::Error::other(
                                "resource requests require asset release",
                            ));
                        }
                        if !(200..=599).contains(&response.status)
                            || response.body.len() > MAX_BODY
                            || ([204, 205, 304].contains(&response.status)
                                && !response.body.is_empty())
                        {
                            return Err(io::Error::other("response exceeds status/body bounds"));
                        }
                        let retained: usize = self
                            .record
                            .releases
                            .values()
                            .map(|receipt| match &receipt.command {
                                Control::Release { response, .. } => response.body.len(),
                                _ => 0,
                            })
                            .sum();
                        if retained + response.body.len() > MAX_RETAINED_BODY {
                            return Err(io::Error::other(
                                "generation response body budget exhausted",
                            ));
                        }
                        ReleasedBody {
                            status: response.status,
                            content_type: "application/json".into(),
                            content_security_policy: None,
                            body: Bytes::from(response.body.clone()),
                        }
                    }
                    Control::ReleaseAsset { sha256, .. } => {
                        let resource = self
                            .record
                            .requests
                            .get(request)
                            .and_then(|row| row.resource.as_ref())
                            .ok_or_else(|| io::Error::other("request is not a resource"))?;
                        if sha256 != &resource.asset.sha256 {
                            return Err(io::Error::other("asset identity differs"));
                        }
                        let path = resource.uri.split('?').next().unwrap();
                        let asset = self
                            .assets
                            .get(path)
                            .ok_or_else(|| io::Error::other("frozen asset missing"))?;
                        ReleasedBody {
                            status: 200,
                            content_type: asset.identity.content_type.clone(),
                            content_security_policy: asset.identity.content_security_policy.clone(),
                            body: asset.body.clone(),
                        }
                    }
                    _ => unreachable!(),
                };
                let received = self
                    .record
                    .requests
                    .get(request)
                    .ok_or_else(|| io::Error::other("request is not waiting"))?;
                let terminal = matches!(
                    received.transport,
                    Transport::Closed | Transport::Failed | Transport::TimedOut
                );
                if received.release.is_some()
                    || !(terminal
                        || (received.transport == Transport::Open
                            && self.pending.contains_key(request)))
                {
                    return Err(io::Error::other("request is not waiting"));
                }
                let mut next = self.record.clone();
                let receipt = ReleaseReceipt {
                    command: command.clone(),
                    state: ReleaseState::Prepared,
                };
                next.releases.insert(id.clone(), receipt);
                next.requests.get_mut(request).unwrap().release = Some(id.clone());
                // Reserve both publications before handoff; exhaustion cannot occur between them.
                let prepared_bytes = record_bytes(&next);
                let mut completed = next.clone();
                completed.releases.get_mut(id).unwrap().state = ReleaseState::QueuedForHttp;
                let queued_bytes = record_bytes(&completed);
                completed.releases.get_mut(id).unwrap().state = ReleaseState::ReceiverGone;
                self.charge(
                    Class::Ordinary,
                    prepared_bytes + queued_bytes.max(record_bytes(&completed)),
                )?;
                self.persist(next)?;
                // A terminal admitted request still accepts one explicit release attempt.
                // Its missing receiver is evidence of non-delivery, not a new transport.
                let state = if self
                    .pending
                    .remove(request)
                    .is_some_and(|tx| tx.send(body).is_ok())
                {
                    ReleaseState::QueuedForHttp
                } else {
                    ReleaseState::ReceiverGone
                };
                let mut next = self.record.clone();
                let receipt = next.releases.get_mut(id).unwrap();
                receipt.state = state;
                let receipt = receipt.clone();
                self.persist(next)?;
                Ok(Reply::Released { receipt })
            }
        }
    }
    fn closed(&mut self, id: &RequestId, transport: Transport) -> io::Result<()> {
        self.pending.remove(id);
        let mut next = self.record.clone();
        let Some(row) = next.requests.get_mut(id) else {
            return Ok(());
        };
        if row.transport == Transport::Stopped || row.transport == transport {
            return Ok(());
        }
        row.transport = transport;
        // This observes already-admitted work; ordinary exhaustion must not suppress it.
        self.charge(Class::Retirement, record_bytes(&next))?;
        self.persist(next)
    }
}

fn record_bytes(record: &Record) -> u64 {
    frontcap_core::canonical::canonical_of(record).len() as u64
}

pub(crate) fn save_budgeted<T: serde::Serialize>(
    budget: Option<&WriteBudget>,
    path: &std::path::Path,
    value: &T,
) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
        None => save_atomic(path, value),
    }
}

type HttpResponse = Response<Full<Bytes>>;
fn response(status: u16, body: String) -> HttpResponse {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}
fn rejected(status: u16, error: impl std::fmt::Display) -> HttpResponse {
    response(
        status,
        serde_json::to_string(&Reply::Rejected {
            reason: error.to_string(),
        })
        .unwrap(),
    )
}

pub struct Fixture {
    listener: TcpListener,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<Notify>,
    pub record: Record,
}
impl Fixture {
    /// A directory is a single generation. Existing evidence is never adopted or overwritten.
    pub async fn create(directory: PathBuf) -> io::Result<Self> {
        Self::create_with_assets(directory, Vec::new()).await
    }
    pub async fn create_with_assets(
        directory: PathBuf,
        inputs: Vec<assets::AssetInput>,
    ) -> io::Result<Self> {
        Self::create_bound(directory, inputs, None).await
    }
    /// Bind the ordinary endpoint compiled into a consumer without changing that product.
    pub async fn create_at(directory: PathBuf, endpoint: RequestEndpoint) -> io::Result<Self> {
        endpoint.validate().map_err(io::Error::other)?;
        Self::create_bound(directory, Vec::new(), Some(endpoint)).await
    }
    async fn create_bound(
        directory: PathBuf,
        inputs: Vec<assets::AssetInput>,
        endpoint: Option<RequestEndpoint>,
    ) -> io::Result<Self> {
        Self::create_budgeted(directory, inputs, endpoint, None).await
    }
    /// Use a launch-owned allowance shared with other cooperating writers.
    pub async fn create_budgeted(
        directory: PathBuf,
        inputs: Vec<assets::AssetInput>,
        endpoint: Option<RequestEndpoint>,
        budget: Option<WriteBudget>,
    ) -> io::Result<Self> {
        if let Some(endpoint) = &endpoint {
            endpoint.validate().map_err(io::Error::other)?;
        }
        std::fs::create_dir(&directory)?;
        let assets = assets::freeze(&directory, inputs, budget.as_ref())?;
        let port = endpoint.as_ref().map_or(0, |endpoint| endpoint.port);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let record = Record {
            generation: LaunchId::generate(),
            address: listener.local_addr()?,
            request_path: endpoint.map(|endpoint| endpoint.request_path),
            pid: std::process::id(),
            stopping: false,
            requests: BTreeMap::new(),
            releases: BTreeMap::new(),
        };
        let path = directory.join("state.json");
        save_budgeted(budget.as_ref(), &path, &record)?;
        Ok(Self {
            listener,
            record: record.clone(),
            stop: Arc::new(Notify::new()),
            shared: Arc::new(Mutex::new(Shared {
                record,
                path,
                pending: BTreeMap::new(),
                poisoned: false,
                assets,
                budget,
            })),
        })
    }
    pub async fn run(self) -> io::Result<()> {
        self.serve(true).await
    }
    async fn serve(self, handle_signals: bool) -> io::Result<()> {
        let mut connections = JoinSet::new();
        let permits = Arc::new(Semaphore::new(160));
        loop {
            tokio::select! {
                _ = self.stop.notified() => break,
                _ = tokio::signal::ctrl_c(), if handle_signals => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    result.map_err(io::Error::other)??;
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    let Ok(permit) = permits.clone().try_acquire_owned() else { drop(stream); continue; };
                    let shared = self.shared.clone(); let stop = self.stop.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        let admitted = Arc::new(Mutex::new(None)); let recorded = admitted.clone();
                        let service_shared = shared.clone();
                        let service = service_fn(move |request| route(request, service_shared.clone(), stop.clone(), admitted.clone()));
                        let result = tokio::time::timeout(CONNECTION_BUDGET,
                            hyper::server::conn::http1::Builder::new().max_buf_size(32768)
                                .serve_connection(TokioIo::new(stream), service)).await;
                        let transport = match result { Ok(Ok(())) => Transport::Closed, Ok(Err(_)) => Transport::Failed, Err(_) => Transport::TimedOut };
                        if let Some(id) = recorded.lock().await.as_ref() { shared.lock().await.closed(id, transport)?; }
                        Ok::<(), io::Error>(())
                    });
                }
            }
        }
        {
            let mut shared = self.shared.lock().await;
            let generation = shared.record.generation.clone();
            shared.control(Control::Stop { generation })?;
        }
        // Pending GETs are closed; a stop acknowledgement may flush before remaining connections retire.
        match tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(result) = connections.join_next().await {
                result.map_err(io::Error::other)??;
            }
            Ok::<(), io::Error>(())
        })
        .await
        {
            Ok(result) => result?,
            Err(_) => connections.abort_all(),
        }
        while let Some(result) = connections.join_next().await {
            if let Ok(result) = result {
                result?;
            }
        }
        Ok(())
    }
}

async fn route(
    request: Request<Incoming>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<Notify>,
    admitted: Arc<Mutex<Option<RequestId>>>,
) -> Result<HttpResponse, Infallible> {
    if request.method() == hyper::Method::POST
        && request.uri().path() == "/control"
        && request.uri().query().is_none()
    {
        let body = match Limited::new(request.into_body(), MAX_BODY * 6 + 8192)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(error) => return Ok(rejected(400, error)),
        };
        let command: Control = match serde_json::from_slice(&body) {
            Ok(command) => command,
            Err(error) => return Ok(rejected(400, error)),
        };
        let stopping = matches!(command, Control::Stop { .. });
        let (result, poisoned) = {
            let mut shared = shared.lock().await;
            let result = shared.control(command);
            (result, shared.poisoned)
        };
        if poisoned {
            stop.notify_one();
        }
        return Ok(match result {
            Ok(reply) => {
                if stopping {
                    stop.notify_one();
                }
                response(200, serde_json::to_string(&reply).unwrap())
            }
            Err(error) if poisoned => response(
                500,
                serde_json::to_string(&Reply::Uncertain {
                    reason: error.to_string(),
                })
                .unwrap(),
            ),
            Err(error) => rejected(409, error),
        });
    }
    if request.method() != hyper::Method::GET
        || request.headers().contains_key("transfer-encoding")
        || request
            .headers()
            .get("content-length")
            .is_some_and(|v| v != "0")
    {
        return Ok(rejected(400, "expected bodyless GET"));
    }
    let resource = {
        let shared = shared.lock().await;
        shared
            .assets
            .get(request.uri().path())
            .map(|asset| ResourceRequest {
                uri: request.uri().path_and_query().unwrap().as_str().to_owned(),
                asset: asset.identity.clone(),
            })
    };
    if let Some(resource) = resource {
        if resource.uri.len() > 2048 || request.headers().contains_key("range") {
            return Ok(rejected(400, "resource URI or range unsupported"));
        }
        let id = RequestId::generate();
        let received = {
            let mut shared = shared.lock().await;
            let result = shared.receive_resource(id.clone(), Some(resource));
            (result, shared.poisoned)
        };
        return Ok(deliver(received, id, admitted, stop).await);
    }
    if request.uri().query().is_some() {
        return Ok(rejected(400, "managed requests cannot contain a query"));
    }
    let (id, received) = {
        let mut shared = shared.lock().await;
        let path = request.uri().path();
        let suffix = if let Some(suffix) = shared
            .record
            .request_path
            .as_deref()
            .and_then(|prefix| path.strip_prefix(prefix))
        {
            suffix
        } else {
            let parts: Vec<_> = path.split('/').collect();
            if parts.len() != 4 || parts[2] != "requests" {
                return Ok(rejected(404, "unknown fixture endpoint"));
            }
            if parts[1] != shared.record.generation.as_str() {
                return Ok(rejected(409, "fixture generation differs"));
            }
            parts[3]
        };
        let id = match RequestId::new(suffix) {
            Ok(id) => id,
            Err(error) => return Ok(rejected(400, error)),
        };
        let result = shared.receive(id.clone());
        (id, (result, shared.poisoned))
    };
    Ok(deliver(received, id, admitted, stop).await)
}

async fn deliver(
    received: (io::Result<oneshot::Receiver<ReleasedBody>>, bool),
    id: RequestId,
    admitted: Arc<Mutex<Option<RequestId>>>,
    stop: Arc<Notify>,
) -> HttpResponse {
    let receiver = match received {
        (Ok(receiver), _) => receiver,
        (Err(error), true) => {
            stop.notify_one();
            return response(
                500,
                serde_json::to_string(&Reply::Uncertain {
                    reason: error.to_string(),
                })
                .unwrap(),
            );
        }
        (Err(error), false) => return rejected(409, error),
    };
    *admitted.lock().await = Some(id);
    match receiver.await {
        Ok(spec) => {
            let mut response = Response::builder()
                .status(spec.status)
                .header("content-type", spec.content_type)
                .header("cache-control", "no-store")
                .header("connection", "close");
            if let Some(policy) = spec.content_security_policy {
                response = response.header("content-security-policy", policy);
            }
            response.body(Full::new(spec.body)).unwrap()
        }
        Err(_) => rejected(503, "fixture stopped"),
    }
}

#[cfg(test)]
#[path = "fixture_tests.rs"]
mod tests;
