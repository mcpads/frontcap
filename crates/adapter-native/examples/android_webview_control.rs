//! Loaded consumer preparation, timer/frame progress, native pixels and same-owner CDP recovery.
use frontcap_core::{ids::RequestId, state::save_atomic};
use frontcap_native::{
    android_frames,
    android_timers::Action,
    device::{AndroidLaunch, Device},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn id(value: impl Into<String>) -> RequestId {
    RequestId::new(value.into()).unwrap()
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("expected config, fresh evidence directory and explicit entry URL".into());
    }
    let entry_url = args[2]
        .to_str()
        .ok_or("entry URL must be UTF-8")?
        .to_owned();
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config
        .webview_app
        .as_ref()
        .and_then(|app| app.local_service.as_ref())
        .is_none()
    {
        return Err("owned local service mapping required".into());
    }
    let root = PathBuf::from(&args[1]);
    let cancelled = AtomicBool::new(false);
    let mut device = Device::acquire(
        config,
        &root,
        Instant::now() + Duration::from_secs(240),
        &cancelled,
    )?;
    let work = async {
        let deadline = Instant::now() + Duration::from_secs(240);
        device
            .connect_webview_debugger(deadline, &cancelled)
            .map_err(std::io::Error::other)?;
        let pages = device.inspect_webview_pages(
            Path::new("/usr/bin/curl"),
            &root.join("pages"),
            deadline,
            &cancelled,
        )?;
        if pages.len() != 1 {
            return Err(std::io::Error::other("expected one page"));
        }
        let evidence = root.join("page");
        let document;
        let held;
        let progress_matches;
        {
            let mut page = device
                .connect_webview_page(&pages[0].id, &evidence, deadline, &cancelled)
                .await?;
            page.prepare_frames(&id("prepare"), &entry_url, None, deadline, &cancelled)
                .await?;
            let mut index = 0;
            loop {
                let snapshot = page
                    .read(&id(format!("startup-read-{index}")), deadline, &cancelled)
                    .await?;
                let receipt = page
                    .inspect_frames(&id(format!("startup-frames-{index}")), deadline, &cancelled)
                    .await?;
                if snapshot.ready_state == "complete"
                    && snapshot.href == entry_url
                    && let Some(receipt) = receipt
                {
                    document = receipt.document;
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::other("consumer startup deadline"));
                }
                index += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            page.control_timers(&id("pause"), &Action::Pause, deadline, &cancelled)
                .await?;
            held = page.read(&id("held-before"), deadline, &cancelled).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let after = page.read(&id("held-after"), deadline, &cancelled).await?;
            if held.performance_ms != after.performance_ms || held.date_ms != after.date_ms {
                return Err(std::io::Error::other("timer domain did not hold"));
            }
            let mut previous_clock = after.performance_ms;
            let mut progress_steps = Vec::new();
            for step in 1..=60 {
                page.control_timers(
                    &id(format!("timer-{step}")),
                    &Action::Advance { milliseconds: 100 },
                    deadline,
                    &cancelled,
                )
                .await?;
                let now = page
                    .read(&id(format!("clock-{step}")), deadline, &cancelled)
                    .await?;
                page.dispatch_frame(
                    &id(format!("frame-{step}")),
                    &android_frames::Request {
                        document: document.clone(),
                        id: step,
                        timestamp: now.performance_ms,
                    },
                    deadline,
                    &cancelled,
                )
                .await?;
                let mut poll = 0;
                loop {
                    let receipt = page
                        .inspect_frames(
                            &id(format!("frame-{step}-read-{poll}")),
                            deadline,
                            &cancelled,
                        )
                        .await?
                        .ok_or_else(|| std::io::Error::other("frame producer disappeared"))?;
                    if receipt
                        .last
                        .as_ref()
                        .is_some_and(|last| last.state == android_frames::State::Completed)
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::other("frame completion deadline"));
                    }
                    poll += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let after_frame = page
                    .read(&id(format!("clock-after-frame-{step}")), deadline, &cancelled)
                    .await?;
                progress_steps.push(serde_json::json!({
                    "step": step,
                    "requested_ms": 100,
                    "before_budget_ms": previous_clock,
                    "after_budget_ms": now.performance_ms,
                    "after_frame_ms": after_frame.performance_ms,
                }));
                previous_clock = after_frame.performance_ms;
                save_atomic(&root.join("progress-steps.json"), &progress_steps)?;
            }
            let progressed = page.read(&id("progressed"), deadline, &cancelled).await?;
            progress_matches = (progressed.performance_ms - held.performance_ms - 6000.0).abs() <= 0.001;
            save_atomic(&root.join("progress.json"), &(&held, &progressed))?;
            // A completed budget with a mismatched observed duration does not prevent
            // independent capture/reconnect checks. Preserve the failure after those checks.
            page.capture(&id("screen"), deadline, &cancelled)?;
        }
        {
            let mut page = device
                .resume_webview_page(&evidence, &id("reconnect"), deadline, &cancelled)
                .await?;
            let receipt = page
                .inspect_frames(&id("reconnected-frames"), deadline, &cancelled)
                .await?
                .ok_or_else(|| std::io::Error::other("missing resumed producer"))?;
            if receipt.document != document {
                return Err(std::io::Error::other("reconnected document changed"));
            }
            let cached = page.read(&id("held-before"), deadline, &cancelled).await?;
            if cached.performance_ms != held.performance_ms {
                return Err(std::io::Error::other("original read changed"));
            }
            page.control_timers(
                &id("reconnected-pause"),
                &Action::Pause,
                deadline,
                &cancelled,
            )
            .await?;
        }
        if !progress_matches {
            return Err(std::io::Error::other("timer progress differs from budgets"));
        }
        Ok::<(), std::io::Error>(())
    }
    .await;
    let retirement = device.stop();
    save_atomic(
        &root.join("control-result.json"),
        &(work.as_ref().err().map(ToString::to_string), &retirement),
    )?;
    work?;
    if !retirement.passed {
        return Err("retirement failed".into());
    }
    Ok(())
}
