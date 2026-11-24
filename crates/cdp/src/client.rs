//! Transport envelopes are parsed generically to route by `id` and `sessionId`; every payload is then
//! decoded into a generated protocol type before it leaves this module.
use crate::{Command, Event};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("protocol error {code}: {message}")]
    Protocol {
        code: i64,
        message: String,
        data: Option<String>,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("cannot decode {method}: {source}")]
    Decode {
        method: String,
        source: serde_json::Error,
    },
    #[error("connection closed")]
    Closed,
    /// The session's renderer crashed (`Inspector.targetCrashed`): its calls are never answered.
    #[error("the target's renderer crashed")]
    TargetCrashed,
    /// The browser detached this session; it can no longer answer commands.
    #[error("the target session detached")]
    SessionDetached,
}

/// Where an event came in the connection's stream: events of one connection compare by it, whichever
/// subscriptions they were delivered to. One session's events come in the order its target sent them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Arrival(pub u64);

type Stamped = (Arrival, Option<Arc<str>>, serde_json::Value);

/// An event with where it came in the connection's stream and the session it came on (`None`: the
/// browser's own). A subscription that other sessions' events are routed to tells them apart by it.
#[derive(Debug, Clone)]
pub struct Tagged<E> {
    pub arrival: Arrival,
    pub session: Option<Arc<str>>,
    pub event: E,
}

/// One subscriber: events of `method` (and `session`, when set) are routed to it in arrival order, with
/// no bound, so a subscriber that is read late never loses events.
struct Route {
    method: &'static str,
    session: Option<String>,
    tx: mpsc::UnboundedSender<Stamped>,
}

/// `None` once the connection has closed; checked under the same lock that registers a call, so a call
/// can never be registered after the reader has failed the pending ones.
type Pending = Mutex<Option<Calls>>;

type Waiter = oneshot::Sender<Result<serde_json::Value, CdpError>>;

/// The calls awaiting an answer, with the session each addresses, and the sessions whose renderer
/// crashed or detached: neither can answer, so pending and later calls fail at once.
#[derive(Default)]
struct Calls {
    waiting: HashMap<u64, (Option<Arc<str>>, Waiter)>,
    crashed: std::collections::HashSet<Arc<str>>,
    detached: std::collections::HashSet<Arc<str>>,
}

impl Calls {
    /// Registers call `id`, or refuses a crashed or detached session.
    fn register(&mut self, id: u64, session: Option<&str>, tx: Waiter) -> Result<(), CdpError> {
        if session.is_some_and(|s| self.crashed.contains(s)) {
            return Err(CdpError::TargetCrashed);
        }
        if session.is_some_and(|s| self.detached.contains(s)) {
            return Err(CdpError::SessionDetached);
        }
        self.waiting.insert(id, (session.map(Arc::from), tx));
        Ok(())
    }

    fn crashed(&mut self, session: &Arc<str>) {
        self.crashed.insert(session.clone());
        let ids: Vec<u64> = self.waiting.iter().filter(|(_, (s, _))| s.as_ref() == Some(session)).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some((_, tx)) = self.waiting.remove(&id) {
                let _ = tx.send(Err(CdpError::TargetCrashed));
            }
        }
    }

    fn detached(&mut self, session: &str) {
        self.detached.insert(Arc::from(session));
        let ids: Vec<u64> = self.waiting.iter().filter(|(_, (s, _))| s.as_deref() == Some(session)).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some((_, tx)) = self.waiting.remove(&id) {
                let _ = tx.send(Err(CdpError::SessionDetached));
            }
        }
    }
}

struct Inner {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    routes: Mutex<Vec<Route>>,
    /// Events numbered in the order they reach subscribers.
    arrivals: AtomicU64,
    #[cfg(feature = "test-hooks")]
    delays: Mutex<Vec<Delay>>,
    #[cfg(feature = "test-hooks")]
    omitted_answers: Mutex<OmittedAnswers>,
    closed: watch::Sender<bool>,
    /// The writer and reader tasks; aborting them drops the socket.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// Closes the connection when the last `Client` handle goes away. The transport tasks hold `Inner`, not
/// this, so they never keep a connection alive on their own.
struct Owner(Arc<Inner>);

impl Drop for Owner {
    fn drop(&mut self) {
        close_inner(&self.0);
    }
}

impl Inner {
    /// Routes an event to its subscribers under the next arrival.
    fn deliver(&self, method: &str, tag: Option<Arc<str>>, params: serde_json::Value) {
        let arrival = Arrival(self.arrivals.fetch_add(1, Ordering::SeqCst) + 1);
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|r| !r.tx.is_closed());
        for r in routes.iter().filter(|r| r.method == method && (r.session.is_none() || r.session.as_deref() == tag.as_deref())) {
            let _ = r.tx.send((arrival, tag.clone(), params.clone()));
        }
    }

    #[cfg(feature = "test-hooks")]
    fn omit_answer_if_requested(&self, id: u64, method: &str, params: &serde_json::Value) {
        let mut omitted = self.omitted_answers.lock().unwrap();
        if let Some(at) = omitted.matching.iter().position(|(m, part)| m == method && params.to_string().contains(part)) {
            omitted.matching.remove(at);
            omitted.ids.insert(id);
        }
    }

    #[cfg(feature = "test-hooks")]
    fn delay_of(&self, method: &str, params: &serde_json::Value) -> Option<Duration> {
        let delays = self.delays.lock().unwrap();
        let text = params.to_string();
        delays.iter().find(|d| d.method == method && text.contains(&d.contains)).map(|d| d.by)
    }
}

#[cfg(feature = "test-hooks")]
#[derive(Default)]
struct OmittedAnswers {
    matching: Vec<(String, String)>,
    ids: std::collections::HashSet<u64>,
}

/// An event the connection hands over late, as a peer that sent it later would.
#[cfg(feature = "test-hooks")]
struct Delay {
    method: String,
    contains: String,
    by: Duration,
}

fn close_inner(inner: &Inner) {
    for t in inner.tasks.lock().unwrap().drain(..) {
        t.abort();
    }
    if let Some(pending) = inner.pending.lock().unwrap().take() {
        for (_, (_, tx)) in pending.waiting {
            let _ = tx.send(Err(CdpError::Closed));
        }
    }
    inner.closed.send_replace(true);
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    _owner: Arc<Owner>,
}

pub struct Subscription<E> {
    rx: mpsc::UnboundedReceiver<Stamped>,
    tx: mpsc::UnboundedSender<Stamped>,
    closed: watch::Receiver<bool>,
    _event: PhantomData<E>,
}

/// Where a subscription receives: [`Caller::route`] adds another session's events of the same kind to it,
/// in the order the connection delivers them.
#[derive(Clone)]
pub struct Feed<E> {
    tx: mpsc::UnboundedSender<Stamped>,
    _event: PhantomData<fn() -> E>,
}

impl Client {
    pub async fn connect(ws_url: &str) -> Result<Client, CdpError> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| CdpError::Transport(e.to_string()))?;
        Ok(Self::over(ws))
    }

    /// Connects over a TCP connection the caller opened: one made inside another network namespace keeps
    /// it, so a browser reachable only there is still driven from here.
    pub async fn connect_over(stream: tokio::net::TcpStream, ws_url: &str) -> Result<Client, CdpError> {
        let (ws, _) = tokio_tungstenite::client_async(ws_url, stream)
            .await
            .map_err(|e| CdpError::Transport(e.to_string()))?;
        Ok(Self::over(ws))
    }

    fn over<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> Client
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (closed, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            outgoing: tx,
            pending: Mutex::new(Some(Calls::default())),
            next_id: AtomicU64::new(1),
            routes: Mutex::new(Vec::new()),
            arrivals: AtomicU64::new(0),
            #[cfg(feature = "test-hooks")]
            delays: Mutex::new(Vec::new()),
            #[cfg(feature = "test-hooks")]
            omitted_answers: Mutex::new(Default::default()),
            closed,
            tasks: Mutex::new(Vec::new()),
        });
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });
        let reader = inner.clone();
        let read_task = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let text = match msg {
                    Ok(Message::Text(text)) => text,
                    Ok(_) => continue,
                    Err(_) => break,
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    #[cfg(feature = "test-hooks")]
                    if reader.omitted_answers.lock().unwrap().ids.remove(&id) {
                        continue;
                    }
                    let result = if let Some(err) = v.get("error") {
                        Err(CdpError::Protocol {
                            code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("")
                                .to_string(),
                            data: err.get("data").and_then(|d| d.as_str()).map(str::to_string),
                        })
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(serde_json::json!({})))
                    };
                    let waiter = reader
                        .pending
                        .lock()
                        .unwrap()
                        .as_mut()
                        .and_then(|p| p.waiting.remove(&id));
                    if let Some((_, tx)) = waiter {
                        let _ = tx.send(result);
                    }
                } else if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                    let tag: Option<Arc<str>> = v.get("sessionId").and_then(|s| s.as_str()).map(Arc::from);
                    if method == "Inspector.targetCrashed"
                        && let Some(s) = &tag
                        && let Some(pending) = reader.pending.lock().unwrap().as_mut()
                    {
                        pending.crashed(s);
                    }
                    let params = v.get("params").cloned().unwrap_or(serde_json::json!({}));
                    // The envelope names the parent (or browser); params names
                    // the session that can no longer answer its pending calls.
                    if method == "Target.detachedFromTarget"
                        && let Some(session) = params.get("sessionId").and_then(|s| s.as_str())
                        && let Some(pending) = reader.pending.lock().unwrap().as_mut()
                    {
                        pending.detached(session);
                    }
                    #[cfg(feature = "test-hooks")]
                    if let Some(by) = reader.delay_of(method, &params) {
                        let (reader, method) = (reader.clone(), method.to_string());
                        tokio::spawn(async move {
                            tokio::time::sleep(by).await;
                            reader.deliver(&method, tag, params);
                        });
                        continue;
                    }
                    reader.deliver(method, tag, params);
                }
            }
            let pending = reader.pending.lock().unwrap().take();
            for (_, (_, tx)) in pending.into_iter().flat_map(|p| p.waiting) {
                let _ = tx.send(Err(CdpError::Closed));
            }
            reader.closed.send_replace(true);
        });
        inner.tasks.lock().unwrap().extend([writer, read_task]);
        Client {
            _owner: Arc::new(Owner(inner.clone())),
            inner,
        }
    }

    /// Ends the connection for every clone of this client: the socket is dropped, pending calls fail,
    /// and subscriptions end.
    pub fn close(&self) {
        close_inner(&self.inner);
    }

    /// Delivers `method` events whose parameters contain `contains` `by` later than they arrive, after
    /// the events that arrive meanwhile.
    #[cfg(feature = "test-hooks")]
    pub fn delay_events(&self, method: &str, contains: &str, by: Duration) {
        self.inner.delays.lock().unwrap().push(Delay { method: method.into(), contains: contains.into(), by });
    }

    /// A handle that can call but does not keep the connection open: for tasks that live beside the
    /// connection's owners and must end with it.
    pub fn caller(&self) -> Caller {
        Caller {
            inner: self.inner.clone(),
        }
    }

    /// Leaves the next matching command pending even if its answer arrives, as a
    /// target destroyed before replying would. The detach still fails the call.
    #[cfg(feature = "test-hooks")]
    pub fn omit_next_answer(&self, method: &str, contains: &str) {
        self.inner.omitted_answers.lock().unwrap().matching.push((method.into(), contains.into()));
    }

    pub async fn call<C: Command>(
        &self,
        session: Option<&str>,
        params: &C,
    ) -> Result<C::Returns, CdpError> {
        call_inner(&self.inner, session, params).await
    }

    /// Sends a command without waiting for its answer: for a command the engine answers only later (a
    /// held mouse move while virtual time is paused). The answer, an error included, arrives on the
    /// returned receiver.
    pub fn send<C: Command>(&self, session: Option<&str>, params: &C) -> Result<Answer, CdpError> {
        send_inner(&self.inner, session, params)
    }

    pub fn subscribe<E: Event>(&self, session: Option<&str>) -> Subscription<E> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.routes.lock().unwrap().push(Route {
            method: E::NAME,
            session: session.map(str::to_string),
            tx: tx.clone(),
        });
        Subscription {
            rx,
            tx,
            closed: self.inner.closed.subscribe(),
            _event: PhantomData,
        }
    }
}

/// The answer to a command sent with [`Client::send`].
pub struct Answer {
    rx: oneshot::Receiver<Result<serde_json::Value, CdpError>>,
}

impl Answer {
    /// `None` while unanswered; `Some(Err)` when the engine refused the command or the connection ended.
    pub fn try_answer(&mut self) -> Option<Result<(), CdpError>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r.map(|_| ())),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(CdpError::Closed)),
        }
    }

    /// Waits for the answer.
    pub async fn answered(self) -> Result<(), CdpError> {
        self.rx.await.map_err(|_| CdpError::Closed)?.map(|_| ())
    }
}

/// Calls on a connection without owning it; fails with `Closed` once the connection's owners are gone.
#[derive(Clone)]
pub struct Caller {
    inner: Arc<Inner>,
}

impl Caller {
    pub async fn call<C: Command>(
        &self,
        session: Option<&str>,
        params: &C,
    ) -> Result<C::Returns, CdpError> {
        call_inner(&self.inner, session, params).await
    }

    /// Sends a command without waiting for its answer ([`Client::send`]).
    pub fn send<C: Command>(&self, session: Option<&str>, params: &C) -> Result<Answer, CdpError> {
        send_inner(&self.inner, session, params)
    }

    pub fn subscribe<E: Event>(&self, session: Option<&str>) -> Subscription<E> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.routes.lock().unwrap().push(Route {
            method: E::NAME,
            session: session.map(str::to_string),
            tx: tx.clone(),
        });
        Subscription {
            rx,
            tx,
            closed: self.inner.closed.subscribe(),
            _event: PhantomData,
        }
    }

    /// Delivers `session`'s events of the feed's kind to its subscription as well.
    pub fn route<E: Event>(&self, session: &str, feed: &Feed<E>) {
        self.inner.routes.lock().unwrap().push(Route {
            method: E::NAME,
            session: Some(session.to_string()),
            tx: feed.tx.clone(),
        });
    }

    /// Drops every route of `session` (a detached target's).
    pub fn unroute(&self, session: &str) {
        self.inner.routes.lock().unwrap().retain(|r| r.session.as_deref() != Some(session));
    }
}

fn send_inner<C: Command>(inner: &Inner, session: Option<&str>, params: &C) -> Result<Answer, CdpError> {
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let mut envelope = serde_json::json!({ "id": id, "method": C::METHOD, "params": params });
    if let Some(s) = session {
        envelope["sessionId"] = serde_json::Value::String(s.to_string());
    }
    let (tx, rx) = oneshot::channel();
    match inner.pending.lock().unwrap().as_mut() {
        Some(pending) => pending.register(id, session, tx)?,
        None => return Err(CdpError::Closed),
    };
    #[cfg(feature = "test-hooks")]
    inner.omit_answer_if_requested(id, C::METHOD, &envelope["params"]);
    inner.outgoing.send(envelope.to_string()).map_err(|_| CdpError::Closed)?;
    Ok(Answer { rx })
}

async fn call_inner<C: Command>(
    inner: &Inner,
    session: Option<&str>,
    params: &C,
) -> Result<C::Returns, CdpError> {
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let mut envelope = serde_json::json!({ "id": id, "method": C::METHOD, "params": params });
    if let Some(s) = session {
        envelope["sessionId"] = serde_json::Value::String(s.to_string());
    }
    let (tx, rx) = oneshot::channel();
    match inner.pending.lock().unwrap().as_mut() {
        Some(pending) => pending.register(id, session, tx)?,
        None => return Err(CdpError::Closed),
    };
    #[cfg(feature = "test-hooks")]
    inner.omit_answer_if_requested(id, C::METHOD, &envelope["params"]);
    inner
        .outgoing
        .send(envelope.to_string())
        .map_err(|_| CdpError::Closed)?;
    let value = rx.await.map_err(|_| CdpError::Closed)??;
    serde_json::from_value(value).map_err(|source| CdpError::Decode {
        method: C::METHOD.to_string(),
        source,
    })
}

impl<E: Event> Subscription<E> {
    pub fn feed(&self) -> Feed<E> {
        Feed { tx: self.tx.clone(), _event: PhantomData }
    }

    fn decode(params: serde_json::Value) -> Result<E, CdpError> {
        serde_json::from_value(params).map_err(|source| CdpError::Decode {
            method: E::NAME.to_string(),
            source,
        })
    }

    pub async fn next(&mut self) -> Result<E, CdpError> {
        self.next_stamped().await.map(|(_, e)| e)
    }

    /// The next event and where it came in the connection's stream.
    pub async fn next_stamped(&mut self) -> Result<(Arrival, E), CdpError> {
        self.next_tagged().await.map(|t| (t.arrival, t.event))
    }

    /// The next event with where it came and the session it came on.
    pub async fn next_tagged(&mut self) -> Result<Tagged<E>, CdpError> {
        let received = tokio::select! {
            biased;
            r = self.rx.recv() => r,
            _ = self.closed.wait_for(|closed| *closed) => None,
        };
        match received {
            Some((arrival, session, params)) => Self::decode(params).map(|event| Tagged { arrival, session, event }),
            None => Err(CdpError::Closed),
        }
    }

    /// The next event if one has arrived, without waiting.
    pub fn try_next(&mut self) -> Result<Option<E>, CdpError> {
        self.try_next_stamped().map(|o| o.map(|(_, e)| e))
    }

    pub fn try_next_stamped(&mut self) -> Result<Option<(Arrival, E)>, CdpError> {
        self.try_next_tagged().map(|o| o.map(|t| (t.arrival, t.event)))
    }

    pub fn try_next_tagged(&mut self) -> Result<Option<Tagged<E>>, CdpError> {
        match self.rx.try_recv() {
            Ok((arrival, session, params)) => Self::decode(params).map(|event| Some(Tagged { arrival, session, event })),
            Err(mpsc::error::TryRecvError::Empty) if !*self.closed.borrow() => Ok(None),
            Err(_) => Err(CdpError::Closed),
        }
    }

    pub async fn next_timeout(&mut self, d: Duration) -> Result<Option<E>, CdpError> {
        self.next_timeout_stamped(d).await.map(|o| o.map(|(_, e)| e))
    }

    pub async fn next_timeout_stamped(&mut self, d: Duration) -> Result<Option<(Arrival, E)>, CdpError> {
        self.next_timeout_tagged(d).await.map(|o| o.map(|t| (t.arrival, t.event)))
    }

    pub async fn next_timeout_tagged(&mut self, d: Duration) -> Result<Option<Tagged<E>>, CdpError> {
        if d.is_zero() {
            return self.try_next_tagged();
        }
        match tokio::time::timeout(d, self.next_tagged()).await {
            Ok(r) => r.map(Some),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
