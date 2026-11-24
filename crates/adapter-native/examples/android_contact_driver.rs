//! Qualify external producer acquisition/retirement; does not issue native contact input.
use frontcap_native::device::{AndroidLaunch, Device};
use frontcap_core::state::save_atomic;
use std::{path::Path, sync::atomic::AtomicBool, time::{Duration, Instant}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 { return Err("expected config, fresh evidence directory, driver JAR and SHA256".into()); }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let root = Path::new(&args[1]);
    let cancelled = AtomicBool::new(false);
    let mut device = Device::acquire(config, root, Instant::now()+Duration::from_secs(240), &cancelled)?;
    let work = device.start_contact_driver(Path::new(&args[2]), args[3].to_str().ok_or("invalid driver hash")?,
        Instant::now()+Duration::from_secs(60), &cancelled);
    let retirement = device.stop();
    save_atomic(&root.join("contact-acquisition-result.json"),
        &(work.as_ref().map_err(ToString::to_string), &retirement))?;
    work?;
    if !retirement.passed { return Err("contact target retirement failed".into()); }
    Ok(())
}
