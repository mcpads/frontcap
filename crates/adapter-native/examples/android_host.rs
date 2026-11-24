//! Bounded native-host diagnostic. Commands contain native operations, never app business actions.
use frontcap_core::{ids::RequestId, native::{android_webview::{AppProcess, ContactCapture, Debugger},
    contacts::Operation}, process::ProcessIdentity, state::{load, save_atomic}};
use frontcap_native::device::{AndroidLaunch, ContactDriverLimits, Device};
use serde::{Deserialize, Serialize};
use std::{io, path::{Path, PathBuf}, sync::atomic::AtomicBool, time::{Duration, Instant}};

#[derive(Deserialize)]
#[serde(tag="operation",rename_all="snake_case",deny_unknown_fields)]
enum Command {
    Capture { id: RequestId },
    Input { id: RequestId, observation: RequestId, input: Operation },
    Reconcile { id: RequestId },
    DiscoverWebview,
    Stop,
}
#[derive(Serialize)]
#[serde(tag="result",rename_all="snake_case")]
enum Reply {
    Capture { capture: ContactCapture },
    Input { receipt: serde_json::Value },
    Debugger { debugger: Debugger },
    Stopped,
    Failed { error: String },
}
fn execute(device: &mut Device, command: Command, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Reply> {
    match command {
        Command::Capture{id} => Ok(Reply::Capture{capture:device.capture_contacts(&id,deadline,cancelled)?}),
        Command::Input{id,observation,input} => Ok(Reply::Input{receipt:device.input_contacts(&id,&observation,&input,deadline,cancelled)?}),
        Command::Reconcile{id} => Ok(Reply::Input{receipt:device.reconcile_contacts(&id,deadline,cancelled)?}),
        Command::DiscoverWebview => {
            device.discover_webview_debugger(deadline,cancelled)?;
            Ok(Reply::Debugger{debugger:device.webview_debugger().unwrap().clone()})
        }
        Command::Stop => Ok(Reply::Stopped),
    }
}
fn main() -> Result<(),Box<dyn std::error::Error>> {
    let args:Vec<_>=std::env::args_os().skip(1).collect();
    if args.len()!=5 { return Err("expected config, fresh evidence directory, input JAR, JAR SHA256 and live guardian PID".into()); }
    let config:AndroidLaunch=serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.host_app.is_none() { return Err("expected native host_app profile".into()); }
    let root=PathBuf::from(&args[1]);std::fs::create_dir(&root)?;
    let guardian=ProcessIdentity::of(args[4].to_str().ok_or("invalid guardian PID")?.parse()?)?.ok_or("guardian absent")?;
    save_atomic(&root.join("guardian.json"),&guardian)?;
    save_atomic(&root.join("worker.json"),&ProcessIdentity::of(std::process::id())?)?;
    let cancelled=AtomicBool::new(false);
    let mut device=Device::acquire(config,&root.join("target"),Instant::now()+Duration::from_secs(240),&cancelled)?;
    let work=(|| -> io::Result<()> {
        let end=Instant::now()+Duration::from_secs(900);
        let owner:AppProcess=device.app_process().ok_or_else(||io::Error::other("missing host identity"))?.clone();
        let driver=device.start_contact_driver_bounded(Path::new(&args[2]),args[3].to_str().ok_or_else(||io::Error::other("invalid JAR hash"))?,
            ContactDriverLimits{lifetime:Duration::from_secs(900),request_capacity:512},end,&cancelled).map_err(io::Error::other)?;
        save_atomic(&root.join("ready.json"),&(owner,driver))?;
        for number in 1..=512 {
            let path=root.join(format!("command-{number:03}.json"));
            while !path.exists() {
                if guardian.has_exited()? || root.join("stop").exists() { return Ok(()); }
                if Instant::now()>=end { return Err(io::Error::other("operator lifetime exhausted")); }
                std::thread::sleep(Duration::from_millis(100));
            }
            if guardian.has_exited()? { return Ok(()); }
            let command=load(&path)?.ok_or_else(||io::Error::other("command disappeared"))?;
            let reply=execute(&mut device,command,end.min(Instant::now()+Duration::from_secs(30)),&cancelled)
                .unwrap_or_else(|error|Reply::Failed{error:error.to_string()});
            save_atomic(&root.join(format!("response-{number:03}.json")),&reply)?;
            if matches!(reply,Reply::Stopped) { return Ok(()); }
        }
        Err(io::Error::other("operator command capacity exhausted"))
    })();
    let retirement=device.stop();
    save_atomic(&root.join("result.json"),&(work.as_ref().err().map(ToString::to_string),&retirement))?;
    work?;
    if !retirement.passed { return Err("owned retirement failed".into()); }
    Ok(())
}
