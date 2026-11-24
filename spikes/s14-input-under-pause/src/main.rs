//! Input and navigation under the frozen state (virtual-time pause): which key, mouse, and wheel events
//! apply before any advance; whether reload and history navigation answer at the commit; whether going
//! back restores a document from the back-forward cache.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, fetch, input, network, page, runtime};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn frozen_page(b: &Browser, url: &str) -> anyhow::Result<Page> {
    let c = &b.client;
    let p = b.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(c, &p, 50.0).await?;
    Ok(p)
}

/// A move carries no button: `button: left` on `mouseMoved` means the button is held (a drag), and a
/// wheel event dispatched during a drag is never answered.
fn mouse(kind: input::DispatchMouseEventParamsType, x: f64, y: f64, count: i64) -> input::DispatchMouseEventParams {
    let moved = kind == input::DispatchMouseEventParamsType::MouseMoved;
    input::DispatchMouseEventParams {
        button: Some(if moved && std::env::var("DRAG_MOVE").is_err() { input::MouseButton::None } else { input::MouseButton::Left }),
        click_count: Some(count),
        ..input::DispatchMouseEventParams::new(kind, x, y)
    }
}

async fn take_log(b: &Browser, p: &Page) -> anyhow::Result<serde_json::Value> {
    Ok(eval(&b.client, p, "(() => { const l = log; window.log = []; return l; })()").await?)
}

async fn keys_and_mouse(b: &Browser, url: &str) -> anyhow::Result<serde_json::Value> {
    let c = &b.client;
    let p = frozen_page(b, url).await?;
    let s = Some(p.session.as_str());
    eval(c, &p, "document.getElementById('q').focus()").await?;
    take_log(b, &p).await?;
    c.call(s, &input::DispatchKeyEventParams { key: Some("a".into()), code: Some("KeyA".into()), text: Some("a".into()), windows_virtual_key_code: Some(65), ..input::DispatchKeyEventParams::new(input::DispatchKeyEventParamsType::KeyDown) }).await?;
    c.call(s, &input::DispatchKeyEventParams { key: Some("a".into()), code: Some("KeyA".into()), windows_virtual_key_code: Some(65), ..input::DispatchKeyEventParams::new(input::DispatchKeyEventParamsType::KeyUp) }).await?;
    let after_key = eval(c, &p, "document.getElementById('q').value").await?;
    let key_events = take_log(b, &p).await?;
    c.call(s, &input::InsertTextParams::new("한글".into())).await?;
    let after_insert = eval(c, &p, "document.getElementById('q').value").await?;
    let insert_events = take_log(b, &p).await?;
    use input::DispatchMouseEventParamsType as M;
    let skip = std::env::var("SKIP").unwrap_or_default();
    if !skip.contains("dbl") {
    for (kind, count) in [(M::MousePressed, 1), (M::MouseReleased, 1), (M::MousePressed, 2), (M::MouseReleased, 2)] {
        c.call(s, &mouse(kind, 60.0, 30.0, count)).await?;
    }
    }
    eval(c, &p, "0").await?;
    let dbl = take_log(b, &p).await?;
    if !skip.contains("move1") { c.call(s, &mouse(M::MouseMoved, 61.0, 31.0, 0)).await?; }
    eval(c, &p, "0").await?;
    let first_move = take_log(b, &p).await?;
    if !skip.contains("move2") { c.call(s, &mouse(M::MouseMoved, 61.0, 31.0, 0)).await?; }
    eval(c, &p, "0").await?;
    let same_move = take_log(b, &p).await?;
    let (c2, s2) = (c.clone(), p.session.clone());
    let mut wheel = tokio::spawn(async move { c2.call(Some(&s2), &input::DispatchMouseEventParams { delta_x: Some(0.0), delta_y: Some(400.0), ..input::DispatchMouseEventParams::new(M::MouseWheel, 300.0, 300.0) }).await.map(|_| ()) });
    let t_wheel = std::time::Instant::now();
    let answered_frozen = tokio::time::timeout(Duration::from_millis(3000), &mut wheel).await.is_ok();
    let wheel_ms = t_wheel.elapsed().as_millis() as u64;
    let scroll = vec![eval(c, &p, "window.scrollY").await?];
    let scroll_after_100ms_wall = serde_json::json!(wheel_ms);
    let mut steps = vec![];
    let mut answered_after = None;
    for i in 1..=10 {
        advance(c, &p, 1.0).await?;
        steps.push(eval(c, &p, "window.scrollY").await?);
        if answered_after.is_none() && wheel.is_finished() {
            answered_after = Some(i);
        }
    }
    let wheel_answer = if answered_frozen || wheel.is_finished() { "answered" } else { "pending" };
    let wheel_events = take_log(b, &p).await?;
    let scroll_after_advance = serde_json::json!({"answered_while_frozen": answered_frozen, "answered_after_1ms_advances": answered_after, "scroll_y_per_1ms_advance": steps, "final": wheel_answer});
    let js_scroll = { eval(c, &p, "window.scrollTo(0, 0); window.scrollBy(0, 250); window.scrollY").await? };
    Ok(serde_json::json!({
        "key_value_while_frozen": after_key, "key_events": key_events,
        "insert_value_while_frozen": after_insert, "insert_events": insert_events,
        "dblclick_events": dbl, "first_move": first_move, "same_point_move": same_move,
        "scroll_y_after_round_trips": scroll, "scroll_y_after_100ms_wall": scroll_after_100ms_wall,
        "wheel_events": wheel_events, "wheel": scroll_after_advance, "script_scroll_by_250_while_frozen": js_scroll,
    }))
}

/// Issues a navigation command with the document intercepted; reports whether the command answered
/// before `frameNavigated`, and whether the new document ran script before an advance.
async fn intercepted(b: &Browser, p: &Page, cmd: &str) -> anyhow::Result<serde_json::Value> {
    let c = b.client.clone();
    let s = p.session.clone();
    c.call(Some(&s), &fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*".into()), request_stage: Some(fetch::RequestStage::Request), ..Default::default() }]), ..Default::default() }).await?;
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(Some(&s));
    let mut navigated = c.subscribe::<page::FrameNavigatedEvent>(Some(&s));
    let mut contexts = c.subscribe::<runtime::ExecutionContextCreatedEvent>(Some(&s));
    let (c2, s2) = (c.clone(), s.clone());
    let cmd = cmd.to_string();
    let index = if cmd == "back" {
        let h = c.call(Some(&s), &page::GetNavigationHistoryParams::default()).await?;
        Some(h.entries[h.current_index as usize - 1].id)
    } else {
        None
    };
    let mut call = tokio::spawn(async move {
        match index {
            Some(id) => c2.call(Some(&s2), &page::NavigateToHistoryEntryParams::new(id)).await.map(|_| ()),
            None => c2.call(Some(&s2), &page::ReloadParams::default()).await.map(|_| ()),
        }
    });
    let mut order = vec![];
    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !done || order.iter().all(|o: &String| o != "frameNavigated") {
        tokio::select! {
            r = &mut call, if !done => { order.push(format!("answered {:?}", r?.map_err(|e| e.to_string()))); done = true; }
            ev = paused.next() => { let ev = ev?; order.push(format!("requestPaused {:?}", ev.resource_type)); c.call(Some(&s), &fetch::ContinueRequestParams::new(ev.request_id)).await?; }
            ev = navigated.next() => { ev?; order.push("frameNavigated".into()); }
            _ = tokio::time::sleep_until(deadline) => { order.push("timeout".into()); break; }
        }
    }
    let ran_before_advance = eval(&c, p, "window.__ran === true").await?;
    let mut new_contexts = 0;
    while contexts.next_timeout(Duration::from_millis(50)).await?.is_some() {
        new_contexts += 1;
    }
    c.call(Some(&s), &fetch::DisableParams::default()).await?;
    Ok(serde_json::json!({"order": order, "script_ran_before_advance": ran_before_advance, "new_contexts": new_contexts}))
}

async fn navigation(b: &Browser, base: &str) -> anyhow::Result<serde_json::Value> {
    let c = &b.client;
    let p = frozen_page(b, &format!("{base}/index.html")).await?;
    let first_id = eval(c, &p, "window.__id").await?;
    let reload = intercepted(b, &p, "reload").await?;
    advance(c, &p, 50.0).await?;
    let reloaded_id = eval(c, &p, "window.__id").await?;
    let before_back = eval(c, &p, "window.__id").await?;
    navigate(c, &p, &format!("{base}/b.html")).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(c, &p, 50.0).await?;
    let back = intercepted(b, &p, "back").await?;
    advance(c, &p, 50.0).await?;
    let after_back = eval(c, &p, "window.__id").await?;
    let nav_type = eval(c, &p, "performance.getEntriesByType('navigation')[0].type").await?;
    Ok(serde_json::json!({
        "reload": reload, "reload_new_document": first_id != reloaded_id,
        "back": back, "back_restored_same_window": before_back == after_back, "back_navigation_type": nav_type,
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let base = format!("http://{addr}");
    let flags = vec!["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string()];
    let browser = Browser::start(&flags).await?;
    let input_report = keys_and_mouse(&browser, &format!("{base}/index.html")).await?;
    let nav_default = navigation(&browser, &base).await?;
    let mut no_bfcache = flags.clone();
    no_bfcache.push("--disable-features=BackForwardCache".into());
    let browser2 = Browser::start(&no_bfcache).await?;
    let nav_no_bfcache = navigation(&browser2, &base).await?;
    report("s14-input-under-pause", serde_json::json!({"input": input_report, "navigation": nav_default, "navigation_without_bfcache": nav_no_bfcache}));
    Ok(())
}
