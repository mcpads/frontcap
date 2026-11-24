//! Qualify live View session ownership and offline capture recovery.
use frontcap_core::ids::RequestId;
use frontcap_native::{
    device::AndroidLaunch,
    session::{Recovery, Session, ViewReply, read_view_observation},
};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err(
            "usage: android_view_session launch.json fresh-directory session-executable".into(),
        );
    }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.view_app.is_none() {
        return Err("requires a View app profile".into());
    }
    let root = PathBuf::from(&args[1]);
    let cancel = AtomicBool::new(false);
    let mut session = Session::start(
        &PathBuf::from(&args[2]),
        config,
        &root,
        Instant::now() + Duration::from_secs(240),
    )?;
    let work = (|| -> Result<_, Box<dyn std::error::Error>> {
        let ready = session.wait_view_ready(Instant::now() + Duration::from_secs(240), &cancel)?;
        let id = RequestId::new("first-view")?;
        let ViewReply::Observed { capture: first } = session.observe_views(
            id.clone(),
            Instant::now() + Duration::from_secs(30),
            &cancel,
        )?
        else {
            return Err("initial session observation failed".into());
        };
        std::thread::sleep(Duration::from_millis(700));
        let ViewReply::Observed { capture: retry } = session.observe_views(
            id.clone(),
            Instant::now() + Duration::from_secs(30),
            &cancel,
        )?
        else {
            return Err("retained observation failed".into());
        };
        if first != retry {
            return Err("retry changed original observation".into());
        }
        let ViewReply::Observed { capture: held } = session.observe_views(
            RequestId::new("held-view")?,
            Instant::now() + Duration::from_secs(30),
            &cancel,
        )?
        else {
            return Err("held observation failed".into());
        };
        if first.now_ns != 0 || held.now_ns != 0 || first.capture_id == held.capture_id {
            return Err("held observation is not fresh at zero controlled time".into());
        }
        println!(
            "{}: {} Views, retry preserved, fresh held capture at {} ns",
            ready.package,
            held.catalog.targets.len(),
            held.now_ns
        );
        Ok((id, first))
    })();
    let retirement = session.stop(Instant::now() + Duration::from_secs(30))?;
    let (id, first) = work?;
    if !matches!(retirement,Recovery::Finished {result,..} if result.error.is_none() && result.retirement.as_ref().is_some_and(|r|r.passed))
    {
        return Err("session retirement was not confirmed".into());
    }
    if read_view_observation(&root, &id)? != Some(first) {
        return Err("offline recovery differs".into());
    }
    println!("owned retirement and offline recovery verified");
    Ok(())
}
