//! Real subprocess controls for bounded I/O and ownership-based retirement.
use frontcap_native::bridge::{BridgeCall, BridgeReply};
use frontcap_native::process::{Exchange, exchange};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn run(script: &str, budget: Duration, cancel: &AtomicBool, limit: usize) -> Exchange {
    exchange(
        Command::new("python3").args(["-c", script]),
        &BridgeCall::Initialize {},
        Instant::now() + budget,
        cancel,
        limit,
    )
}
fn retired(result: &Exchange) {
    assert!(result.cleanup_failure.is_none(), "{result:?}");
    let pid = result.pid.unwrap();
    assert!(
        frontcap_core::process::ProcessIdentity::of(pid)
            .unwrap()
            .is_none()
    );
}
#[test]
fn typed_reply_requires_clean_exit_and_valid_single_message() {
    let cancel = AtomicBool::new(false);
    for (script, valid) in [
        (
            "import sys; sys.stdin.read(); print('{\"reply\":\"unresolved\",\"error\":\"pending\"}')",
            true,
        ),
        (
            "import sys; sys.stdin.read(); print('{\"reply\":\"unresolved\",\"error\":\"pending\"}'); sys.exit(3)",
            false,
        ),
        ("import sys; sys.stdin.read(); print('{}{}')", false),
    ] {
        let result = run(script, Duration::from_secs(3), &cancel, 4096);
        retired(&result);
        assert_eq!(result.failure.is_none(), valid, "{result:?}");
        assert_eq!(
            matches!(result.reply, Some(BridgeReply::Unresolved { .. })),
            valid
        );
        assert!(!result.stdout.is_empty());
    }
}
#[test]
fn cancellation_retires_live_bridge_and_pre_cancel_does_not_spawn() {
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            cancel.store(true, Ordering::Release);
        });
        let result = run(
            "import time; time.sleep(30)",
            Duration::from_secs(10),
            &cancel,
            4096,
        );
        retired(&result);
        assert_eq!(result.failure.as_deref(), Some("bridge cancelled"));
    });
    let result = run(
        "raise Exception('must not run')",
        Duration::from_secs(3),
        &cancel,
        4096,
    );
    assert!(result.pid.is_none());
}
#[test]
fn output_flood_is_bounded_without_pipe_deadlock() {
    for stream in ["stdout", "stderr"] {
        let result = run(
            &format!(
                "import sys; sys.stdin.read();\nwhile True: sys.{stream}.write('x'*8192); sys.{stream}.flush()"
            ),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            16384,
        );
        retired(&result);
        assert_eq!(
            result.failure.as_deref(),
            Some("bridge output limit exceeded")
        );
        assert!(result.stdout.len() <= 16384 && result.stderr.len() <= 16384);
    }
}
#[test]
fn exited_leader_cannot_leave_a_child_holding_output_open() {
    let result = run(
        "import subprocess,sys; sys.stdin.read(); p=subprocess.Popen(['python3','-c','import time; time.sleep(30)']); print(p.pid,flush=True)",
        Duration::from_secs(3),
        &AtomicBool::new(false),
        4096,
    );
    retired(&result);
    let pid = std::str::from_utf8(&result.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        frontcap_core::process::ProcessIdentity::of(pid)
            .unwrap()
            .is_none()
    );
    assert!(result.reply.is_none());
}

#[test]
fn recording_keeps_raw_failure_and_never_overwrites_a_prior_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let evidence = temp.path().join("attempt");
    let mut command = Command::new("python3");
    command.args([
        "-c",
        "import sys; sys.stdin.read(); print('partial'); sys.exit(7)",
    ]);
    let result = frontcap_native::process::exchange_recorded(
        &mut command,
        &BridgeCall::Initialize {},
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
        4096,
        &evidence,
    )
    .unwrap();
    retired(&result);
    assert!(result.evidence_failure.is_none());
    let recorded = std::fs::read(evidence.join("exchange.json")).unwrap();
    assert!(String::from_utf8_lossy(&recorded).contains("bridge exited unsuccessfully"));
    assert!(
        frontcap_native::process::exchange_recorded(
            &mut command,
            &BridgeCall::Initialize {},
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(false),
            4096,
            &evidence
        )
        .is_err()
    );
    assert_eq!(
        recorded,
        std::fs::read(evidence.join("exchange.json")).unwrap()
    );
}

#[test]
fn evidence_publication_failure_does_not_replace_the_process_failure() {
    let temp = tempfile::tempdir().unwrap();
    let evidence = temp.path().join("attempt");
    let mut command = Command::new("python3");
    command.args(["-c", "import pathlib,sys; sys.stdin.read(); pathlib.Path(sys.argv[1]).mkdir(); print('partial'); sys.exit(7)"])
        .arg(evidence.join("exchange.json"));
    let result = frontcap_native::process::exchange_recorded(
        &mut command,
        &BridgeCall::Initialize {},
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
        4096,
        &evidence,
    )
    .unwrap();
    retired(&result);
    assert_eq!(
        result.failure.as_deref(),
        Some("bridge exited unsuccessfully")
    );
    assert!(result.evidence_failure.is_some());
    assert_eq!(result.stdout, b"partial\n");
    assert_eq!(result.exit_code, Some(7));
    assert!(result.reply.is_none());
}
