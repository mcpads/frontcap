//! Source-bound session check: long-lived ownership, clock read/advance and retirement.
//! This is a qualification probe, not a recorder/MCP entry point.
use frontcap_core::{ids::RequestId, state::save_atomic};
use frontcap_native::{
    device::AndroidLaunch,
    request::Operation,
    session::{Recovery, ViewReply, Session},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err(
            "usage: acquire_android launch.json new-session-directory native-session-executable"
                .into(),
        );
    }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let directory = PathBuf::from(&args[1]);
    let cancel = AtomicBool::new(false);
    eprintln!("starting owned Android session");
    let acquisition = Instant::now() + Duration::from_secs(240);
    let mut session = Session::start(Path::new(&args[2]), config, &directory, acquisition)?;
    let work = (|| -> Result<(), Box<dyn std::error::Error>> {
        session.wait_view_ready(acquisition, &cancel)?;
        for (label, operation) in [
            ("read", Operation::ReadClock {}),
            ("advance", Operation::Advance { by_us: 1000 }),
        ] {
            eprintln!("executing {label} through session owner");
            match session.execute_view_operation(
                RequestId::new(label)?,
                operation,
                Instant::now() + Duration::from_secs(10),
                &cancel,
            )? {
                ViewReply::Completed { .. } => {}
                ViewReply::Observed { .. } | ViewReply::Unavailable { .. } => return Err("unexpected observation reply".into()),
                ViewReply::Failed { error, .. } => return Err(format!("{label}: {error}").into()),
            }
        }
        Ok(())
    })();
    eprintln!("waiting for owned session retirement");
    let retirement = session.stop(Instant::now() + Duration::from_secs(8));
    let evidence = match &retirement {
        Ok(Recovery::Finished { result, .. }) => save_atomic(
            &directory.join("probe.json"),
            &(work.as_ref().err().map(ToString::to_string), result),
        ),
        other => save_atomic(
            &directory.join("probe.json"),
            &(
                work.as_ref().err().map(ToString::to_string),
                format!("retirement unconfirmed: {other:?}"),
            ),
        ),
    };
    if let Err(error) = &evidence {
        eprintln!("probe report publication failed: {error}");
    }
    work?;
    let Recovery::Finished { result, .. } = retirement? else {
        return Err("session retirement unconfirmed".into());
    };
    if result.error.is_some() || !result.retirement.is_some_and(|r| r.passed) {
        return Err(format!("session failed: {:?}", result.error).into());
    }
    evidence?;
    eprintln!("session acquisition, clock read/advance and retirement passed");
    Ok(())
}
