use super::*;
use crate::process::exchange_typed_recorded;
use frontcap_core::{
    canonical::sha256_bytes_hex,
    native::{ios::ClockStatus, request::ClockOperation},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[test]
fn system_ui_reply_needs_its_runner_but_not_a_consumer_clock_identity() {
    use frontcap_core::native::ios_ui::{UiOperation, UiOutcome};
    let runner = UiIdentity { simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(), pid: 123 };
    let observation = "abcd0000-0000-0000-0000-000000000003".to_owned();
    let reference = format!("{observation}-0");
    let request = UiRequest { generation: runner.generation.clone(), request_id: 2,
        operation: UiOperation::TapSystemButton { reference: reference.clone(), observation: observation.clone() } };
    let reply = UiReply::Completed { completion: Box::new(UiCompletion {
        request: request.clone(), generation: runner.generation.clone(), pid: runner.pid, last_id: 2,
        outcome: UiOutcome::SystemButtonTapped { reference, observation }, receipt_sha256: "a".repeat(64),
    }) };
    let call = UiCall::Execute { identity: runner.clone(), app: None, request: request.clone() };
    reply.validate(&call).unwrap();
    let mut wrong_runner = runner.clone(); wrong_runner.pid += 1;
    assert!(reply.validate(&UiCall::Recover { identity: wrong_runner, app: None, request: request.clone() }).is_err());
    let app_operation = UiRequest { operation: UiOperation::ObserveTargets {}, ..request };
    assert!(reply.validate(&UiCall::Execute { identity: runner, app: None, request: app_operation }).is_err());
}

#[test]
fn ui_reservation_and_release_acknowledgements_bind_both_channels_and_proof() {
    use frontcap_core::native::ios_ui::UiOperation;
    let app=ClockIdentity { simulator:"abcd0000-0000-0000-0000-000000000001".into(),
        generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:123 };
    let runner=UiIdentity { simulator:app.simulator.clone(),
        generation:"abcd0000-0000-0000-0000-000000000003".into(),pid:124 };
    let clock=ClockRequest { generation:app.generation.clone(),request_id:1,
        operation:ClockOperation::Advance,delta_ns:70_000_000 };
    let observation="abcd0000-0000-0000-0000-000000000004".to_owned();
    let ui=UiRequest { generation:runner.generation.clone(),request_id:3,
        operation:UiOperation::PressTarget { reference:format!("{observation}-0"),
            observation,clock_request:clock.clone() } };
    let reserve=ClockCall::ReserveUi { identity:app.clone(),request:clock.clone(),
        ui_identity:runner.clone(),ui_request:ui.clone() };
    let reserved=ClockReply::UiReserved { identity:app.clone(),request:clock.clone(),
        ui_identity:runner.clone(),ui_request:ui.clone() };
    reserved.validate(&reserve).unwrap();
    let release=ClockCall::ReleaseUi { identity:app.clone(),request:clock.clone(),
        ui_identity:runner.clone(),ui_request:ui.clone(),receipt_sha256:"a".repeat(64) };
    let mut released=ClockReply::UiReleased { identity:app.clone(),request:clock.clone(),
        ui_identity:runner,ui_request:ui,receipt_sha256:"a".repeat(64) };
    released.validate(&release).unwrap();
    assert!(released.validate(&reserve).is_err());
    assert!(reserved.validate(&release).is_err());
    assert!(released.validate(&ClockCall::Execute { identity:app,request:clock }).is_err());
    if let ClockReply::UiReleased { ui_request, .. } = &mut released { ui_request.request_id+=1; }
    assert!(released.validate(&release).is_err());
    if let ClockReply::UiReleased { ui_request, receipt_sha256, .. } = &mut released {
        ui_request.request_id-=1;*receipt_sha256="b".repeat(64);
    }
    assert!(released.validate(&release).is_err());
}

fn invoke(root: &Path, descriptor: &Path, call: &ClockCall, name: &str) -> ClockReply {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/native_ios_clock_bridge.py");
    let mut command = Command::new("python3");
    command
        .arg(script)
        .arg("--descriptor")
        .arg(descriptor)
        .arg("--directory")
        .arg(root.join("bridge"));
    let result = exchange_typed_recorded::<_, ClockReply>(
        &mut command,
        call,
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
        65536,
        &root.join(name),
    )
    .unwrap();
    assert!(
        result.failure.is_none() && result.cleanup_failure.is_none(),
        "{result:?}"
    );
    let reply = result.reply.unwrap();
    reply.validate(call).unwrap();
    reply
}

#[test]
fn supervised_ios_bridge_preserves_clock_outcomes_and_reuses_receipt_after_endpoint_loss() {
    for (advance, drain) in [(0, Some(0)), (60, None), (0, Some(60))] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let identity = ClockIdentity {
            simulator: "ABCD0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 123,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let descriptor = root.join("endpoint.json");
        frontcap_core::state::save_atomic(
            &descriptor,
            &serde_json::json!({
                "generation":identity.generation,"pid":identity.pid,"status":"ready","last_id":0,
                "port":listener.local_addr().unwrap().port(),
            }),
        )
        .unwrap();
        invoke(
            root,
            &descriptor,
            &ClockCall::Initialize {
                identity: identity.clone(),
                source_sha256: "a".repeat(64),
            },
            "initialize",
        );
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "bridge never connected");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Accepted socket modes differ by platform; read_exact below is a bounded blocking read.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = [0; 4];
            socket.read_exact(&mut header).unwrap();
            let size = u32::from_be_bytes(header) as usize;
            assert!(size < 65536);
            let mut body = vec![0; size];
            socket.read_exact(&mut body).unwrap();
            let request: ClockRequest = serde_json::from_slice(&body).unwrap();
            let applied = advance == 0;
            let response=serde_json::to_vec(&serde_json::json!({
                "request":request,"generation":request.generation,"pid":123,"last_id":request.request_id,
                "status":if applied && drain==Some(0) { "ok" } else { "failed" },
                "before_ns":0,"now_ns":if applied { request.delta_ns } else { 0 },
                "applied":applied,"advance_result":advance,"drain_result":drain,"replayed":false,
            })).unwrap();
            socket
                .write_all(&(response.len() as u32).to_be_bytes())
                .unwrap();
            socket.write_all(&response).unwrap();
            request
        });
        let request = ClockRequest {
            generation: identity.generation.clone(),
            request_id: 1,
            operation: ClockOperation::Advance,
            delta_ns: 1000,
        };
        let call = ClockCall::Execute {
            identity,
            request: request.clone(),
        };
        let reply = invoke(root, &descriptor, &call, "execute");
        assert_eq!(server.join().unwrap(), request);
        let ClockReply::Completed { completion } = reply else {
            panic!()
        };
        assert_eq!(completion.applied, advance == 0);
        assert_eq!(
            completion.status == ClockStatus::Ok,
            advance == 0 && drain == Some(0)
        );
        let raw = std::fs::read(
            root.join("bridge/receipts")
                .join(format!("{}.json", completion.receipt_sha256)),
        )
        .unwrap();
        assert_eq!(sha256_bytes_hex(&raw), completion.receipt_sha256);
        std::fs::remove_file(&descriptor).unwrap();
        let ClockReply::Completed {
            completion: recovered,
        } = invoke(root, &descriptor, &call, "repeat")
        else {
            panic!()
        };
        assert_eq!(recovered, completion);
    }
}


#[test]
fn external_clock_reservation_survives_bridge_exit_without_transport() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let identity = ClockIdentity {
        simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 123,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let descriptor = root.join("endpoint.json");
    frontcap_core::state::save_atomic(&descriptor, &serde_json::json!({
        "generation":identity.generation,"pid":identity.pid,"status":"ready","last_id":0,
        "port":listener.local_addr().unwrap().port(),
    })).unwrap();
    invoke(root, &descriptor, &ClockCall::Initialize {
        identity: identity.clone(), source_sha256: "a".repeat(64),
    }, "initialize");
    let request = ClockRequest {
        generation: identity.generation.clone(), request_id: 1,
        operation: ClockOperation::Advance, delta_ns: 70_000_000,
    };
    let call = ClockCall::Reserve { identity: identity.clone(), request: request.clone() };
    let reply = invoke(root, &descriptor, &call, "reserve");
    assert!(matches!(reply, ClockReply::Reserved { .. }));
    assert!(reply.validate(&ClockCall::Execute { identity: identity.clone(), request: request.clone() }).is_err());
    invoke(root, &descriptor, &call, "reserve-again");
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    // This models the separate UI delivery's published receipt, not a socket reply to the bridge.
    frontcap_core::state::save_atomic(&descriptor, &serde_json::json!({
        "generation":identity.generation,"pid":identity.pid,"status":"ok","last_id":1,
        "request":request,"before_ns":0,"now_ns":70_000_000,"applied":true,
        "advance_result":0,"drain_result":0,"replayed":false,
    })).unwrap();
    let recovered = invoke(root, &descriptor, &ClockCall::Recover {
        identity: identity.clone(), request: request.clone(),
    }, "recover");
    let ClockReply::Completed { completion } = recovered else { panic!() };
    assert_eq!(completion.now_ns, 70_000_000);
    std::fs::remove_file(&descriptor).unwrap();
    let saved = invoke(root, &descriptor, &ClockCall::Recover { identity, request }, "saved");
    let ClockReply::Completed { completion: again } = saved else { panic!() };
    assert_eq!(again, completion);
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
