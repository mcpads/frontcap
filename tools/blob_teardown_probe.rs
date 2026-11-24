//! Verify actual context destruction after input EOF and before native Blob completion.
mod support;
use cdp::protocol::{emulation, log, network, page, runtime, target};
use frontcap_chromium::session::Session;
use serde_json::{Value, json};
use std::time::Duration;

async fn teardown(url: &str, worker: bool) {
    let (binary, _) = browser_pin::engine::patched_binary(
        &support::repo_root(),
        browser_pin::current_platform().unwrap(),
    )
    .unwrap()
    .expect("build the current patched engine first");
    let browser = browser_pin::launch(
        &binary,
        &[
            "--enable-features=FrontcapVirtualWorkers:blob_finalization_hold_for_testing/true"
                .into(),
        ],
        Duration::from_secs(10),
    )
    .unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let info = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap()
        .target_infos
        .into_iter()
        .find(|t| t.r#type == "page")
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(info.target_id)
            },
        )
        .await
        .unwrap();
    let page = Session::new(client.clone(), attached.session_id.0);
    page.call(&page::EnableParams::default()).await.unwrap();
    let mut loaded = page.subscribe::<page::LoadEventFiredEvent>();
    page.call(&page::NavigateParams::new(url.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), loaded.next())
        .await
        .unwrap()
        .unwrap();
    page.call(&log::EnableParams::default()).await.unwrap();
    let mut phase = page.subscribe::<log::EntryAddedEvent>();
    let mut worker_session = None;
    if worker {
        let mut attached = page.subscribe::<target::AttachedToTargetEvent>();
        page.call(&target::SetAutoAttachParams {
            flatten: Some(true),
            ..target::SetAutoAttachParams::new(true, true)
        })
        .await
        .unwrap();
        page.eval("globalThis.owner=new Worker('/worker');true", None)
            .await
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), attached.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.target_info.r#type, "worker");
        let session = Session::new(client.clone(), ev.session_id.0);
        session.call(&log::EnableParams::default()).await.unwrap();
        phase = session.subscribe::<log::EntryAddedEvent>();
        session
            .call(&network::EnableParams::default())
            .await
            .unwrap();
        session
            .call(&runtime::EnableParams::default())
            .await
            .unwrap();
        let mut ready = session.subscribe::<runtime::ConsoleApiCalledEvent>();
        session
            .call(&runtime::RunIfWaitingForDebuggerParams::default())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = ready.next().await.unwrap();
                if event.args.first().and_then(|arg| arg.value.as_ref())
                    == Some(&json!("blob owner ready"))
                {
                    break;
                }
            }
        })
        .await
        .expect("worker fixture must finish loading before its clock is enabled");
        worker_session = Some(session);
    } else {
        page.eval("(async()=>{globalThis.owner=document.createElement('iframe');const ready=new Promise(resolve=>owner.onload=resolve);owner.src='/child';document.body.append(owner);await ready;return true})()",None).await.unwrap();
    }
    page.call(&emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    })
    .await
    .unwrap();
    if let Some(session) = &worker_session {
        session
            .call(&emulation::SetVirtualTimePolicyParams::new(
                emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
            ))
            .await
            .unwrap();
        session.eval("void begin();true", None).await.unwrap();
    } else {
        page.eval("void owner.contentWindow.begin();true", None)
            .await
            .unwrap();
    }
    let mut exhausted = page.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>();
    page.call(&emulation::SetVirtualTimePolicyParams {
        budget: Some(50.0),
        ..emulation::SetVirtualTimePolicyParams::new(
            emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
        )
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = phase.next().await.unwrap();
            if event.entry.text == "frontcap: blob finalization input complete" {
                break;
            }
        }
    })
    .await
    .expect("must reach the native input-complete/finalization-wait boundary");
    let held = page.eval("Date.now()", None).await.unwrap();
    let owner_times = if let Some(session) = &worker_session {
        session.eval("({startedAt,timerAt})", None).await.unwrap()
    } else {
        page.eval(
            "({startedAt:owner.contentWindow.startedAt,timerAt:owner.contentWindow.timerAt})",
            None,
        )
        .await
        .unwrap()
    };
    println!(
        "BLOB_TEARDOWN_PROBE {}",
        json!({"stage":"held","worker":worker,"held":held,"owner_times":owner_times})
    );
    assert_eq!(held, 946_684_800_010i64);
    assert!(
        exhausted
            .next_timeout(Duration::from_millis(100))
            .await
            .unwrap()
            .is_none(),
        "the fixture must remain held before owner removal"
    );
    let remove = if worker {
        "owner.terminate()"
    } else {
        "owner.remove()"
    };
    page.eval(&format!(r#"{remove};globalThis.outcome=null;globalThis.failure=null;
(async()=>{{const c=await caches.open('probe');const absent=!(await c.match('/pending'));
await c.put('/parent',new Response(new Blob(['parent'])));const value=await(await c.match('/parent')).text();
globalThis.outcome={{absent,value,completedAt:Date.now()}};
setTimeout(()=>globalThis.outcome.timerAt=Date.now(),10);
}})().catch(e=>globalThis.failure=String(e));true"#),None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), exhausted.next())
        .await
        .expect("context destruction must release the finalization wait")
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
        "BLOB_TEARDOWN_PROBE {}",
        json!({"worker":worker,"held_at":held,"observed":observed})
    );
    assert_eq!(observed["failure"], Value::Null, "{observed}");
    assert_eq!(observed["outcome"]["absent"], true, "{observed}");
    assert_eq!(observed["outcome"]["value"], "parent", "{observed}");
    assert_eq!(
        observed["outcome"]["completedAt"], 946_684_800_010i64,
        "{observed}"
    );
    assert_eq!(
        observed["outcome"]["timerAt"], 946_684_800_020i64,
        "{observed}"
    );
    assert_eq!(observed["now"], 946_684_800_050i64, "{observed}");
    drop(worker_session);
}

#[tokio::test]
async fn blob_teardown_probe() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        let owner = r#"globalThis.begin=async()=>{globalThis.startedAt=Date.now();const c=await caches.open('probe');const s=new ReadableStream({start(controller){setTimeout(()=>{globalThis.timerAt=Date.now();controller.enqueue(new TextEncoder().encode('pending'));controller.close()},10)}});await c.put('/pending',new Response(s))};console.log('blob owner ready');"#;
        for request in server.incoming_requests() {
            let (body, mime) = match request.url() {
                "/child" => (
                    format!("<!doctype html><script>{owner}</script>"),
                    "text/html",
                ),
                "/worker" => (owner.to_string(), "text/javascript"),
                _ => (
                    "<!doctype html><title>Blob teardown</title>".to_string(),
                    "text/html",
                ),
            };
            let _ = request.respond(
                tiny_http::Response::from_string(body)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", mime).unwrap()),
            );
        }
    });
    let url = format!("http://{address}/");
    teardown(&url, false).await;
    teardown(&url, true).await;
}
