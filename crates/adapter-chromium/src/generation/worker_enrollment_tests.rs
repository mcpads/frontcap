use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

struct Peer {
    client: cdp::Client,
    requests: mpsc::UnboundedReceiver<Value>,
    replies: mpsc::UnboundedSender<Value>,
}

async fn peer() -> Peer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, requests) = mpsc::unbounded_channel();
    let (replies, mut outgoing) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        loop {
            tokio::select! {
                message = socket.next() => match message {
                    Some(Ok(Message::Text(text))) => { if sent.send(serde_json::from_str(text.as_str()).unwrap()).is_err() { break; } },
                    _ => break,
                },
                reply = outgoing.recv() => match reply {
                    Some(reply) => { if socket.send(Message::Text(reply.to_string().into())).await.is_err() { break; } },
                    None => break,
                },
            }
        }
    });
    Peer { client: cdp::Client::connect(&format!("ws://{address}")).await.unwrap(), requests, replies }
}

fn join(peer: &Peer) -> Workers { join_with_waiting(peer, true) }

fn join_with_waiting(peer: &Peer, waiting: bool) -> Workers {
    let feeds = NetworkFeeds {
        worker_changes: Default::default(),
        attached: peer.client.subscribe::<target::AttachedToTargetEvent>(Some("page")).feed(),
        sent: peer.client.subscribe::<cnet::RequestWillBeSentEvent>(Some("page")).feed(),
        response: peer.client.subscribe::<cnet::ResponseReceivedEvent>(Some("page")).feed(),
        finished: peer.client.subscribe::<cnet::LoadingFinishedEvent>(Some("page")).feed(),
        failed: peer.client.subscribe::<cnet::LoadingFailedEvent>(Some("page")).feed(),
    };
    let book = join_workers_to_virtual_time(peer.client.caller(), "page".into(), peer.client.subscribe(Some("page")), peer.client.subscribe(Some("page")), feeds);
    peer.replies.send(json!({"sessionId":"page","method":"Target.attachedToTarget","params":{
        "sessionId":"worker","waitingForDebugger":waiting,"targetInfo":{
            "targetId":"native-worker","type":"worker","title":"same.js","url":"https://example.test/same.js","attached":true,"canAccessOpener":false
        }
    }})).unwrap();
    book
}

async fn next(peer: &mut Peer, method: &str) -> Value {
    let request = tokio::time::timeout(Duration::from_secs(5), peer.requests.recv()).await.unwrap().unwrap();
    assert_eq!(request["method"], method);
    assert_eq!(request["sessionId"], "worker");
    request
}

fn answer(peer: &Peer, request: &Value) {
    let result = if request["method"] == "Emulation.setVirtualTimePolicy" { json!({"virtualTimeTicksBase":1.0}) } else { json!({}) };
    peer.replies.send(json!({"id":request["id"],"sessionId":"worker","result":result})).unwrap();
}

async fn finished(book: &Workers) {
    let changed = book.lock().unwrap().changed.clone();
    tokio::time::timeout(Duration::from_secs(5), changed.notified()).await.unwrap();
}

const STEPS: [&str; 4] = ["Network.enable", "Emulation.setVirtualTimePolicy", "Target.setAutoAttach", "Runtime.runIfWaitingForDebugger"];

#[tokio::test]
async fn a_worker_is_ready_only_after_every_setup_and_resume_acknowledgement() {
    let mut peer = peer().await;
    let book = join(&peer);
    for method in STEPS {
        let request = next(&mut peer, method).await;
        assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Joining);
        answer(&peer, &request);
    }
    finished(&book).await;
    assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Ready);
    assert!(book.lock().unwrap().failure.is_none());
}

#[tokio::test]
async fn a_rejected_setup_step_stops_the_chain_and_latches_its_failure() {
    for rejected in 0..STEPS.len() {
        let mut peer = peer().await;
        let book = join(&peer);
        for method in &STEPS[..rejected] {
            let request = next(&mut peer, method).await;
            answer(&peer, &request);
        }
        let request = next(&mut peer, STEPS[rejected]).await;
        peer.replies.send(json!({"id":request["id"],"sessionId":"worker","error":{"code":-32000,"message":"injected setup refusal"}})).unwrap();
        finished(&book).await;
        let mut workers = book.lock().unwrap();
        assert_eq!(workers.sessions["worker"].enrollment, WorkerEnrollment::Failed);
        let failure = workers.failure.as_ref().unwrap();
        assert_eq!(failure.target, "native-worker");
        assert_eq!(failure.method, STEPS[rejected]);
        assert!(failure.detail.contains("injected setup refusal"));
        assert!(peer.requests.try_recv().is_err(), "no later step, especially resume, may be sent");
        workers.forget("worker");
        assert!(workers.failure.is_some(), "retirement cannot erase an observed setup failure");
    }
}

#[tokio::test]
async fn a_detached_worker_is_retired_without_setup_failure_or_resume() {
    let mut peer = peer().await;
    let book = join(&peer);
    let _request = next(&mut peer, STEPS[0]).await;
    peer.replies.send(json!({"sessionId":"page","method":"Target.detachedFromTarget","params":{"sessionId":"worker","targetId":"native-worker"}})).unwrap();
    finished(&book).await;
    assert!(book.lock().unwrap().sessions.is_empty());
    assert!(book.lock().unwrap().failure.is_none());
    assert!(peer.requests.try_recv().is_err());
}

#[tokio::test]
async fn retirement_during_a_setup_call_prevents_the_next_command() {
    let mut peer = peer().await;
    let book = join(&peer);
    let request = next(&mut peer, STEPS[0]).await;
    book.lock().unwrap().forget("worker");
    answer(&peer, &request);
    finished(&book).await;
    assert!(book.lock().unwrap().failure.is_none());
    assert!(peer.requests.try_recv().is_err());
}

#[tokio::test]
async fn a_setup_timeout_keeps_the_worker_unresumed_and_reports_the_failed_step() {
    let mut peer = peer().await;
    let book = join(&peer);
    let _request = next(&mut peer, STEPS[0]).await;
    tokio::time::pause();
    tokio::time::advance(Session::CALL_TIMEOUT + Duration::from_millis(1)).await;
    finished(&book).await;
    assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Failed);
    assert_eq!(book.lock().unwrap().failure.as_ref().unwrap().method, STEPS[0]);
    assert!(peer.requests.try_recv().is_err());
}

#[tokio::test]
async fn a_synchronous_wait_marker_is_retained_before_enrollment_finishes() {
    let mut peer = peer().await;
    let book = join(&peer);
    let request = next(&mut peer, STEPS[0]).await;
    peer.replies.send(json!({"sessionId":"worker","method":"Inspector.frontcapSynchronousWait","params":{"requestId":"17"}})).unwrap();
    finished(&book).await;
    {
        let workers = book.lock().unwrap();
        let worker = &workers.sessions["worker"];
        assert_eq!(worker.enrollment, WorkerEnrollment::Joining);
        assert_eq!(worker.synchronous_wait.as_ref().unwrap().0, "17");
    }
    answer(&peer, &request);
    for method in &STEPS[1..] {
        let request = next(&mut peer, method).await;
        answer(&peer, &request);
    }
    finished(&book).await;
    let workers = book.lock().unwrap();
    assert_eq!(workers.sessions["worker"].enrollment, WorkerEnrollment::Ready);
    assert_eq!(workers.sessions["worker"].synchronous_wait.as_ref().unwrap().0, "17");
}


#[tokio::test]
async fn a_reattached_worker_retains_its_setup_call_only_with_current_native_wait_proof() {
    for blocked in [false, true] {
        let mut peer = peer().await;
        let book = join_with_waiting(&peer, false);
        let original = next(&mut peer, STEPS[0]).await;
        tokio::time::pause();
        tokio::time::advance(Session::CALL_TIMEOUT + Duration::from_millis(1)).await;
        tokio::time::resume();
        let probe = tokio::time::timeout(Duration::from_secs(5), peer.requests.recv()).await.expect("first inventory probe").unwrap();
        assert_eq!(probe["sessionId"], "page");
        assert_eq!(probe["method"], "Emulation.frontcapGetWorkers");
        let workers = if blocked { json!([{"targetId":"native-worker","creatorId":"document",
            "creatorIsWorker":false,"sequence":"1","scriptUrl":"https://example.test/same.js",
            "threadStarted":true,"synchronousWaitId":"17"}]) } else { json!([]) };
        peer.replies.send(json!({"id":probe["id"],"sessionId":"page","result":{"workers":workers}})).unwrap();
        if blocked {
            let second_probe = tokio::time::timeout(Session::CALL_TIMEOUT + Duration::from_secs(5), peer.requests.recv()).await.expect("second inventory probe").unwrap();
            assert_eq!(second_probe["method"], "Emulation.frontcapGetWorkers");
            assert_eq!(second_probe["sessionId"], "page");
            assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Joining);
            answer(&peer, &original);
            for method in &STEPS[1..3] {
                let request = next(&mut peer, method).await;
                answer(&peer, &request);
            }
            finished(&book).await;
            assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Ready);
            assert!(book.lock().unwrap().failure.is_none());
        } else {
            finished(&book).await;
            assert_eq!(book.lock().unwrap().sessions["worker"].enrollment, WorkerEnrollment::Failed);
        }
        assert!(peer.requests.try_recv().is_err(), "the original setup command is not reissued");
    }
}
