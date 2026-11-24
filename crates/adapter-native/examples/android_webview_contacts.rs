//! Screen-selected multi-contact probe, Start press/release and observed consumer progress.
use frontcap_core::{ids::RequestId, native::contacts::{Contact, Operation}, state::save_atomic};
use frontcap_native::{android_frames::Request, android_timers::Action,
    android_webview_page::Connection, device::{AndroidLaunch, Device}};
use std::{path::{Path,PathBuf},sync::atomic::AtomicBool,time::{Duration,Instant}};
fn id(s:impl Into<String>)->RequestId {RequestId::new(s.into()).unwrap()}
async fn steps(page:&mut Connection<'_>,document:&str,first:u64,count:u64,
    deadline:Instant,cancelled:&AtomicBool)->std::io::Result<()> {
    for n in first..first+count {
        page.control_timers(&id(format!("timer-{n}")),&Action::Advance{milliseconds:100},deadline,cancelled).await?;
        let clock=page.read(&id(format!("clock-{n}")),deadline,cancelled).await?;
        page.dispatch_frame(&id(format!("frame-{n}")),&Request{document:document.into(),id:n,timestamp:clock.performance_ms},deadline,cancelled).await?;
        let mut poll=0;
        loop {
            let receipt=page.inspect_frames(&id(format!("frame-{n}-poll-{poll}")),deadline,cancelled).await?
                .ok_or_else(||std::io::Error::other("missing frame producer"))?;
            if receipt.last.is_some_and(|r|r.state==frontcap_native::android_frames::State::Completed) {break;}
            tokio::time::sleep(Duration::from_millis(20)).await;poll+=1;
        }
    }
    Ok(())
}
async fn pointers(page:&mut Connection<'_>,name:&str,count:u64,deadline:Instant,cancelled:&AtomicBool)
    ->std::io::Result<frontcap_native::android_pointers::Snapshot> {
    let mut poll=0;
    loop {
        let snapshot=page.inspect_pointers(&id(format!("{name}-pointers-{poll}")),deadline,cancelled).await?;
        if snapshot.received>=count {return Ok(snapshot);}
        tokio::time::sleep(Duration::from_millis(25)).await;poll+=1;
    }
}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>> {
    let args:Vec<_>=std::env::args_os().skip(1).collect();
    if args.len()!=5 {return Err("expected config, fresh evidence directory, entry URL, driver JAR and SHA256".into());}
    let config:AndroidLaunch=serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let root=PathBuf::from(&args[1]);let cancelled=AtomicBool::new(false);
    let mut device=Device::acquire(config,&root,Instant::now()+Duration::from_secs(240),&cancelled)?;
    let work=async {
        let deadline=Instant::now()+Duration::from_secs(240);
        device.start_contact_driver(Path::new(&args[3]),args[4].to_str().ok_or_else(||std::io::Error::other("invalid hash"))?,deadline,&cancelled)
            .map_err(std::io::Error::other)?;
        device.connect_webview_debugger(deadline,&cancelled).map_err(std::io::Error::other)?;
        let pages=device.inspect_webview_pages(Path::new("/usr/bin/curl"),&root.join("pages"),deadline,&cancelled)?;
        if pages.len()!=1 {return Err(std::io::Error::other("expected one page"));}
        let mut page=device.connect_webview_page(&pages[0].id,&root.join("page"),deadline,&cancelled).await?;
        let url=args[2].to_str().ok_or_else(||std::io::Error::other("invalid entry URL"))?;
        page.prepare_frames(&id("prepare"),url,None,deadline,&cancelled).await?;
        let mut poll=0;
        let document=loop {
            let snapshot=page.read(&id(format!("ready-{poll}")),deadline,&cancelled).await?;
            let receipt=page.inspect_frames(&id(format!("producer-{poll}")),deadline,&cancelled).await?;
            if snapshot.ready_state=="complete" && snapshot.href==url && let Some(receipt)=receipt {break receipt.document;}
            tokio::time::sleep(Duration::from_millis(50)).await;poll+=1;
        };
        page.control_timers(&id("pause"),&Action::Pause,deadline,&cancelled).await?;
        steps(&mut page,&document,1,30,deadline,&cancelled).await?;
        let basis=page.capture_contacts(&id("before-input"),deadline,&cancelled)?;
        save_atomic(&root.join("selection-ready.json"),&basis)?;
        // The caller selects from this actual native capture; never infer CSS coordinates.
        let selection=root.join("point.json");
        while !selection.exists() {
            if Instant::now()>=deadline {return Err(std::io::Error::other("screen selection deadline"));}
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        #[derive(serde::Deserialize,serde::Serialize)]
        #[serde(deny_unknown_fields)]
        struct Point { capture_sha256:String,x:f64,y:f64,probe:[Contact;2] }
        let point:Point=serde_json::from_slice(&std::fs::read(selection)?)?;
        if point.capture_sha256!=basis.sha256 {return Err(std::io::Error::other("selection belongs to another capture"));}
        save_atomic(&root.join("selected-point.json"),&point)?;
        let baseline=page.inspect_pointers(&id("before-probe"),deadline,&cancelled).await?;
        if baseline.received!=0 || point.probe[0].id!=1 || point.probe[1].id!=2 {
            return Err(std::io::Error::other("probe requires unused observer and contact IDs 1/2"));
        }
        let mut moved=point.probe[0].clone();moved.x+=0.01;
        let operations=[
            Operation::Set{expected_revision:0,contacts:vec![point.probe[0].clone()]},
            Operation::Set{expected_revision:1,contacts:point.probe.to_vec()},
            Operation::Set{expected_revision:2,contacts:vec![point.probe[0].clone()]},
            Operation::Set{expected_revision:3,contacts:vec![moved]},
            Operation::Cancel{expected_revision:4},
        ];
        let mut probe_results=Vec::new();
        for (index,operation) in operations.iter().enumerate() {
            let basis=page.capture_contacts(&id(format!("probe-basis-{index}")),deadline,&cancelled)?;
            let reply=page.input_contacts(&id(format!("probe-{index}")),&basis.id,operation,deadline,&cancelled)?;
            if reply["status"]!="injected" {return Err(std::io::Error::other("probe input rejected"));}
            let observed=pointers(&mut page,&format!("probe-{index}"),index as u64+1,deadline,&cancelled).await?;
            probe_results.push((reply,observed));
            save_atomic(&root.join("probe-results.json"),&probe_results)?;
        }
        let delivered=&probe_results.last().unwrap().1;
        let kinds:Vec<_>=delivered.events.iter().map(|e|e.kind.as_str()).collect();
        if kinds!=["pointerdown","pointerdown","pointerup","pointermove","pointercancel"]
            || delivered.active!=0 || delivered.maximum_active!=2 || delivered.events.iter().any(|e|!e.trusted) {
            return Err(std::io::Error::other("native probe delivery differs"));
        }
        let basis=page.capture_contacts(&id("before-start"),deadline,&cancelled)?;
        let down=Operation::Set{expected_revision:5,contacts:vec![Contact{id:1,x:point.x,y:point.y}]};
        let pressed=page.input_contacts(&id("down"),&basis.id,&down,deadline,&cancelled)?;
        if pressed["status"]!="injected" {return Err(std::io::Error::other("press rejected"));}
        let release_basis=page.capture_contacts(&id("before-release"),deadline,&cancelled)?;
        let released=page.input_contacts(&id("up"),&release_basis.id,&Operation::Set{expected_revision:6,contacts:vec![]},deadline,&cancelled)?;
        if released["status"]!="injected" {return Err(std::io::Error::other("release rejected"));}
        if page.reconcile_contacts(&id("down"),deadline,&cancelled)?!=pressed {return Err(std::io::Error::other("original input result changed"));}
        let delivered=pointers(&mut page,"start",7,deadline,&cancelled).await?;
        save_atomic(&root.join("start-delivery.json"),&delivered)?;
        page.capture(&id("after-input"),deadline,&cancelled)?;
        steps(&mut page,&document,31,30,deadline,&cancelled).await?;
        page.capture(&id("after-progress"),deadline,&cancelled)?;
        let snapshot=page.read(&id("after-progress-read"),deadline,&cancelled).await?;
        save_atomic(&root.join("input-result.json"),&(pressed,released,snapshot))?;
        Ok::<(),std::io::Error>(())
    }.await;
    let retirement=device.stop();
    save_atomic(&root.join("contacts-result.json"),&(work.as_ref().err().map(ToString::to_string),&retirement))?;
    work?;if !retirement.passed {return Err("retirement failed".into());}Ok(())
}
