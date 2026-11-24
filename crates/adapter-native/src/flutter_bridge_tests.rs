use super::*;
use crate::flutter_store::{Owner, Platform, Recovery};
use frontcap_core::native::flutter::{Completion, Operation};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    time::Duration,
};

fn exchange_with_server(drop_reply: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&socket);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut data = vec![0; length];
        reader.read_exact(&mut data).unwrap();
        drop(reader);
        let request: Request = serde_json::from_slice(&data).unwrap();
        request.validate().unwrap();
        if !drop_reply {
            let result = Response::Completed(Box::new(Completion {
                generation: request.generation,
                id: request.id,
                operation: request.operation.name(),
                logical_us: 0,
                held: true,
                faulted: false,
                advance: None,
                surface: None,
                capture: None,
                not_applied: None,
            }));
            let data = serde_json::to_vec(&result).unwrap();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len()).unwrap();
            socket.write_all(&data).unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let budget = frontcap_core::write_budget::WriteBudget::create(&root.path().join("budget"),
        frontcap_core::ids::LaunchId::new("g-http").unwrap(),
        frontcap_core::write_budget::Limits { ordinary: 1 << 20, retirement: 4096 }).unwrap();
    let mut store = RequestStore::create_with_budget(
        &root.path().join("store"),
        Owner {
            platform: Platform::Android,
            device: "emulator-5580".into(),
            generation: "a".repeat(48),
            source_sha256: "b".repeat(64),
        },
        Some(&budget),
    )
    .unwrap();
    let id = RequestId::new("read").unwrap();
    store.prepare(id.clone(), Operation::Read {}).unwrap();
    let mut command = Command::new("python3");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/flutter_http_bridge.py"));
    let endpoint = Endpoint {
        port,
        token: "c".repeat(48),
    };
    let result = dispatch(
        &mut store,
        &id,
        &mut command,
        endpoint.clone(),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &root.path().join("exchange"),
    )
    .unwrap();
    server.join().unwrap();
    assert!(matches!(
        store.recovery(&id).unwrap(),
        Recovery::Reconcile(_)
    ));
    assert!(matches!(
        dispatch(
            &mut store,
            &id,
            &mut command,
            endpoint,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
            &root.path().join("retry")
        ),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(!root.path().join("retry").exists());
    if drop_reply {
        assert!(result.failure.is_some());
        assert!(result.reply.is_none());
    } else {
        assert!(result.failure.is_none(), "{:?}", result.failure);
        let Response::Completed(receipt) = result.reply.unwrap().response else {
            panic!()
        };
        store.complete(&id, *receipt).unwrap();
        assert!(matches!(
            store.recovery(&id).unwrap(),
            Recovery::Completed(_)
        ));
    }
    assert!(root.path().join("exchange/call.json").is_file());
    assert!(root.path().join("exchange/exchange.json").is_file());
}
#[test]
fn typed_loopback_exchange_preserves_dispatch_and_explicit_completion() {
    exchange_with_server(false);
}
#[test]
fn lost_http_reply_stays_dispatched_without_retry() {
    exchange_with_server(true);
}
