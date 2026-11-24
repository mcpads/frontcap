//! Qualify profile-selected acquisition and repeated capture on an owned emulator.
use frontcap_core::state::save_atomic;
use frontcap_native::device::{AndroidLaunch, Device};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: android_view_app launch.json fresh-directory".into());
    }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.view_app.is_none() {
        return Err("requires a View app profile".into());
    }
    let root = PathBuf::from(&args[1]);
    let cancelled = AtomicBool::new(false);
    let mut device = Device::acquire(
        config,
        &root,
        Instant::now() + Duration::from_secs(240),
        &cancelled,
    )?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let ready = device
            .view_ready()
            .ok_or("missing acquired View owner")?
            .clone();
        std::thread::sleep(Duration::from_millis(700));
        let capture = device.collect_view(
            &root.join("held-view"),
            Instant::now() + Duration::from_secs(30),
            &cancelled,
        )?;
        if capture.now_ns != 0 {
            return Err("View app advanced during host hold".into());
        }
        save_atomic(&root.join("verified-capture.json"), &capture)?;
        println!(
            "{}: {} Views, {}x{}, held {} ns",
            ready.package,
            capture.catalog.targets.len(),
            capture.size[0],
            capture.size[1],
            capture.now_ns
        );
        Ok(())
    })();
    let retirement = device.stop();
    result?;
    if !retirement.passed {
        return Err(format!("retirement failed: {retirement:?}").into());
    }
    Ok(())
}
