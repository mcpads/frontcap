use cdp::protocol::{fetch, input, page, runtime};
use spike_common::*;
use std::time::Duration;

const KEEPER_LEDGER: &str = r#"
  (() => {
    let seq = 0;
    for (const type of ["beforeinput", "input", "keydown", "mousedown", "mouseup", "click"]) {
      document.addEventListener(type, (e) => {
        fetch("http://127.0.0.1:KEEPER_PORT/receipt", { method: "POST", keepalive: true, mode: "no-cors",
          body: JSON.stringify([++seq, type, location.pathname]) });
      }, true);
    }
  })();
"#;

fn start_keeper() -> anyhow::Result<(u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>)> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let port = server.server_addr().to_ip().unwrap().port();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let mut body = String::new();
            let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
            g.lock().unwrap().push(body);
            let _ = req.respond(tiny_http::Response::from_string("ok"));
        }
    });
    Ok((port, got))
}

const LEDGER: &str = r#"
  (() => {
    window.__receipts = [];
    for (const type of ["keydown", "mousedown", "mouseup", "click"]) {
      document.addEventListener(type, (e) => window.__receipts.push([globalThis.__mark || null, type]), true);
    }
  })();
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;

    c.call(Some(&p.session), &page::AddScriptToEvaluateOnNewDocumentParams {
        world_name: Some("frontcap".into()),
        ..page::AddScriptToEvaluateOnNewDocumentParams::new(LEDGER.into())
    }).await?;
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(Some(&p.session));
    c.call(Some(&p.session), &fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*held.json".into()), request_stage: Some(fetch::RequestStage::Request), resource_type: None }]),
        ..fetch::EnableParams::default()
    }).await?;
    navigate(c, &p, &format!("http://{addr}/form.html")).await?;
    let held = paused.next_timeout(Duration::from_secs(5)).await?.map(|e| e.request.url);

    // Type two of five characters, then click the navigating button, then drop the first session.
    for ch in ["a", "b"] {
        c.call(Some(&p.session), &input::InsertTextParams::new(ch.into())).await?;
    }
    let click = |kind| input::DispatchMouseEventParams {
        button: Some(input::MouseButton::Left), click_count: Some(1),
        ..input::DispatchMouseEventParams::new(kind, 100.0, 75.0)
    };
    c.call(Some(&p.session), &click(input::DispatchMouseEventParamsType::MousePressed)).await?;
    c.call(Some(&p.session), &click(input::DispatchMouseEventParamsType::MouseReleased)).await?;

    // "Recorder dies": detach the session without cleanup.
    c.call(None, &cdp::protocol::target::DetachFromTargetParams {
        session_id: Some(cdp::protocol::target::SessionID(p.session.clone())),
        ..cdp::protocol::target::DetachFromTargetParams::default()
    }).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Second client attaches to the same target and inspects survivors.
    let second = cdp::Client::connect(&browser.launched.browser_ws_url).await?;
    let attached = second.call(None, &cdp::protocol::target::AttachToTargetParams {
        flatten: Some(true),
        ..cdp::protocol::target::AttachToTargetParams::new(cdp::protocol::target::TargetID(p.target_id.clone()))
    }).await?;
    let s2 = Page { session: attached.session_id.0, target_id: p.target_id.clone() };
    second.call(Some(&s2.session), &runtime::EnableParams::default()).await?;
    let location = eval(&second, &s2, "location.pathname").await?;
    let held_outcome = eval(&second, &s2, "String(window.__held)").await.ok();
    let world = second.call(Some(&s2.session), &page::CreateIsolatedWorldParams {
        world_name: Some("frontcap".into()),
        ..page::CreateIsolatedWorldParams::new(page::FrameId(p.target_id.clone()))
    }).await;
    let receipts = match world {
        Ok(w) => second.call(Some(&s2.session), &runtime::EvaluateParams {
            context_id: Some(w.execution_context_id),
            return_by_value: Some(true),
            ..runtime::EvaluateParams::new("JSON.stringify(window.__receipts || null)".into())
        }).await.ok().and_then(|r| r.result.value),
        Err(_) => None,
    };

    // Scenario B: keeper candidate — receipts sent out of the page as they happen.
    let (keeper_port, keeper) = start_keeper()?;
    let pb = browser.new_page(false).await?;
    c.call(Some(&pb.session), &page::AddScriptToEvaluateOnNewDocumentParams {
        world_name: Some("frontcap-keeper".into()),
        ..page::AddScriptToEvaluateOnNewDocumentParams::new(KEEPER_LEDGER.replace("KEEPER_PORT", &keeper_port.to_string()))
    }).await?;
    navigate(c, &pb, &format!("http://{addr}/form.html")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for ch in ["a", "b"] {
        c.call(Some(&pb.session), &input::InsertTextParams::new(ch.into())).await?;
    }
    c.call(Some(&pb.session), &click(input::DispatchMouseEventParamsType::MousePressed)).await?;
    c.call(Some(&pb.session), &click(input::DispatchMouseEventParamsType::MouseReleased)).await?;
    c.call(None, &cdp::protocol::target::DetachFromTargetParams {
        session_id: Some(cdp::protocol::target::SessionID(pb.session.clone())),
        ..cdp::protocol::target::DetachFromTargetParams::default()
    }).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let keeper_receipts = keeper.lock().unwrap().clone();

    // Scenario C: a request held by Fetch when its session detaches, without navigation.
    let pc = browser.new_page(false).await?;
    let mut paused_c = c.subscribe::<fetch::RequestPausedEvent>(Some(&pc.session));
    c.call(Some(&pc.session), &fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*held.json".into()), request_stage: Some(fetch::RequestStage::Request), resource_type: None }]),
        ..fetch::EnableParams::default()
    }).await?;
    navigate(c, &pc, &format!("http://{addr}/hold.html")).await?;
    let held_c = paused_c.next_timeout(Duration::from_secs(5)).await?.is_some();
    c.call(None, &cdp::protocol::target::DetachFromTargetParams {
        session_id: Some(cdp::protocol::target::SessionID(pc.session.clone())),
        ..cdp::protocol::target::DetachFromTargetParams::default()
    }).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let att = second.call(None, &cdp::protocol::target::AttachToTargetParams {
        flatten: Some(true),
        ..cdp::protocol::target::AttachToTargetParams::new(cdp::protocol::target::TargetID(pc.target_id.clone()))
    }).await?;
    let sc = Page { session: att.session_id.0, target_id: pc.target_id.clone() };
    second.call(Some(&sc.session), &runtime::EnableParams::default()).await?;
    let held_fate = eval(&second, &sc, "String(window.__held)").await.ok();

    report("s7-recovery", serde_json::json!({
        "keeper_receipts": keeper_receipts,
        "held_paused_before_detach": held_c, "held_fate_after_detach": held_fate,
        "held_request": held, "held_outcome_after_detach": held_outcome,
        "location_after_detach": location, "receipts_in_new_world": receipts,
    }));
    Ok(())
}
