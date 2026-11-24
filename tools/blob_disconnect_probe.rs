//! Opt-in native failure injection; not part of the default product test suite.
mod support;
use cdp::protocol::{emulation, network, page, target};
use frontcap_chromium::session::Session;
use serde_json::{Value, json};
use std::time::Duration;

async fn scenario(url: &str, inject: bool) -> Value {
    let (binary, _) = browser_pin::engine::patched_binary(
        &support::repo_root(),
        browser_pin::current_platform().unwrap(),
    )
    .unwrap()
    .expect("build the current patched engine first");
    let features = if inject {
        "--enable-features=FrontcapVirtualWorkers:blob_registry_disconnect_for_testing/true"
    } else {
        "--enable-features=FrontcapVirtualWorkers"
    };
    let browser =
        browser_pin::launch(&binary, &[features.into()], Duration::from_secs(10)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let target = targets
        .target_infos
        .into_iter()
        .find(|t| t.r#type == "page")
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target.target_id)
            },
        )
        .await
        .unwrap();
    let page = Session::new(client, attached.session_id.0);
    page.call(&page::EnableParams::default()).await.unwrap();
    let mut loaded = page.subscribe::<page::LoadEventFiredEvent>();
    page.call(&page::NavigateParams::new(url.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), loaded.next())
        .await
        .unwrap()
        .unwrap();
    page.call(&emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    })
    .await
    .unwrap();
    page.eval(r#"globalThis.outcome=null;globalThis.failure=null;
(async()=>{
 const start=Date.now();const c=await caches.open('probe');
 const streamed=(text,delay)=>new Response(new ReadableStream({start(controller){setTimeout(()=>{controller.enqueue(new TextEncoder().encode(text));controller.close()},delay)}}));
 let rejected=false,error=null;
 try{await c.put('/first',streamed('first',25))}catch(e){rejected=true;error=String(e)}
 const firstAt=Date.now()-start;
 const first=await c.match('/first');
 await c.put('/second',streamed('second',10));
 const second=await(await c.match('/second')).text();
 globalThis.outcome={rejected,error,firstAt,first:first?await first.text():null,second,secondAt:Date.now()-start};
 setTimeout(()=>globalThis.outcome.timerAt=Date.now()-start,10);
})().catch(e=>globalThis.failure=String(e));true"#, None).await.unwrap();
    let mut exhausted = page.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>();
    page.call(&emulation::SetVirtualTimePolicyParams {
        budget: Some(50.0),
        ..emulation::SetVirtualTimePolicyParams::new(
            emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
        )
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), exhausted.next())
        .await
        .expect("a dropped reply must not retain a virtual-time wait")
        .unwrap();
    page.call(&emulation::SetVirtualTimePolicyParams::new(
        emulation::VirtualTimePolicy::Pause,
    ))
    .await
    .unwrap();
    let observed = page
        .eval("({outcome,failure,now:Date.now()})", None)
        .await
        .unwrap();
    println!(
        "BLOB_DISCONNECT_PROBE {}",
        json!({"injected":inject,"observed":observed})
    );
    assert_eq!(observed["failure"], Value::Null, "{observed}");
    assert_eq!(observed["outcome"]["rejected"], inject, "{observed}");
    assert_eq!(observed["outcome"]["firstAt"], 25, "{observed}");
    assert_eq!(
        observed["outcome"]["first"],
        if inject { Value::Null } else { json!("first") }
    );
    assert_eq!(observed["outcome"]["second"], "second", "{observed}");
    assert_eq!(observed["outcome"]["secondAt"], 35, "{observed}");
    assert_eq!(observed["outcome"]["timerAt"], 45, "{observed}");
    assert_eq!(observed["now"], 946_684_800_050i64, "{observed}");
    observed
}

#[tokio::test]
async fn blob_disconnect_probe() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ = request.respond(
                tiny_http::Response::from_string("<!doctype html><title>Blob reply probe</title>")
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap(),
                    ),
            );
        }
    });
    let url = format!("http://{address}/");
    scenario(&url, false).await;
    scenario(&url, true).await;
}
