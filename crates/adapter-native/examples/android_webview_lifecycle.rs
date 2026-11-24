//! One owned Home/return journey with original-page reconciliation and explicit re-pause.
use frontcap_core::{ids::RequestId, state::save_atomic};
use frontcap_native::{android_frames::{Request, State}, android_timers::Action, device::{AndroidLaunch, Device, WebViewLifecycle}};
use std::{path::{Path,PathBuf},sync::atomic::AtomicBool,time::{Duration,Instant}};
fn id(value:&str)->RequestId {RequestId::new(value).unwrap()}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>> {
    let args:Vec<_>=std::env::args_os().skip(1).collect();
    if args.len()!=3 {return Err("expected config, fresh evidence directory and entry URL".into());}
    let config:AndroidLaunch=serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let root=PathBuf::from(&args[1]);let cancelled=AtomicBool::new(false);
    let mut device=Device::acquire(config,&root,Instant::now()+Duration::from_secs(240),&cancelled)?;
    let work=async {
        let deadline=Instant::now()+Duration::from_secs(120);
        device.connect_webview_debugger(deadline,&cancelled).map_err(std::io::Error::other)?;
        let pages=device.inspect_webview_pages(Path::new("/usr/bin/curl"),&root.join("pages"),deadline,&cancelled)?;
        if pages.len()!=1 {return Err(std::io::Error::other("expected one selected page"));}
        let journal=root.join("page");
        let mut page=device.connect_webview_page(&pages[0].id,&journal,deadline,&cancelled).await?;
        let url=args[2].to_str().ok_or_else(||std::io::Error::other("invalid URL"))?;
        // Wait for the host's initial navigation to settle before overriding its entry URL.
        // An initially blank WebView can still have a host-owned navigation queued.
        let mut initial=0;
        loop {
            let snapshot=page.read(&id(&format!("initial-host-{initial}")),deadline,&cancelled).await?;
            if snapshot.initial_navigation_finished() {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;initial+=1;
        }
        page.prepare_frames(&id("prepare"),url,None,deadline,&cancelled).await?;
        let mut poll=0;
        let before_frames=loop {
            let ready=page.read(&id(&format!("ready-{poll}")),deadline,&cancelled).await?;
            let frames=page.inspect_frames(&id(&format!("frames-{poll}")),deadline,&cancelled).await?;
            if ready.ready_state=="complete" && ready.href==url && let Some(frames)=frames {break frames;}
            tokio::time::sleep(Duration::from_millis(50)).await;poll+=1;
        };
        page.control_timers(&id("pause"),&Action::Pause,deadline,&cancelled).await?;
        // Exercise a rendered consumer, rather than a paused bootstrap document.
        for frame in 1..=20 {
            page.control_timers(&id(&format!("warmup-timer-{frame}")),&Action::Advance {milliseconds:100},deadline,&cancelled).await?;
            let clock=page.read(&id(&format!("warmup-clock-{frame}")),deadline,&cancelled).await?;
            page.dispatch_frame(&id(&format!("warmup-frame-{frame}")),&Request {document:before_frames.document.clone(),id:frame,timestamp:clock.performance_ms},deadline,&cancelled).await?;
            let mut poll=0;
            loop {
                let receipt=page.inspect_frames(&id(&format!("warmup-receipt-{frame}-{poll}")),deadline,&cancelled).await?
                    .ok_or_else(||std::io::Error::other("missing warmup frame producer"))?;
                if receipt.last.is_some_and(|r|r.state==State::Completed) {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;poll+=1;
            }
        }
        let before=page.read(&id("before"),deadline,&cancelled).await?;
        page.capture(&id("before-screen"),deadline,&cancelled)?;
        page.close();drop(page);
        let home=device.transition_webview(&id("home"),WebViewLifecycle::Home,deadline,&cancelled)?;
        let returned=device.transition_webview(&id("activate"),WebViewLifecycle::Activate,deadline,&cancelled)?;
        let cached=device.transition_webview(&id("home"),WebViewLifecycle::Home,deadline,&cancelled)?;
        if cached!=home || home.owner!=returned.owner {return Err(std::io::Error::other("host transition ownership differs"));}
        let mut page=device.resume_webview_page(&journal,&id("return"),deadline,&cancelled).await?;
        let after_frames=page.inspect_frames(&id("returned-producer"),deadline,&cancelled).await?
            .ok_or_else(||std::io::Error::other("missing returned frame producer"))?;
        if before_frames.document!=after_frames.document {return Err(std::io::Error::other("return replaced page document"));}
        page.control_timers(&id("re-pause"),&Action::Pause,deadline,&cancelled).await?;
        let after=page.read(&id("after"),deadline,&cancelled).await?;
        page.capture(&id("after-screen"),deadline,&cancelled)?;
        save_atomic(&root.join("lifecycle-observation.json"),&(before,after,home,returned,before_frames,after_frames))?;
        Ok::<_,std::io::Error>(())
    }.await;
    let retirement=device.stop();
    save_atomic(&root.join("lifecycle-result.json"),&(work.as_ref().err().map(ToString::to_string),&retirement))?;
    work?;
    if !retirement.passed {return Err("owned retirement failed".into());}
    Ok(())
}
