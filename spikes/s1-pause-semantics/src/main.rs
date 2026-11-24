use cdp::protocol::{debugger, dom, emulation::VirtualTimePolicy, input, page};
mod atomic;

use spike_common::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let c = &browser.client;

    // Part A: what runs while paused from before navigation.
    let p = browser.new_page(false).await?;
    set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    // Injected disturbance: 1 s of wall time while virtual time is paused.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let while_paused = eval(c, &p, "JSON.stringify({log: window.__log, ready: document.readyState})").await?;
    advance(c, &p, 200.0).await?;
    let after_advance = eval(c, &p, "JSON.stringify(window.__log)").await?;

    // Part B: atomic dispatch candidates against the mutator.
    let mut results = serde_json::Map::new();
    // Control: is the mutator actually swapping when virtual time is not paused?
    let control = {
        let p = browser.new_page(false).await?;
        navigate(c, &p, &format!("http://{addr}/mutator.html")).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let g0 = eval(c, &p, "window.__gen()").await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let g1 = eval(c, &p, "window.__gen()").await?;
        serde_json::json!({ "gen_before": g0, "gen_after_50ms_wall": g1 })
    };
    // Frozen = pause + lifecycle freeze: do observations still work, and does rAF stop?
    let frozen_observation = {
        let p = browser.new_page(false).await?;
        navigate(c, &p, &format!("http://{addr}/mutator.html?raf")).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
        let paused_g0 = eval(c, &p, "window.__gen()").await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let paused_g1 = eval(c, &p, "window.__gen()").await?;
        c.call(Some(&p.session), &page::SetWebLifecycleStateParams::new(page::SetWebLifecycleStateParamsState::Frozen)).await?;
        let frozen_g0 = tokio::time::timeout(Duration::from_secs(3), eval(c, &p, "window.__gen()")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let frozen_g1 = tokio::time::timeout(Duration::from_secs(3), eval(c, &p, "window.__gen()")).await;
        let snapshot = tokio::time::timeout(Duration::from_secs(3), c.call(Some(&p.session), &dom::GetDocumentParams::default())).await;
        serde_json::json!({
            "paused_only": [paused_g0, paused_g1],
            "paused_and_frozen": [frozen_g0.ok().and_then(|r| r.ok()), frozen_g1.ok().and_then(|r| r.ok())],
            "dom_get_document_under_freeze": snapshot.map(|r| r.is_ok()).unwrap_or(false),
        })
    };
    for mechanism in ["none", "lifecycle-freeze", "debugger-pause", "none-raf-mutator", "lifecycle-freeze-raf-mutator"] {
        let mechanism_key = mechanism;
        let outcome: anyhow::Result<serde_json::Value> = async {
        let p = browser.new_page(false).await?;
        let query = if mechanism.ends_with("-raf-mutator") { "?raf" } else { "" };
        navigate(c, &p, &format!("http://{addr}/mutator.html{query}")).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
        let mechanism = mechanism.trim_end_matches("-raf-mutator");
        if mechanism == "debugger-pause" {
            c.call(Some(&p.session), &debugger::EnableParams::default()).await?;
        }
        let mut gen_changed = 0;
        for _ in 0..200 {
            let g0 = eval(c, &p, "window.__gen()").await?;
            match mechanism {
                "lifecycle-freeze" => {
                    c.call(Some(&p.session), &page::SetWebLifecycleStateParams::new(
                        page::SetWebLifecycleStateParamsState::Frozen)).await?;
                }
                "debugger-pause" => {
                    c.call(Some(&p.session), &debugger::PauseParams::default()).await?;
                }
                _ => {}
            }
            // The final hit-test the recorder would run before dispatch.
            c.call(Some(&p.session), &dom::GetNodeForLocationParams::new(70, 30)).await?;
            let pressed = input::DispatchMouseEventParams {
                button: Some(input::MouseButton::Left),
                click_count: Some(1),
                ..input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MousePressed, 70.0, 30.0)
            };
            let released = input::DispatchMouseEventParams {
                button: Some(input::MouseButton::Left),
                click_count: Some(1),
                ..input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MouseReleased, 70.0, 30.0)
            };
            let dispatch = async {
                c.call(Some(&p.session), &pressed).await?;
                c.call(Some(&p.session), &released).await
            };
            match mechanism {
                "lifecycle-freeze" => {
                    // Release the freeze only after dispatch is queued; record whether dispatch blocks.
                    let active = page::SetWebLifecycleStateParams::new(page::SetWebLifecycleStateParamsState::Active);
                    let (d, _) = tokio::join!(
                        tokio::time::timeout(Duration::from_secs(2), dispatch),
                        c.call(Some(&p.session), &active)
                    );
                    d.map_err(|_| anyhow::anyhow!("dispatch blocked under freeze"))??;
                }
                "debugger-pause" => {
                    let resume = debugger::ResumeParams::default();
                    let (d, _) = tokio::join!(
                        tokio::time::timeout(Duration::from_secs(2), dispatch),
                        c.call(Some(&p.session), &resume)
                    );
                    d.map_err(|_| anyhow::anyhow!("dispatch blocked under debugger pause"))??;
                }
                _ => {
                    dispatch.await?;
                }
            }
            let g1 = eval(c, &p, "window.__gen()").await?;
            if g0 != g1 {
                gen_changed += 1;
            }
        }
        let hits = eval(c, &p, "JSON.stringify(window.__hits)").await?;
        Ok(serde_json::json!({ "hits": hits, "iterations_with_generation_change": gen_changed }))
        }.await;
        results.insert(mechanism_key.into(), match outcome {
            Ok(v) => v,
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        });
    }

    let atomic_followup = atomic::run(&browser, addr).await?;
    report("s1-pause-semantics", serde_json::json!({
        "atomic_followup": atomic_followup,
        "while_paused": while_paused,
        "after_advance_200ms": after_advance,
        "dispatch": results,
        "mutator_control_unpaused": control,
        "frozen_observation": frozen_observation,
    }));
    Ok(())
}
