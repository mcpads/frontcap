mod sse;

use cdp::protocol::{emulation::VirtualTimePolicy, page, runtime};
use spike_common::proxy::Proxy;
use spike_common::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let page_addr = serve_dir(&fixtures)?;
    let sse_port = sse::start().await?;
    let ws_port = sse::start_ws().await?;
    let proxy = Proxy::start("127.0.0.1:0").await?;
    let args = vec![
        format!("--proxy-server=http://{}", proxy.addr()),
        "--proxy-bypass-list=<-loopback>".to_string(),
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
    ];
    let browser = Browser::start(&args).await?;
    let c = &browser.client;

    // Routing.
    let p = browser.new_page(false).await?;
    navigate(c, &p, &format!("http://{page_addr}/index.html")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    eval(c, &p, &format!("probe({sse_port})")).await?;
    eval(c, &p, &format!("probeWs({ws_port})")).await?;
    let routed = proxy.log().into_iter().map(|e| format!("{} {}", e.kind, e.target)).collect::<Vec<_>>();
    let probe_log = eval(c, &p, "JSON.stringify(window.__log)").await?;

    // Delivery barrier: instrument EventSource before page script, hold the stream at the proxy, release,
    // and observe when the callback runs relative to virtual time.
    let p2 = browser.new_page(false).await?;
    c.call(Some(&p2.session), &runtime::AddBindingParams::new("__frontcapDelivered".into())).await?;
    c.call(Some(&p2.session), &page::AddScriptToEvaluateOnNewDocumentParams::new(r#"
        (() => {
          const ES = window.EventSource;
          window.EventSource = function (...a) {
            const es = new ES(...a);
            es.addEventListener("message", (e) => __frontcapDelivered(JSON.stringify([e.data, performance.now()])), true);
            return es;
          };
          window.EventSource.prototype = ES.prototype;
        })();
    "#.into())).await?;
    let mut delivered = c.subscribe::<runtime::BindingCalledEvent>(Some(&p2.session));
    navigate(c, &p2, &format!("http://{page_addr}/index.html")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    set_policy(c, &p2, VirtualTimePolicy::Pause, None).await?;
    proxy.hold("/stream");
    eval(c, &p2, &format!("openSse('http://127.0.0.1:{sse_port}/stream')")).await?;
    // Injected disturbance: messages accumulate upstream while held.
    tokio::time::sleep(Duration::from_millis(600)).await;
    proxy.release("/stream");
    let mut while_frozen = Vec::new();
    while let Some(ev) = delivered.next_timeout(Duration::from_millis(500)).await? {
        while_frozen.push(ev.payload);
    }
    // Slot delivery: grant a zero-length budget at the frozen instant and see whether queued messages run
    // without virtual time moving.
    eprintln!("[s6] let t_before_zero");
    let t_before_zero = eval(c, &p2, "performance.now()").await?;
    eprintln!("[s6] let zero =");
    let zero = tokio::time::timeout(Duration::from_secs(5), advance(c, &p2, 0.0)).await;
    eprintln!("[s6] zero returned: {:?}", zero.as_ref().map(|r| r.is_ok()));
    let mut after_zero = Vec::new();
    while let Some(ev) = delivered.next_timeout(Duration::from_millis(300)).await? {
        eprintln!("[s6] zero delivered {}", ev.payload);
        after_zero.push(ev.payload);
        if after_zero.len() > 50 { break; }
    }
    eprintln!("[s6] let t_after_zero");
    let t_after_zero = eval(c, &p2, "performance.now()").await?;
    eprintln!("[s6] advance(c, &p2, 100.0).await?;");
    advance(c, &p2, 100.0).await?;
    let mut after_advance = Vec::new();
    while let Some(ev) = delivered.next_timeout(Duration::from_millis(300)).await? {
        after_advance.push(ev.payload);
    }

    eprintln!("[s6] // Proxy down");
    // Proxy down: requests must fail rather than go direct.
    proxy.stop();
    let p3 = browser.new_page(false).await?;
    navigate(c, &p3, &format!("http://{page_addr}/index.html")).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let ready_without_proxy = eval(c, &p3, "JSON.stringify({href: location.href, title: document.title, probe: typeof window.probe})").await.ok();

    eprintln!("[s6] // Reattach");
    // Reattach: second client on the same browser endpoint.
    let second = cdp::Client::connect(&browser.launched.browser_ws_url).await;

    report("s6-transport", serde_json::json!({
        "proxy_log": routed, "probe_log": probe_log,
        "delivered_while_frozen": while_frozen, "delivered_after_advance": after_advance,
        "zero_budget": { "t_before": t_before_zero, "t_after": t_after_zero, "returned": zero.is_ok(), "delivered": after_zero },
        "page_ready_without_proxy": ready_without_proxy,
        "second_client_connects": second.is_ok(),
    }));
    Ok(())
}
