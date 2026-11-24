//! Bounded interactive diagnostic on one owned Android consumer; no arbitrary page evaluation.
use frontcap_core::{ids::RequestId, native::contacts::Operation, state::save_atomic};
use frontcap_native::{android_frames::Request, android_timers::Action,
    device::{AndroidLaunch, Device, ContactDriverLimits, WebViewLifecycle}};
use std::{path::{Path,PathBuf},sync::atomic::AtomicBool,time::{Duration,Instant}};
fn id(s:impl Into<String>)->RequestId {RequestId::new(s.into()).unwrap()}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>> {
    let args:Vec<_>=std::env::args_os().skip(1).collect();
    if args.len()!=5 {return Err("expected config, fresh evidence directory, entry URL, driver JAR and SHA256".into());}
    let config:AndroidLaunch=serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let root=PathBuf::from(&args[1]);let cancelled=AtomicBool::new(false);
    let mut device=Device::acquire(config,&root,Instant::now()+Duration::from_secs(240),&cancelled)?;
    let work=async {
        let deadline=Instant::now()+Duration::from_secs(900);
        device.start_contact_driver_bounded(Path::new(&args[3]),args[4].to_str().ok_or_else(||std::io::Error::other("invalid hash"))?,ContactDriverLimits { lifetime: Duration::from_secs(900), request_capacity: 128 },deadline,&cancelled)
            .map_err(std::io::Error::other)?;
        device.connect_webview_debugger(deadline,&cancelled).map_err(std::io::Error::other)?;
        let pages=device.inspect_webview_pages(Path::new("/usr/bin/curl"),&root.join("pages"),deadline,&cancelled)?;
        if pages.len()!=1 {return Err(std::io::Error::other("expected one page"));}
        let mut page=device.connect_webview_page(&pages[0].id,&root.join("page"),deadline,&cancelled).await?;
        let url=args[2].to_str().ok_or_else(||std::io::Error::other("invalid entry URL"))?;
        // Wait for the host's initial navigation to settle before overriding its entry URL.
        // An initially blank WebView can still have a host-owned navigation queued.
        let mut initial=0;
        loop {
            let snapshot=page.read(&id(format!("initial-host-{initial}")),deadline,&cancelled).await?;
            if snapshot.initial_navigation_finished() {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;initial+=1;
        }
        page.prepare_frames(&id("prepare"),url,None,deadline,&cancelled).await?;
        let mut poll=0;
        let document=loop {
            let snapshot=page.read(&id(format!("ready-{poll}")),deadline,&cancelled).await?;
            let receipt=page.inspect_frames(&id(format!("producer-{poll}")),deadline,&cancelled).await?;
            if snapshot.ready_state=="complete" && snapshot.href==url && let Some(receipt)=receipt {break receipt.document;}
            tokio::time::sleep(Duration::from_millis(50)).await;poll+=1;
        };
        page.control_timers(&id("pause"),&Action::Pause,deadline,&cancelled).await?;
        let mut frame=1;
        let mut errors=Vec::new();
        for sequence in 1..=128 {
            let path=root.join(format!("command-{sequence}.json"));
            save_atomic(&root.join("command-ready.json"),&sequence)?;
            while !path.exists() {
                if Instant::now()>=deadline {return Err(std::io::Error::other("operator command deadline"));}
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            #[derive(serde::Deserialize)]
            #[serde(tag="operation",rename_all="snake_case",deny_unknown_fields)]
            enum Command {
                Observe { #[serde(default)] input_basis:bool },
                Advance { milliseconds:u32,frames:u32 },
                Input { observation:frontcap_core::ids::RequestId,contacts:Operation },
                HomeReturn,
                Stop,
            }
            let bytes=std::fs::read(path)?;
            if bytes.len()>8192 {return Err(std::io::Error::other("operator command too large"));}
            let command:Command=serde_json::from_slice(&bytes)?;
            let name=format!("operator-{sequence}");
            let response=root.join(format!("response-{sequence}.json"));
            if matches!(command,Command::Stop) {
                save_atomic(&response,&serde_json::json!({"stopped":true,"prior_errors":errors}))?;
                if !errors.is_empty() {return Err(std::io::Error::other("operator batch has rejected or failed commands"));}
                return Ok(());
            }
            if matches!(command,Command::HomeReturn) {
                // Re-pause first: unresolved timer effects must not be crossed by a transition.
                let before=async {
                    let receipt=page.inspect_frames(&id(format!("{name}-before-producer")),deadline,&cancelled).await?
                        .ok_or_else(||std::io::Error::other("missing producer before host transition"))?;
                    if receipt.last.is_some_and(|r|r.state!=frontcap_native::android_frames::State::Completed) {
                        return Err(std::io::Error::other("frame effect unresolved before host transition"));
                    }
                    page.control_timers(&id(format!("{name}-before-pause")),&Action::Pause,deadline,&cancelled).await?;
                    let clock=page.read(&id(format!("{name}-before-clock")),deadline,&cancelled).await?;
                    let screen=page.capture(&id(format!("{name}-before-screen")),deadline,&cancelled)?;
                    Ok::<_,std::io::Error>((clock,screen))
                }.await;
                let (before_clock,before_screen)=match before {
                    Ok(value)=>value,
                    Err(error)=>{
                        errors.push((sequence,error.to_string()));
                        save_atomic(&response,&serde_json::json!({"error":error.to_string()}))?;
                        continue;
                    }
                };
                page.close();drop(page);
                let transitions=(|| {
                    let home_id=id(format!("{name}-home"));
                    let home=device.transition_webview(&home_id,WebViewLifecycle::Home,deadline,&cancelled)?;
                    let returned=device.transition_webview(&id(format!("{name}-activate")),WebViewLifecycle::Activate,deadline,&cancelled)?;
                    let cached=device.transition_webview(&home_id,WebViewLifecycle::Home,deadline,&cancelled)?;
                    if cached!=home || home.owner!=returned.owner {return Err(std::io::Error::other("host transition owner changed"));}
                    Ok::<_,std::io::Error>((home,returned))
                })();
                // A resume failure is terminal; retain the command error before owned cleanup.
                page=match device.resume_webview_page(&root.join("page"),&id(format!("{name}-return")),deadline,&cancelled).await {
                    Ok(page)=>page,
                    Err(error)=>{
                        save_atomic(&response,&serde_json::json!({"error":error.to_string(),"transition_error":transitions.as_ref().err().map(ToString::to_string)}))?;
                        return Err(error);
                    }
                };
                let result=async {
                    let (home,returned)=transitions?;
                    let receipt=page.inspect_frames(&id(format!("{name}-returned-producer")),deadline,&cancelled).await?
                        .ok_or_else(||std::io::Error::other("missing returned producer"))?;
                    if receipt.document!=document {return Err(std::io::Error::other("return replaced page document"));}
                    page.control_timers(&id(format!("{name}-returned-pause")),&Action::Pause,deadline,&cancelled).await?;
                    let clock=page.read(&id(format!("{name}-clock")),deadline,&cancelled).await?;
                    let screen=page.capture(&id(format!("{name}-screen")),deadline,&cancelled)?;
                    let pointers=page.inspect_pointers(&id(format!("{name}-pointers")),deadline,&cancelled).await?;
                    Ok::<_,std::io::Error>(serde_json::json!({"before_clock":before_clock,"before_screen":before_screen,
                        "clock":clock,"screen":screen,"home":home,"returned":returned,"receipt":receipt,"pointers":pointers}))
                }.await;
                match result {
                    Ok(value)=>save_atomic(&response,&value)?,
                    Err(error)=>{
                        errors.push((sequence,error.to_string()));
                        save_atomic(&response,&serde_json::json!({"error":error.to_string()}))?;
                    }
                }
                continue;
            }
            let result=async {
                let mut basis=None;
                let mut input=None;
                match command {
                    Command::Observe{input_basis}=>{
                        if input_basis {basis=Some(page.capture_contacts(&id(&name),deadline,&cancelled)?);}
                    }
                    Command::Advance{milliseconds,frames}=>{
                        if frames==0 || frames>300 {return Err(std::io::Error::other("frame count must be 1..300"));}
                        for _ in 0..frames {
                            page.control_timers(&id(format!("timer-{frame}")),&Action::Advance{milliseconds},deadline,&cancelled).await?;
                            let clock=page.read(&id(format!("clock-{frame}")),deadline,&cancelled).await?;
                            page.dispatch_frame(&id(format!("frame-{frame}")),&Request{document:document.clone(),id:frame,timestamp:clock.performance_ms},deadline,&cancelled).await?;
                            let mut poll=0;
                            loop {
                                let receipt=page.inspect_frames(&id(format!("frame-{frame}-poll-{poll}")),deadline,&cancelled).await?
                                    .ok_or_else(||std::io::Error::other("missing producer"))?;
                                if receipt.last.is_some_and(|r|r.state==frontcap_native::android_frames::State::Completed) {break;}
                                tokio::time::sleep(Duration::from_millis(10)).await;poll+=1;
                            }
                            frame+=1;
                        }
                    }
                    Command::Input{observation,contacts}=>{
                        input=Some(page.input_contacts(&id(&name),&observation,&contacts,deadline,&cancelled)?);
                    }
                    Command::Stop | Command::HomeReturn=>unreachable!(),
                }
                let screen=if let Some(b)=&basis {b.path.clone()} else {page.capture(&id(format!("{name}-screen")),deadline,&cancelled)?};
                let clock=page.read(&id(format!("{name}-clock")),deadline,&cancelled).await?;
                let pointers=page.inspect_pointers(&id(format!("{name}-pointers")),deadline,&cancelled).await?;
                Ok::<_,std::io::Error>(serde_json::json!({"screen":screen,"basis":basis,"input":input,"clock":clock,"pointers":pointers}))
            }.await;
            match result {
                Ok(value)=>save_atomic(&response,&value)?,
                Err(error)=>{
                    errors.push((sequence,error.to_string()));
                    save_atomic(&response,&serde_json::json!({"error":error.to_string()}))?;
                }
            }
        }
        Err(std::io::Error::other("operator command capacity exhausted"))
    }.await;
    let retirement=device.stop();
    save_atomic(&root.join("play-result.json"),&(work.as_ref().err().map(ToString::to_string),&retirement))?;
    work?;if !retirement.passed {return Err("retirement failed".into());}Ok(())
}
