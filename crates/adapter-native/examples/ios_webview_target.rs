//! Installed acquisition/control/retirement probe; no recorder Ready or input capability claim.
use frontcap_core::{ids::RequestId, state::save_atomic};
use frontcap_native::{
    request::ClockOperation,
    simulator::{Simulator, SimulatorLaunch},
    simulator_app::{App, Launch},
    webview_target::Target,
};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    simulator: SimulatorLaunch,
    app: Launch,
    startup_steps: u32,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("expected config and new evidence directory".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.startup_steps > 600 {
        return Err("startup budget exceeds 600 steps".into());
    }
    let directory = PathBuf::from(&args[1]);
    std::fs::create_dir(&directory)?;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(240);
    let simulator = Simulator::acquire(
        config.simulator,
        &directory.join("simulator"),
        deadline,
        &cancelled,
    )?;
    let app = App::launch(
        simulator,
        config.app,
        &directory.join("app"),
        deadline,
        &cancelled,
    )?;
    let mut target = Target::connect(app, &directory.join("target"), deadline, &cancelled)?;
    let work = (|| -> Result<(), Box<dyn std::error::Error>> {
        for number in 0..config.startup_steps {
            target.execute(
                RequestId::new(format!("startup-{number}"))?,
                ClockOperation::Advance,
                50_000_000,
                deadline,
                &cancelled,
            )?;
        }
        let before = target.execute(
            RequestId::new("before")?,
            ClockOperation::Read,
            0,
            deadline,
            &cancelled,
        )?;
        std::thread::sleep(Duration::from_millis(700));
        let held = target.execute(
            RequestId::new("held")?,
            ClockOperation::Read,
            0,
            deadline,
            &cancelled,
        )?;
        target.capture(RequestId::new("held")?, deadline, &cancelled)?;
        let advanced = target.execute(
            RequestId::new("advance")?,
            ClockOperation::Advance,
            100_000_000,
            deadline,
            &cancelled,
        )?;
        let replay = target.execute(
            RequestId::new("advance")?,
            ClockOperation::Advance,
            100_000_000,
            deadline,
            &cancelled,
        )?;
        let conflict = target
            .execute(
                RequestId::new("advance")?,
                ClockOperation::Advance,
                200_000_000,
                deadline,
                &cancelled,
            )
            .is_err();
        let checks = [
            before.state == held.state,
            advanced.now_ns == before.now_ns + 100_000_000,
            replay == advanced,
            conflict,
        ];
        save_atomic(&directory.join("checks.json"), &checks)?;
        if !checks.into_iter().all(|v| v) {
            return Err("WebView target checks failed".into());
        }
        Ok(())
    })();
    let retirement = target.stop();
    save_atomic(&directory.join("retirement.json"), &retirement)?;
    if !retirement.passed {
        return Err("WebView target retirement unconfirmed".into());
    }
    work
}
