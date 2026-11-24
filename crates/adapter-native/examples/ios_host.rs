//! Bounded native-host diagnostic using the same simulator/HID ownership as page targets.
use frontcap_core::{
    ids::RequestId,
    native::contacts,
    process::ProcessIdentity,
    state::{load, save_atomic},
};
use frontcap_native::ios_host::{Capture, Config, Target};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Capture {
        id: RequestId,
    },
    Input {
        id: RequestId,
        observation: RequestId,
        input: contacts::Operation,
    },
    Reconcile {
        id: RequestId,
    },
    Restart { id: RequestId, previous: frontcap_core::native::ios_host::Owner },
    Stop,
}
#[derive(Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Reply {
    Capture { capture: Capture },
    Input { receipt: contacts::Receipt },
    Restarted { previous: frontcap_core::native::ios_host::Owner, ready: frontcap_core::native::ios_host::Ready },
    Stopped,
    Failed { error: String },
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("expected config, fresh evidence directory and live guardian PID".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let root = PathBuf::from(&args[1]);
    std::fs::create_dir(&root)?;
    let guardian = ProcessIdentity::of(args[2].to_str().ok_or("invalid guardian PID")?.parse()?)?
        .ok_or("guardian absent")?;
    save_atomic(&root.join("guardian.json"), &guardian)?;
    save_atomic(&root.join("worker.json"), &ProcessIdentity::current())?;
    let cancelled = AtomicBool::new(false);
    let mut target = Target::acquire(
        config,
        &root.join("target"),
        Instant::now() + Duration::from_secs(240),
        &cancelled,
    )?;
    let work = (|| -> io::Result<()> {
        save_atomic(
            &root.join("ready.json"),
            &(target.owner(), target.contact_endpoint()),
        )?;
        let end = Instant::now() + Duration::from_secs(900);
        for n in 1..=512 {
            let path = root.join(format!("command-{n:03}.json"));
            while !path.exists() {
                if guardian.has_exited()? || root.join("stop").exists() {
                    return Ok(());
                }
                if Instant::now() >= end {
                    return Err(io::Error::other("operator lifetime exhausted"));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if guardian.has_exited()? {
                return Ok(());
            }
            let command = load(&path)?.ok_or_else(|| io::Error::other("command disappeared"))?;
            let deadline = end.min(Instant::now() + Duration::from_secs(30));
            let reply = match command {
                Command::Capture { id } => target
                    .capture(id, deadline, &cancelled)
                    .map(|capture| Reply::Capture { capture }),
                Command::Input {
                    id,
                    observation,
                    input,
                } => target
                    .input(id, observation, input, deadline, &cancelled)
                    .map(|receipt| Reply::Input { receipt }),
                Command::Reconcile { id } => target
                    .reconcile_input(&id, deadline, &cancelled)
                    .map(|receipt| Reply::Input { receipt }),
                Command::Restart { id, previous } => {
                    let epochs = root.join("epochs");
                    std::fs::create_dir_all(&epochs)?;
                    target.restart(&previous, &epochs.join(id.as_str()), deadline, &cancelled)
                        .map(|ready| Reply::Restarted { previous, ready })
                        .map_err(|e| io::Error::other(e.cause))
                }
                Command::Stop => Ok(Reply::Stopped),
            }
            .unwrap_or_else(|error| Reply::Failed {
                error: error.to_string(),
            });
            save_atomic(&root.join(format!("response-{n:03}.json")), &reply)?;
            if matches!(reply, Reply::Stopped) {
                return Ok(());
            }
        }
        Err(io::Error::other("operator command capacity exhausted"))
    })();
    let retirement = target.stop();
    save_atomic(
        &root.join("result.json"),
        &(work.as_ref().err().map(ToString::to_string), &retirement),
    )?;
    work?;
    if !retirement.passed {
        return Err("owned retirement failed".into());
    }
    Ok(())
}
