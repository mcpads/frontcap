use super::*;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn release(record: &Record, command: &str, request: &str, status: u16, body: &str) -> Control {
    Control::Release {
        generation: record.generation.clone(),
        id: id(command),
        request: id(request),
        response: ResponseSpec {
            status,
            body: body.into(),
        },
    }
}

#[tokio::test]
async fn fixed_consumer_endpoint_keeps_owned_release_and_generation_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = RequestEndpoint {
        port: reserved.local_addr().unwrap().port(),
        request_path: "/shared/".into(),
    };
    let owner = LaunchId::generate();
    assert!(
        owned::OwnedFixture::start_at(
            root.path().join("occupied"),
            owner.clone(),
            endpoint.clone()
        )
        .await
        .is_err()
    );
    assert!(!root.path().join("occupied/owner.json").exists());
    drop(reserved);
    let fixture =
        owned::OwnedFixture::start_at(root.path().join("first"), owner.clone(), endpoint.clone())
            .await
            .unwrap();
    let record = fixture.initial().clone();
    assert_eq!(record.requests_path(), "/shared/");
    assert_eq!(record.address.port(), endpoint.port);
    assert!(record.address.ip().is_loopback());
    let address = record.address;
    let mut pending = tokio::spawn(async move { http(address, "GET", "/shared/load", "").await });
    wait_received(&record, "load").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    let decision = release(&record, "send", "load", 200, "[1]");
    assert!(matches!(
        fixture
            .controller()
            .exchange(&LaunchId::generate(), decision.clone())
            .await,
        Reply::Rejected { .. }
    ));
    let first = fixture.control(&owner, decision.clone()).await.unwrap();
    assert_eq!(pending.await.unwrap(), (200, "[1]".into()));
    assert_eq!(
        fixture.control(&owner, decision.clone()).await.unwrap(),
        first
    );
    assert_eq!(http(address, "GET", "/shared/load/extra", "").await.0, 400);
    assert_eq!(http(address, "GET", "/shared/query?x=1", "").await.0, 400);
    fixture.shutdown().await.unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());

    let replacement =
        owned::OwnedFixture::start_at(root.path().join("second"), LaunchId::generate(), endpoint)
            .await
            .unwrap();
    let next = replacement.initial().clone();
    assert_ne!(record.generation, next.generation);
    assert!(next.requests.is_empty() && next.releases.is_empty());
    assert!(
        replacement
            .control(&replacement.binding().owner, decision)
            .await
            .is_err()
    );
    assert_eq!(
        http(
            address,
            "GET",
            &format!("/{}/requests/old", record.generation),
            ""
        )
        .await
        .0,
        409
    );
    replacement.shutdown().await.unwrap();
    let saved: Record =
        serde_json::from_slice(&std::fs::read(root.path().join("first/state.json")).unwrap())
            .unwrap();
    assert_eq!(saved.releases.len(), 1);
    assert!(saved.stopping);
}

#[tokio::test]
async fn invalid_consumer_endpoints_reject_before_creating_evidence() {
    let root = tempfile::tempdir().unwrap();
    for (port, path) in [
        (0, "/shared/"),
        (5000, "/"),
        (5000, "relative/"),
        (5000, "/shared"),
        (5000, "/../shared/"),
        (5000, "/shared//"),
        (5000, "/shared/?"),
        (5000, "/%2f/"),
    ] {
        let output = root.path().join("rejected");
        assert!(
            Fixture::create_at(
                output.clone(),
                RequestEndpoint {
                    port,
                    request_path: path.into()
                }
            )
            .await
            .is_err()
        );
        assert!(!output.exists());
    }
}
async fn http(address: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.into(),
    )
}
async fn command(record: &Record, value: &Control) -> Reply {
    let (status, body) = http(
        record.address,
        "POST",
        "/control",
        &serde_json::to_string(value).unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}
async fn wait_received(record: &Record, expected: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Reply::Snapshot { record: snapshot } = command(
                record,
                &Control::Inspect {
                    generation: record.generation.clone(),
                },
            )
            .await
            else {
                panic!()
            };
            if snapshot.requests.contains_key(&id(expected)) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn actual_http_waits_for_release_and_retries_preserve_response_identity() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("run");
    let fixture = Fixture::create(directory.clone()).await.unwrap();
    let record = fixture.record.clone();
    let server = tokio::spawn(fixture.run());
    let path = format!("/{}/requests/load", record.generation);
    let address = record.address;
    let mut pending = tokio::spawn(async move { http(address, "GET", &path, "").await });
    wait_received(&record, "load").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    let decision = release(
        &record,
        "release-load",
        "load",
        200,
        "[{\"title\":\"한글\"}]",
    );
    let Reply::Released { receipt } = command(&record, &decision).await else {
        panic!()
    };
    assert_eq!(receipt.state, ReleaseState::QueuedForHttp);
    assert_eq!(
        pending.await.unwrap(),
        (200, "[{\"title\":\"한글\"}]".into())
    );
    let Reply::Released { receipt: replayed } = command(&record, &decision).await else {
        panic!()
    };
    assert_eq!(receipt, replayed);
    let conflict = release(&record, "release-load", "load", 503, "{}");
    assert_eq!(
        http(
            address,
            "POST",
            "/control",
            &serde_json::to_string(&conflict).unwrap()
        )
        .await
        .0,
        409
    );
    assert_eq!(
        http(
            address,
            "GET",
            &format!("/{}/requests/load", record.generation),
            ""
        )
        .await
        .0,
        409
    );
    assert_eq!(
        http(address, "GET", "/another/requests/new", "").await.0,
        409
    );
    assert!(matches!(
        command(
            &record,
            &Control::Stop {
                generation: record.generation.clone()
            }
        )
        .await,
        Reply::Stopping
    ));
    server.await.unwrap().unwrap();
    let saved: Record =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert_eq!(saved.requests.len(), 1);
    assert_eq!(saved.releases.len(), 1);
    assert!(saved.stopping);
    assert_eq!(saved.requests[&id("load")].transport, Transport::Closed);
    assert!(Fixture::create(directory).await.is_err());
}

#[tokio::test]
async fn independent_requests_allow_empty_error_retry_and_obsolete_response_order() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let record = fixture.record.clone();
    let server = tokio::spawn(fixture.run());
    let mut receivers = BTreeMap::new();
    for name in ["obsolete", "empty", "error", "retry"] {
        let path = format!("/{}/requests/{name}", record.generation);
        let address = record.address;
        receivers.insert(
            name,
            tokio::spawn(async move { http(address, "GET", &path, "").await }),
        );
        wait_received(&record, name).await;
    }
    for (name, status, body) in [
        ("empty", 200, "[]"),
        ("error", 503, "{\"error\":\"offline\"}"),
        ("retry", 200, "[1]"),
        ("obsolete", 200, "[0]"),
    ] {
        let result = command(
            &record,
            &release(&record, &format!("send-{name}"), name, status, body),
        )
        .await;
        assert!(matches!(result, Reply::Released { .. }));
        assert_eq!(
            receivers.remove(name).unwrap().await.unwrap(),
            (status, body.into())
        );
    }
    command(
        &record,
        &Control::Stop {
            generation: record.generation.clone(),
        },
    )
    .await;
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn terminal_requests_retain_one_late_release_without_delivering_to_another_request() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let mut shared = fixture.shared.lock().await;
    let mut current = shared.receive(id("current")).unwrap();
    for (name, transport) in [
        ("closed", Transport::Closed),
        ("failed", Transport::Failed),
        ("timed-out", Transport::TimedOut),
    ] {
        let mut old = shared.receive(id(name)).unwrap();
        shared.closed(&id(name), transport).unwrap();
        assert!(matches!(
            old.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let decision = release(&fixture.record, &format!("send-{name}"), name, 200, "[0]");
        let reply = shared.control(decision.clone()).unwrap();
        let Reply::Released { receipt } = &reply else {
            panic!()
        };
        assert_eq!(receipt.command, decision);
        assert_eq!(receipt.state, ReleaseState::ReceiverGone);
        let saved: Record = serde_json::from_slice(&std::fs::read(&shared.path).unwrap()).unwrap();
        assert_eq!(saved.requests[&id(name)].transport, transport);
        assert_eq!(
            saved.requests[&id(name)].release,
            Some(id(&format!("send-{name}")))
        );
        assert_eq!(saved.releases[&id(&format!("send-{name}"))], *receipt);
        assert_eq!(shared.control(decision).unwrap(), reply);
        assert!(
            shared
                .control(release(
                    &fixture.record,
                    &format!("send-{name}"),
                    name,
                    503,
                    "[]"
                ))
                .is_err()
        );
        assert!(
            shared
                .control(release(
                    &fixture.record,
                    &format!("second-{name}"),
                    name,
                    200,
                    "[0]"
                ))
                .is_err()
        );
        assert!(matches!(
            current.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(shared.record.requests[&id("current")].release.is_none());
    }
    let before = shared.record.clone();
    assert!(
        shared
            .control(release(
                &fixture.record,
                "send-unknown",
                "unknown",
                200,
                "[]"
            ))
            .is_err()
    );
    assert_eq!(shared.record, before);
    shared
        .control(Control::Stop {
            generation: fixture.record.generation.clone(),
        })
        .unwrap();
    assert!(
        shared
            .control(release(
                &fixture.record,
                "send-stopped",
                "current",
                200,
                "[]"
            ))
            .is_err()
    );
    assert!(shared.record.requests[&id("current")].release.is_none());
}

#[tokio::test]
async fn release_racing_receiver_loss_keeps_its_receipt_after_transport_closure() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let mut shared = fixture.shared.lock().await;
    // The connection has dropped its response future, but closed() has not yet
    // acquired the shared lock to record that transport outcome.
    drop(shared.receive(id("load")).unwrap());
    let decision = release(&fixture.record, "send", "load", 200, "[0]");
    let reply = shared.control(decision.clone()).unwrap();
    let Reply::Released { receipt } = &reply else {
        panic!()
    };
    assert_eq!(receipt.state, ReleaseState::ReceiverGone);
    assert_eq!(
        shared.record.requests[&id("load")].transport,
        Transport::Open
    );
    shared.closed(&id("load"), Transport::Failed).unwrap();
    assert_eq!(shared.control(decision).unwrap(), reply);
    assert!(
        shared
            .control(release(&fixture.record, "replacement", "load", 200, "[1]"))
            .is_err()
    );
    let saved: Record = serde_json::from_slice(&std::fs::read(&shared.path).unwrap()).unwrap();
    assert_eq!(saved.requests[&id("load")].transport, Transport::Failed);
    assert_eq!(saved.requests[&id("load")].release, Some(id("send")));
    assert_eq!(saved.releases[&id("send")], *receipt);
}

#[tokio::test]
async fn prepared_release_is_uncertain_and_cannot_be_reissued_with_a_new_identity() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let mut shared = fixture.shared.lock().await;
    let mut receiver = shared.receive(id("load")).unwrap();
    let decision = release(&fixture.record, "send", "load", 200, "[]");
    let mut prepared = shared.record.clone();
    prepared.requests.get_mut(&id("load")).unwrap().release = Some(id("send"));
    prepared.releases.insert(
        id("send"),
        ReleaseReceipt {
            command: decision.clone(),
            state: ReleaseState::Prepared,
        },
    );
    shared.commit(prepared).unwrap();
    assert!(matches!(
        shared.control(decision.clone()).unwrap(),
        Reply::Released {
            receipt: ReleaseReceipt {
                state: ReleaseState::Prepared,
                ..
            }
        }
    ));
    assert!(
        shared
            .control(release(&fixture.record, "replacement", "load", 200, "[]"))
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
    shared.closed(&id("load"), Transport::Failed).unwrap();
    // Closure cannot turn a previously uncertain handoff into known non-delivery.
    assert!(matches!(
        shared.control(decision).unwrap(),
        Reply::Released {
            receipt: ReleaseReceipt {
                state: ReleaseState::Prepared,
                ..
            }
        }
    ));
    assert!(
        shared
            .control(release(&fixture.record, "after-close", "load", 200, "[]"))
            .is_err()
    );
}

#[tokio::test]
async fn persistence_failure_fences_all_later_release_effects() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let mut shared = fixture.shared.lock().await;
    let mut receiver = shared.receive(id("load")).unwrap();
    shared.path = root.path().join("missing/state.json");
    assert!(
        shared
            .control(release(&fixture.record, "send", "load", 200, "[]"))
            .is_err()
    );
    assert!(shared.poisoned);
    shared.path = root.path().join("run/state.json");
    assert!(
        shared
            .control(release(&fixture.record, "retry", "load", 200, "[]"))
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn stop_retires_waiting_requests_without_releasing_a_business_response() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("run");
    let fixture = Fixture::create(directory.clone()).await.unwrap();
    let record = fixture.record.clone();
    let server = tokio::spawn(fixture.run());
    let address = record.address;
    let path = format!("/{}/requests/held", record.generation);
    let held = tokio::spawn(async move { http(address, "GET", &path, "").await });
    wait_received(&record, "held").await;
    command(
        &record,
        &Control::Stop {
            generation: record.generation.clone(),
        },
    )
    .await;
    assert_eq!(held.await.unwrap().0, 503);
    server.await.unwrap().unwrap();
    let saved: Record =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert!(saved.releases.is_empty());
    assert_eq!(saved.requests[&id("held")].transport, Transport::Stopped);
}

#[tokio::test]
async fn owned_service_fences_owner_and_shutdown_retires_pending_http() {
    use crate::owned::{Binding, OwnedFixture};
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owned");
    let owner = LaunchId::new("mobile-one").unwrap();
    let service = OwnedFixture::start(directory.clone(), owner.clone())
        .await
        .unwrap();
    let record = service.initial().clone();
    let binding: Binding =
        serde_json::from_slice(&std::fs::read(directory.join("owner.json")).unwrap()).unwrap();
    assert_eq!(binding.owner, owner);
    assert_eq!(binding.service, record.generation);
    let path = format!("/{}/requests/load", record.generation);
    let pending = tokio::spawn(async move { http(record.address, "GET", &path, "").await });
    wait_received(service.initial(), "load").await;
    let decision = release(service.initial(), "send-load", "load", 200, "[]");
    assert!(
        service
            .control(&LaunchId::new("mobile-two").unwrap(), decision)
            .await
            .is_err()
    );
    let Reply::Snapshot { record } = service
        .control(
            &owner,
            Control::Inspect {
                generation: service.binding().service.clone(),
            },
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(record.releases.is_empty());
    service.shutdown().await.unwrap();
    assert_eq!(pending.await.unwrap().0, 503);
    let final_record: Record =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert!(final_record.stopping);
    assert_eq!(
        final_record.requests[&id("load")].transport,
        Transport::Stopped
    );
    assert!(
        tokio::net::TcpStream::connect(record.address)
            .await
            .is_err()
    );
    assert!(OwnedFixture::start(directory, owner).await.is_err());
}

#[tokio::test]
async fn dropped_owner_closes_http_without_manufacturing_completion() {
    use crate::owned::OwnedFixture;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owned");
    let service = OwnedFixture::start(directory.clone(), LaunchId::new("mobile-one").unwrap())
        .await
        .unwrap();
    let record = service.initial().clone();
    let mut stream = tokio::net::TcpStream::connect(record.address)
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /{}/requests/load HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                record.generation
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    wait_received(&record, "load").await;
    drop(service);
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
    assert!(
        tokio::net::TcpStream::connect(record.address)
            .await
            .is_err()
    );
    let retained: Record =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert!(!retained.stopping);
    assert!(retained.releases.is_empty());
    assert_eq!(retained.requests[&id("load")].transport, Transport::Open);
}

#[tokio::test]
async fn observing_service_exit_is_cancel_safe_and_repeatable() {
    use crate::owned::OwnedFixture;
    let root = tempfile::tempdir().unwrap();
    let owner = LaunchId::new("mobile-one").unwrap();
    let mut service = OwnedFixture::start(root.path().join("owned"), owner.clone())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), service.wait_for_exit())
            .await
            .is_err()
    );
    let generation = service.binding().service.clone();
    assert!(matches!(
        service
            .control(
                &owner,
                Control::Inspect {
                    generation: generation.clone()
                }
            )
            .await
            .unwrap(),
        Reply::Snapshot { .. }
    ));
    service
        .control(&owner, Control::Stop { generation })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), service.wait_for_exit())
        .await
        .unwrap()
        .unwrap();
    service.wait_for_exit().await.unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn poisoned_owner_exits_and_repeated_waits_preserve_failure() {
    use crate::owned::OwnedFixture;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owned");
    let owner = LaunchId::new("mobile-one").unwrap();
    let mut service = OwnedFixture::start(directory.clone(), owner.clone())
        .await
        .unwrap();
    let generation = service.binding().service.clone();
    // An unwritable state destination reproduces persistence loss without a platform permission assumption.
    std::fs::rename(
        directory.join("state.json"),
        directory.join("original.json"),
    )
    .unwrap();
    std::fs::create_dir(directory.join("state.json")).unwrap();
    assert!(
        service
            .control(&owner, Control::Stop { generation })
            .await
            .is_err()
    );
    let first = tokio::time::timeout(Duration::from_secs(3), service.wait_for_exit())
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(first.contains("uncertain"));
    assert_eq!(
        service.wait_for_exit().await.unwrap_err().to_string(),
        first
    );
    assert!(
        tokio::net::TcpStream::connect(service.initial().address)
            .await
            .is_err()
    );
    assert_eq!(service.shutdown().await.unwrap_err().to_string(), first);
}

#[tokio::test]
async fn retained_responses_are_bounded_without_changing_exact_retry() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::create(root.path().join("run")).await.unwrap();
    let mut shared = fixture.shared.lock().await;
    let body = "\u{0001}".repeat(MAX_BODY);
    let mut receivers = Vec::new();
    for i in 0..16 {
        let name = format!("request-{i}");
        receivers.push(shared.receive(id(&name)).unwrap());
        shared
            .control(release(
                &fixture.record,
                &format!("send-{i}"),
                &name,
                200,
                &body,
            ))
            .unwrap();
    }
    let retry = release(&fixture.record, "send-0", "request-0", 200, &body);
    assert!(matches!(
        shared.control(retry).unwrap(),
        Reply::Released { .. }
    ));
    receivers.push(shared.receive(id("overflow")).unwrap());
    assert!(
        shared
            .control(release(
                &fixture.record,
                "send-overflow",
                "overflow",
                200,
                "x"
            ))
            .is_err()
    );
    assert_eq!(shared.record.requests[&id("overflow")].release, None);
    assert_eq!(shared.record.releases.len(), 16);
    // Includes worst-case six-byte JSON escaping; leave room for recorder result framing.
    assert!(serde_json::to_vec(&shared.record).unwrap().len() < 8 << 20);
}

#[tokio::test]
async fn assets_hold_frozen_binary_bytes_and_distinguish_physical_attempts() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("engine.wasm");
    let original = [0, 97, 115, 109, 255, 0, 128];
    std::fs::write(&source, original).unwrap();
    let fixture = Fixture::create_with_assets(
        root.path().join("run"),
        vec![assets::AssetInput {
            path: "/engine.wasm".into(),
            source: source.clone(),
            content_type: "application/wasm".into(),
            content_security_policy: Some("default-src 'self'".into()),
        }],
    )
    .await
    .unwrap();
    let record = fixture.record.clone();
    let shared = fixture.shared.clone();
    let server = tokio::spawn(fixture.run());
    // Source edits after preparation cannot change the bytes served by this generation.
    std::fs::write(&source, b"changed").unwrap();
    let mut requests = Vec::new();
    for _ in 0..2 {
        let mut stream = tokio::net::TcpStream::connect(record.address)
            .await
            .unwrap();
        stream
            .write_all(
                b"GET /engine.wasm?v=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        requests.push(tokio::spawn(async move {
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            bytes
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while shared.lock().await.record.requests.len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut requests[0])
            .await
            .is_err()
    );
    assert!(!requests[1].is_finished());
    let snapshot = shared.lock().await.record.clone();
    let mut receipts = Vec::new();
    for (request, received) in &snapshot.requests {
        let resource = received.resource.as_ref().unwrap();
        assert_eq!(resource.uri, "/engine.wasm?v=1");
        assert_eq!(resource.asset.size, original.len() as u64);
        let decision = Control::ReleaseAsset {
            generation: record.generation.clone(),
            id: request.clone(),
            request: request.clone(),
            sha256: resource.asset.sha256.clone(),
        };
        let mut wrong = decision.clone();
        if let Control::ReleaseAsset { sha256, .. } = &mut wrong {
            *sha256 = "wrong".into();
        }
        assert!(shared.lock().await.control(wrong).is_err());
        assert!(
            shared
                .lock()
                .await
                .control(release(&record, "json", request.as_str(), 200, "{}"))
                .is_err()
        );
        let first = command(&record, &decision).await;
        assert_eq!(command(&record, &decision).await, first);
        receipts.push(first);
    }
    assert_eq!(receipts.len(), 2);
    for pending in requests {
        let response = pending.await.unwrap();
        let boundary = response
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let headers = String::from_utf8(response[..boundary].to_vec()).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200"));
        assert!(headers.contains("content-type: application/wasm"));
        assert!(headers.contains("cache-control: no-store"));
        assert!(headers.contains("content-security-policy: default-src 'self'"));
        assert_eq!(&response[boundary + 4..], &original);
    }
    assert_eq!(http(record.address, "GET", "/missing", "").await.0, 404);
    command(
        &record,
        &Control::Stop {
            generation: record.generation.clone(),
        },
    )
    .await;
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn asset_preparation_rejects_ambiguous_paths_and_header_injection() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::write(&source, "ok").unwrap();
    for (index, (path, content_type, policy)) in [
        ("/control", "text/html", None),
        ("/asset?x=1", "text/html", None),
        ("/asset", "text/html\r\nx-injected: yes", None),
        (
            "/asset",
            "text/html",
            Some("default-src 'self'\r\nx-injected: yes"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            Fixture::create_with_assets(
                root.path().join(format!("run-{index}")),
                vec![assets::AssetInput {
                    path: path.into(),
                    source: source.clone(),
                    content_type: content_type.into(),
                    content_security_policy: policy.map(str::to_owned),
                }]
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn owned_assets_fence_release_and_retire_unreleased_resources() {
    use crate::owned::OwnedFixture;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("app.js");
    std::fs::write(&source, "export const value = 1;").unwrap();
    let directory = root.path().join("owned-assets");
    let owner = LaunchId::new("mobile-assets").unwrap();
    let service = OwnedFixture::start_with_assets(
        directory.clone(),
        owner.clone(),
        vec![assets::AssetInput {
            path: "/app.js".into(),
            source,
            content_type: "text/javascript".into(),
            content_security_policy: Some("default-src 'self'".into()),
        }],
    )
    .await
    .unwrap();
    let record = service.initial().clone();
    let address = record.address;
    let released = tokio::spawn(async move { http(address, "GET", "/app.js?released", "").await });
    let held = tokio::spawn(async move { http(address, "GET", "/app.js?held", "").await });
    let inspect = Control::Inspect {
        generation: record.generation.clone(),
    };
    let snapshot = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Reply::Snapshot { record } =
                service.control(&owner, inspect.clone()).await.unwrap()
            else {
                panic!()
            };
            if record.requests.len() == 2 {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (request, row) = snapshot
        .requests
        .iter()
        .find(|(_, row)| row.resource.as_ref().unwrap().uri.ends_with("?released"))
        .unwrap();
    let decision = Control::ReleaseAsset {
        generation: record.generation.clone(),
        id: id("release-script"),
        request: request.clone(),
        sha256: row.resource.as_ref().unwrap().asset.sha256.clone(),
    };
    assert!(
        service
            .control(&LaunchId::new("other-owner").unwrap(), decision.clone())
            .await
            .is_err()
    );
    assert!(!released.is_finished());
    assert!(!held.is_finished());
    let receipt = service.control(&owner, decision.clone()).await.unwrap();
    assert_eq!(service.control(&owner, decision).await.unwrap(), receipt);
    assert_eq!(
        released.await.unwrap(),
        (200, "export const value = 1;".into())
    );
    assert!(!held.is_finished());
    let controller = service.controller();
    service.shutdown().await.unwrap();
    assert_eq!(held.await.unwrap().0, 503);
    let saved: Record =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    let waiting = saved
        .requests
        .values()
        .find(|row| row.resource.as_ref().unwrap().uri.ends_with("?held"))
        .unwrap();
    assert!(saved.stopping);
    assert_eq!(waiting.transport, Transport::Stopped);
    assert!(waiting.release.is_none());
    assert_eq!(saved.releases.len(), 1);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    assert!(matches!(
        controller.exchange(&owner, inspect).await,
        Reply::Uncertain { .. }
    ));
}

#[tokio::test]
async fn shared_allowance_reserves_release_pair_and_preserves_stop_after_exhaustion() {
    use frontcap_core::write_budget::{Limits, WriteBudget};
    let root = tempfile::tempdir().unwrap();
    let limits = Limits {
        ordinary: 32768,
        retirement: 16384,
    };
    let budget =
        WriteBudget::create(&root.path().join("budget"), LaunchId::generate(), limits).unwrap();
    let fixture = Fixture::create_budgeted(
        root.path().join("service"),
        Vec::new(),
        None,
        Some(budget.clone()),
    )
    .await
    .unwrap();
    let mut shared = fixture.shared.lock().await;
    let mut receiver = shared.receive(id("first")).unwrap();
    let command = release(&shared.record, "release", "first", 200, "hello");
    let reply = shared.control(command.clone()).unwrap();
    assert!(matches!(reply, Reply::Released { .. }));
    assert_eq!(
        receiver.try_recv().unwrap().body,
        Bytes::from_static(b"hello")
    );
    let mut waiting = shared.receive(id("waiting")).unwrap();
    let refused = release(&shared.record, "refused", "waiting", 200, "no handoff");
    let mut prepared = shared.record.clone();
    prepared.releases.insert(
        id("refused"),
        ReleaseReceipt {
            command: refused.clone(),
            state: ReleaseState::Prepared,
        },
    );
    prepared.requests.get_mut(&id("waiting")).unwrap().release = Some(id("refused"));
    // Enough for the prepared state alone, but not its post-handoff result.
    let remaining = record_bytes(&prepared);
    budget
        .charge(
            Class::Ordinary,
            limits.ordinary - budget.charged().unwrap().ordinary - remaining,
        )
        .unwrap();
    let before = std::fs::read(&shared.path).unwrap();
    assert!(shared.control(refused).is_err());
    assert!(!shared.poisoned);
    assert!(matches!(
        waiting.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(std::fs::read(&shared.path).unwrap(), before);
    budget.charge(Class::Ordinary, remaining).unwrap();
    assert!(shared.receive(id("over-budget")).is_err());
    assert!(!shared.poisoned);
    assert_eq!(
        serde_json::to_value(shared.control(command).unwrap()).unwrap(),
        serde_json::to_value(reply).unwrap()
    );
    shared.closed(&id("first"), Transport::Closed).unwrap();
    let generation = shared.record.generation.clone();
    assert!(matches!(
        shared
            .control(Control::Inspect {
                generation: generation.clone()
            })
            .unwrap(),
        Reply::Snapshot { .. }
    ));
    shared
        .control(Control::Stop {
            generation: generation.clone(),
        })
        .unwrap();
    assert!(matches!(
        waiting.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    let spent = budget.charged().unwrap();
    shared.control(Control::Stop { generation }).unwrap();
    shared.closed(&id("waiting"), Transport::Closed).unwrap();
    assert_eq!(budget.charged().unwrap(), spent);
    let retained: Record = frontcap_core::state::load(&shared.path).unwrap().unwrap();
    assert!(retained.stopping);
    assert_eq!(retained.requests[&id("first")].transport, Transport::Closed);
    assert!(!retained.releases.contains_key(&id("refused")));
}

#[tokio::test]
async fn asset_acquisition_refuses_unadmitted_blob_before_binding_service() {
    use frontcap_core::write_budget::{Limits, WriteBudget};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::write(&source, vec![42; 8192]).unwrap();
    let budget = WriteBudget::create(
        &root.path().join("budget"),
        LaunchId::generate(),
        Limits {
            ordinary: 4096,
            retirement: 4096,
        },
    )
    .unwrap();
    let directory = root.path().join("service");
    let result = Fixture::create_budgeted(
        directory.clone(),
        vec![assets::AssetInput {
            path: "/content".into(),
            source: source.clone(),
            content_type: "application/octet-stream".into(),
            content_security_policy: None,
        }],
        None,
        Some(budget.clone()),
    )
    .await;
    assert!(result.is_err());
    assert!(!directory.join("state.json").exists());
    assert_eq!(
        std::fs::read_dir(directory.join("assets")).unwrap().count(),
        0
    );
    assert_eq!(std::fs::read(source).unwrap(), vec![42; 8192]);
    assert!(budget.charged().unwrap().ordinary > 0); // Retained input manifest is charged.
    assert_eq!(budget.charged().unwrap().retirement, 0);
}
