//! Discover an external WebView on an owned emulator and retire it. No clock-control claim.
use frontcap_native::device::{AndroidLaunch, Device};
use std::{
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("expected config and new evidence directory".into());
    }
    let config: AndroidLaunch = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if config.webview_app.is_none() || config.view_app.is_some() {
        return Err("expected exclusive webview_app profile".into());
    }
    let mut device = Device::acquire(
        config,
        &PathBuf::from(&args[1]),
        Instant::now() + Duration::from_secs(240),
        &AtomicBool::new(false),
    )?;
    let port = device.connect_webview_debugger(
        Instant::now() + Duration::from_secs(20),
        &AtomicBool::new(false),
    )?;
    let debugger = device.webview_debugger().cloned();
    let pages = device.inspect_webview_pages(
        std::path::Path::new("/usr/bin/curl"),
        &PathBuf::from(&args[1]).join("debugger-list"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    );
    let work = async {
        let pages = pages?;
        if pages.len() != 1 {
            return Err(std::io::Error::other(
                "probe requires exactly one page candidate",
            ));
        }
        let mut page = device
            .connect_webview_page(
                &pages[0].id,
                &PathBuf::from(&args[1]).join("page"),
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .await?;
        let id = frontcap_core::ids::RequestId::new("read").map_err(std::io::Error::other)?;
        let snapshot = page
            .read(
                &id,
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .await?;
        page.close();
        let replay = page
            .read(
                &id,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .await?;
        if serde_json::to_value(&snapshot)? != serde_json::to_value(&replay)? {
            return Err(std::io::Error::other("original read changed"));
        }
        Ok((pages, snapshot))
    }
    .await;
    let retirement = device.stop();
    if !retirement.passed {
        return Err(format!("retirement failed: {:?}", retirement.errors).into());
    }
    println!(
        "{}",
        serde_json::to_string(&(port, debugger.ok_or("missing WebView debugger")?, work?))?
    );
    Ok(())
}
