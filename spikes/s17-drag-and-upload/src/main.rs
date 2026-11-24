//! Drag and upload under the frozen state (virtual-time pause): does a pointer drag (press, moves with the
//! button held, release) reach the page while frozen, or only across advances; does HTML drag and drop
//! work natively or through `Input.setInterceptDrags` + `Input.dispatchDragEvent`; does
//! `DOM.setFileInputFiles` fire `input`/`change` while frozen, and what does `lastModified` read?
use cdp::protocol::{dom, emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn frozen(b: &Browser, url: &str) -> anyhow::Result<Page> {
    let c = &b.client;
    let p = b.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(c, &p, 50.0).await?;
    Ok(p)
}

fn mouse(kind: input::DispatchMouseEventParamsType, x: f64, y: f64, held: bool) -> input::DispatchMouseEventParams {
    use input::DispatchMouseEventParamsType as M;
    let button = match kind {
        M::MouseMoved if !held => input::MouseButton::None,
        _ => input::MouseButton::Left,
    };
    let buttons = match kind {
        M::MousePressed => 1,
        M::MouseMoved if held => 1,
        _ => 0,
    };
    input::DispatchMouseEventParams { button: Some(button), buttons: Some(buttons), click_count: Some(if kind == M::MouseMoved { 0 } else { 1 }), ..input::DispatchMouseEventParams::new(kind, x, y) }
}

async fn take(b: &Browser, p: &Page) -> anyhow::Result<serde_json::Value> {
    eval(&b.client, p, "(() => { const l = log; window.log = []; return l; })()").await
}

/// Each call bounded by 3 s of wall time: `"timeout"` when the engine never answers.
async fn send<T: cdp::Command>(b: &Browser, p: &Page, params: &T) -> String {
    match tokio::time::timeout(Duration::from_secs(3), b.client.call(Some(p.session.as_str()), params)).await {
        Ok(Ok(_)) => "ok".into(),
        Ok(Err(e)) => format!("error: {e}"),
        Err(_) => "timeout".into(),
    }
}

/// A pointer drag of the red handle from (40, 40) to (140, 40) in `moves` moves, with `step_ms` of virtual
/// time between events (0: all at one frozen instant).
async fn pointer_drag(b: &Browser, url: &str, moves: usize, step_ms: f64) -> anyhow::Result<serde_json::Value> {
    use input::DispatchMouseEventParamsType as M;
    let p = frozen(b, url).await?;
    take(b, &p).await?;
    let mut answers = vec![send(b, &p, &mouse(M::MouseMoved, 40.0, 40.0, false)).await, send(b, &p, &mouse(M::MousePressed, 40.0, 40.0, true)).await];
    for i in 1..=moves {
        if step_ms > 0.0 {
            advance(&b.client, &p, step_ms).await?;
        }
        answers.push(send(b, &p, &mouse(M::MouseMoved, 40.0 + 100.0 * i as f64 / moves as f64, 40.0, true)).await);
    }
    answers.push(send(b, &p, &mouse(M::MouseReleased, 140.0, 40.0, true)).await);
    let frozen_log = take(b, &p).await?;
    let left_frozen = eval(&b.client, &p, "document.getElementById('handle').style.left").await?;
    advance(&b.client, &p, 50.0).await?;
    let later = take(b, &p).await?;
    let left = eval(&b.client, &p, "document.getElementById('handle').style.left").await?;
    Ok(serde_json::json!({"answers": answers, "frozen": frozen_log, "left_frozen": left_frozen, "after_advance": later, "left": left}))
}

/// HTML drag and drop of #src onto #dst with plain mouse events.
async fn native_dnd(b: &Browser, url: &str, step_ms: f64) -> anyhow::Result<serde_json::Value> {
    use input::DispatchMouseEventParamsType as M;
    let p = frozen(b, url).await?;
    take(b, &p).await?;
    let mut answers = vec![send(b, &p, &mouse(M::MouseMoved, 50.0, 140.0, false)).await, send(b, &p, &mouse(M::MousePressed, 50.0, 140.0, true)).await];
    for (x, y) in [(60.0, 140.0), (150.0, 160.0), (260.0, 170.0), (270.0, 170.0)] {
        if step_ms > 0.0 {
            advance(&b.client, &p, step_ms).await?;
        }
        answers.push(send(b, &p, &mouse(M::MouseMoved, x, y, true)).await);
    }
    answers.push(send(b, &p, &mouse(M::MouseReleased, 270.0, 170.0, true)).await);
    let frozen_log = take(b, &p).await?;
    advance(&b.client, &p, 50.0).await?;
    let later = take(b, &p).await?;
    Ok(serde_json::json!({"answers": answers, "frozen": frozen_log, "after_advance": later}))
}

/// HTML drag and drop through interception: the drag the engine starts is reported, and the platform
/// dispatches its enter, over, and drop at the target.
async fn intercepted_dnd(b: &Browser, url: &str, step_ms: f64) -> anyhow::Result<serde_json::Value> {
    use input::DispatchMouseEventParamsType as M;
    let p = frozen(b, url).await?;
    let s = Some(p.session.as_str());
    b.client.call(s, &input::SetInterceptDragsParams::new(true)).await?;
    let mut intercepted = b.client.subscribe::<input::DragInterceptedEvent>(s);
    take(b, &p).await?;
    let mut answers = vec![send(b, &p, &mouse(M::MouseMoved, 50.0, 140.0, false)).await, send(b, &p, &mouse(M::MousePressed, 50.0, 140.0, true)).await];
    if step_ms > 0.0 {
        advance(&b.client, &p, step_ms).await?;
    }
    answers.push(send(b, &p, &mouse(M::MouseMoved, 60.0, 140.0, true)).await);
    let data = match intercepted.next_timeout(Duration::from_secs(2)).await? {
        Some(ev) => ev.data,
        None => return Ok(serde_json::json!({"answers": answers, "intercepted": false, "log": take(b, &p).await?})),
    };
    let drag = |t, x, y| input::DispatchDragEventParams::new(t, x, y, data.clone());
    use input::DispatchDragEventParamsType as D;
    for (t, x, y) in [(D::DragEnter, 260.0, 170.0), (D::DragOver, 270.0, 170.0), (D::Drop, 270.0, 170.0)] {
        if step_ms > 0.0 {
            advance(&b.client, &p, step_ms).await?;
        }
        answers.push(send(b, &p, &drag(t, x, y)).await);
    }
    answers.push(send(b, &p, &mouse(M::MouseReleased, 270.0, 170.0, true)).await);
    let frozen_log = take(b, &p).await?;
    advance(&b.client, &p, 50.0).await?;
    let later = take(b, &p).await?;
    Ok(serde_json::json!({"answers": answers, "items": data.items.iter().map(|i| &i.mime_type).collect::<Vec<_>>(), "frozen": frozen_log, "after_advance": later}))
}

/// `DOM.setFileInputFiles` on the file input with two files; one has its modification time set back.
async fn upload(b: &Browser, url: &str) -> anyhow::Result<serde_json::Value> {
    let p = frozen(b, url).await?;
    let s = Some(p.session.as_str());
    let dir = std::env::temp_dir().join("s17-upload");
    std::fs::create_dir_all(&dir)?;
    let (a, z) = (dir.join("a.txt"), dir.join("photo.png"));
    std::fs::write(&a, "hello")?;
    std::fs::write(&z, [0x89u8, b'P', b'N', b'G'])?;
    let old = std::time::UNIX_EPOCH + Duration::from_secs(946_684_800);
    std::fs::File::options().write(true).open(&z)?.set_modified(old)?;
    let doc = b.client.call(s, &dom::GetDocumentParams::default()).await?;
    let node = b.client.call(s, &dom::QuerySelectorParams::new(doc.root.node_id, "#f".into())).await?;
    take(b, &p).await?;
    let answer = send(b, &p, &dom::SetFileInputFilesParams { node_id: Some(node.node_id.clone()), ..dom::SetFileInputFilesParams::new(vec![a.display().to_string(), z.display().to_string()]) }).await;
    let frozen_log = take(b, &p).await?;
    advance(&b.client, &p, 50.0).await?;
    let later = take(b, &p).await?;
    let again = send(b, &p, &dom::SetFileInputFilesParams { node_id: Some(node.node_id.clone()), ..dom::SetFileInputFilesParams::new(vec![a.display().to_string(), z.display().to_string()]) }).await;
    let same_again = take(b, &p).await?;
    Ok(serde_json::json!({"answer": answer, "frozen": frozen_log, "after_advance": later, "again": again, "same_files_again": same_again}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let out = serde_json::json!({
        "pointer_frozen_1": pointer_drag(&b, &url, 1, 0.0).await?,
        "pointer_frozen_5": pointer_drag(&b, &url, 5, 0.0).await?,
        "pointer_stepped_5": pointer_drag(&b, &url, 5, 16.0).await?,
        "native_dnd_frozen": native_dnd(&b, &url, 0.0).await?,
        "native_dnd_stepped": native_dnd(&b, &url, 16.0).await?,
        "intercepted_frozen": intercepted_dnd(&b, &url, 0.0).await?,
        "intercepted_stepped": intercepted_dnd(&b, &url, 16.0).await?,
        "upload": upload(&b, &url).await?,
    });
    report("s17-drag-and-upload", out);
    Ok(())
}
