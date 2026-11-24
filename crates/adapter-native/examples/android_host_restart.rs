//! Qualify ordinary Android process replacement without consumer clock/input hooks.
use frontcap_core::{ids::RequestId, native::contacts::Operation, state::save_atomic};
use frontcap_native::device::{AndroidLaunch, Device, read_host_replacement};
use std::{path::Path, sync::atomic::AtomicBool, time::{Duration, Instant}};

// Process acquisition is not window readiness. Only these passive startup observations may wait;
// input and replacement effects are never retried. Each capture attempt has a fresh identity.
fn capture_when_focused(device: &mut Device, name: &str, cancelled: &AtomicBool)
    -> Result<RequestId, Box<dyn std::error::Error>> {
    let deadline = Instant::now()+Duration::from_secs(20);
    let mut failures = Vec::new();
    loop {
        let id = RequestId::new(format!("{name}-{}", failures.len()+1))?;
        match device.capture_contacts(&id, deadline, cancelled) {
            Ok(_) => return Ok(id),
            Err(error) if Instant::now() < deadline && matches!(error.to_string().as_str(),
                "unavailable focused window" | "contact consumer is not focused") => {
                failures.push((id, error.to_string()));
                save_atomic(&device.host_evidence_directory().join(format!("focus-wait-{name}.json")), &failures)?;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 { return Err("expected host config, fresh evidence directory, driver JAR and SHA256".into()); }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.host_app.is_none() { return Err("expected host_app profile".into()); }
    let root = Path::new(&args[1]);
    let cancelled = AtomicBool::new(false);
    let mut device = Device::acquire(config, root, Instant::now()+Duration::from_secs(240), &cancelled)?;
    let work = (|| -> Result<_, Box<dyn std::error::Error>> {
        device.start_contact_driver(Path::new(&args[2]), args[3].to_str().ok_or("invalid driver hash")?,
            Instant::now()+Duration::from_secs(60), &cancelled)?;
        let mut results = Vec::new();
        for number in 1..=2 {
            let deadline = Instant::now()+Duration::from_secs(90);
            let observation = capture_when_focused(&mut device, &format!("before-{number}"), &cancelled)?;
            let previous = device.app_process().ok_or("missing owner")?.clone();
            let id = RequestId::new(format!("restart-{number}"))?;
            let result = device.restart_host(&id, &previous, None, deadline, &cancelled)?;
            if device.input_contacts(&RequestId::new(format!("stale-{number}"))?, &observation,
                &Operation::Set { expected_revision: 0, contacts: vec![] }, deadline, &cancelled).is_ok() {
                return Err("previous capture unexpectedly authorized replacement input".into());
            }
            capture_when_focused(&mut device, &format!("after-{number}"), &cancelled)?;
            if device.restart_host(&id, &previous, None, deadline, &cancelled)? != result {
                return Err("original replacement result changed".into());
            }
            results.push((id, result));
        }
        Ok(results)
    })();
    let retirement = device.stop();
    save_atomic(&root.join("host-replacement-result.json"), &(work.as_ref().map_err(ToString::to_string), &retirement))?;
    let results = work?;
    if !retirement.passed { return Err(format!("target retirement failed: {:?}", retirement.errors).into()); }
    for (id, result) in &results {
        if read_host_replacement(root, id, &result.previous.owner, None)?.as_ref() != Some(result) {
            return Err("retired replacement lookup differs".into());
        }
    }
    println!("{}", serde_json::to_string(&results)?);
    Ok(())
}
