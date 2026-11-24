//! Acquire and retire a fresh owned Simulator; does not install or run an app.
use frontcap_native::simulator::{Simulator, SimulatorLaunch};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let config: SimulatorLaunch =
        serde_json::from_slice(&std::fs::read(args.next().ok_or("missing launch JSON")?)?)?;
    let directory = PathBuf::from(args.next().ok_or("missing evidence directory")?);
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let mut owner = Simulator::acquire(
        config,
        &directory,
        Instant::now() + Duration::from_secs(180),
        &AtomicBool::new(false),
    )?;
    println!("Acquired {}", owner.udid());
    let retirement = owner.stop();
    println!("{}", serde_json::to_string(&retirement)?);
    if !retirement.passed {
        return Err("Simulator retirement unconfirmed".into());
    }
    Ok(())
}
