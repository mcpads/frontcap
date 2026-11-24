//! Cancelling a running request (`Call::Cancel`): a wait stops at the frame tick after the request was
//! seen, a predicate wait removes its watch, a probe runs no later step, and the run replays equal with
//! each cancellation at the same instant.
mod support;

use frontcap_core::clock::{frame_slot, frame_tick};
use frontcap_core::event::ClockOutcome;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;
use frontcap_recorder::launcher::{self, Attached, Workspace};
use frontcap_recorder::replay::{ReplayOptions, replay};
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use frontcap_recorder_protocol::{Answer, Call, ErrorCode, ObserveOp, Op, Outcome, ProbeStep, Reply};

/// A page that changes every 10 ms, so a long wait renders every frame and takes wall time.
const PAGE: &str = "<!doctype html><output id=count>0</output><script>let n = 0; setInterval(() => { count.textContent = ++n; }, 10);</script>";

const LONG_US: u64 = 600_000_000;

fn rules() -> RuleSet {
    RuleSet::from_json(&serde_json::json!([
        {"id": "document", "match": {"url": "*busy.html"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": PAGE}}
    ]).to_string()).unwrap()
}

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

/// Another connection to `a`'s recorder, as a second controller call arrives.
async fn other(ws: &Workspace, a: &Attached) -> RecorderClient {
    let socket = frontcap_recorder::runtime::socket_path(&ws.runtime_dir(&a.launch_id));
    RecorderClient::connect(&socket).await.unwrap().0
}

/// Sends `op` as `id` and, once it has run for 200 ms, cancels it from another connection.
async fn cancelled(a: &mut Attached, other: &mut RecorderClient, id: &str, op: Op) -> Reply {
    let lease = a.lease.clone();
    let running = a.client.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid(id), op });
    let cancel = async {
        loop {
            let Ok(Reply::Status { status }) = other.call(Call::Status).await else { panic!("no status") };
            if status.running.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        other.call(Call::Cancel { lease: Some(lease), request_id: rid(id), at_us: None }).await
    };
    let (reply, cancel) = tokio::join!(running, cancel);
    assert!(matches!(cancel, Ok(Reply::Cancelling { .. })), "{cancel:?}");
    reply.unwrap()
}

fn on_a_frame_tick(t: frontcap_core::time::VirtualTime) -> bool {
    let frame = VirtualDuration(support::profile().frame_interval_us);
    frame_tick(frame_slot(t, frame), frame) == t
}

#[tokio::test]
async fn cancelled_waits_stop_at_a_frame_tick_and_replay_there() {
    let (_tmp, ws) = support::workspace();
    let request = launcher::LaunchRequest { rules: rules(), passthrough_origins: Some(vec![]), ..support::launch_request("busy", "https://app.example.test/busy.html") };
    let mut a = launcher::launch(&ws, request, &support::recorder_bin(), &[]).await.unwrap();
    let mut b = other(&ws, &a).await;

    // A long advance stops early, on a frame tick.
    let Reply::Done { outcome: Outcome::Clock { result, start_us, t_us, .. }, .. } = cancelled(&mut a, &mut b, "long", Op::Advance { by_us: LONG_US }).await else {
        panic!("not a clock outcome")
    };
    assert_eq!(result, ClockOutcome::Cancelled);
    assert!(t_us.0 < start_us.0 + LONG_US && t_us > start_us, "{start_us:?} → {t_us:?}");
    assert!(on_a_frame_tick(t_us), "{t_us:?}");

    // A predicate wait stops the same way and removes the watch it registered.
    let never = Predicate::TextEquals { target: Locator::Css { selector: "#count".into() }, text: "never".into() };
    let Reply::Done { outcome: Outcome::Clock { result, .. }, .. } = cancelled(&mut a, &mut b, "never", Op::UntilPredicate { predicate: never, max_us: LONG_US, context: None }).await else {
        panic!("not a clock outcome")
    };
    assert_eq!(result, ClockOutcome::Cancelled);
    let Ok(Reply::Status { status }) = b.call(Call::Status).await else { panic!("no status") };
    assert!(status.watches.is_empty(), "{:?}", status.watches);

    // A probe whose wait is cancelled runs no later step.
    let probe = Op::Probe { steps: vec![
        ProbeStep::Mutate { op: Op::Advance { by_us: LONG_US } },
        ProbeStep::Observe { op: ObserveOp::Query { target: Locator::Css { selector: "#count".into() }, props: vec!["text".into()] }, context: None },
    ] };
    let Reply::Done { outcome: Outcome::Probed { results, completed, cancelled: true, end, interrupted: false }, .. } = cancelled(&mut a, &mut b, "probe", probe).await else {
        panic!("not a cancelled probe")
    };
    assert_eq!((completed, results.len()), (1, 1), "{results:?}");
    let Answer::Ok(Reply::Done { outcome: Outcome::Clock { result: ClockOutcome::Cancelled, t_us, .. }, .. }) = &results[0] else { panic!("{results:?}") };
    assert_eq!(end.map(|b| b.t_us), Some(*t_us));

    // Only a running request is cancelled, and only by the lease holder.
    let ended = b.call(Call::Cancel { lease: Some(a.lease.clone()), request_id: rid("long"), at_us: None }).await;
    assert!(matches!(&ended, Err(ClientError::Recorder(e)) if e.code == ErrorCode::NotFound), "{ended:?}");
    let foreign = b.call(Call::Cancel { lease: Some(frontcap_recorder_protocol::LeaseToken::generate()), request_id: rid("long"), at_us: None }).await;
    assert!(matches!(&foreign, Err(ClientError::Recorder(e)) if e.code == ErrorCode::LeaseRequired), "{foreign:?}");

    // Each cancelled request keeps where it stopped, and a replay stops it there.
    assert!(matches!(a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: rid("stop"), op: Op::Stop }).await, Ok(Reply::Done { .. })));
    support::wait_until_exited(&ws.runtime_dir(&a.launch_id)).await;
    let source = ws.runs().join(launcher::run_id_of(&a.launch_id).as_str());
    let commands = frontcap_core::inputs::read_commands(&source).unwrap();
    let cancelled_at: Vec<_> = commands.iter().filter_map(|c| c.cancelled_at_us.map(|t| (c.request_id.as_str().to_string(), t))).collect();
    assert_eq!(cancelled_at.len(), 3, "{cancelled_at:?}");
    assert!(cancelled_at.iter().all(|(_, t)| on_a_frame_tick(*t)));
    let options = ReplayOptions { url: None, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![] };
    let report = replay(&ws, &source, options).await.unwrap();
    assert!(report.equal, "{report:#?}");
}
