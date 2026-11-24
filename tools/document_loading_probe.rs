//! Loading snapshots must identify current documents without releasing application work.
mod support;
use cdp::protocol::{emulation, page, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{
    action::Action,
    event::ProgressEvent,
    ids::RequestId,
    rules::{MockSpec, RuleSet},
    time::{VirtualDuration, VirtualTime},
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(serde::Serialize)]
struct States {}
impl cdp::Command for States {
    const METHOD: &'static str = "Page.frontcapGetDocumentLoadingStates";
    type Returns = Value;
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BodyHold {
    frame_id: String,
    loader_id: String,
    hold: bool,
}
impl cdp::Command for BodyHold {
    const METHOD: &'static str = "Page.frontcapSetDocumentBodyHold";
    type Returns = Value;
}
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ParserStop {
    frame_id: String,
    loader_id: String,
    operation: String,
}
impl cdp::Command for ParserStop {
    const METHOD: &'static str = "Page.frontcapArmDocumentParserStop";
    type Returns = Value;
}
#[derive(serde::Serialize)]
#[serde(transparent)]
struct ForgetParserStop(ParserStop);
impl cdp::Command for ForgetParserStop {
    const METHOD: &'static str = "Page.frontcapForgetDocumentParserStop";
    type Returns = Value;
}
async fn attach(client: cdp::Client, id: target::TargetID) -> Session {
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(id)
            },
        )
        .await
        .unwrap();
    Session::new(client, attached.session_id.0)
}
async fn snapshot(native: &Session) -> Value {
    let before = native
        .call(&emulation::GetVirtualTimeParams::default())
        .await
        .unwrap()
        .unix_time_us;
    let first = native.call(&States {}).await.unwrap();
    println!(
        "DOCUMENT_LOADING_SNAPSHOT {}",
        json!({"time":before,"states":first})
    );
    let second = native.call(&States {}).await.unwrap();
    let after = native
        .call(&emulation::GetVirtualTimeParams::default())
        .await
        .unwrap()
        .unix_time_us;
    assert_eq!(
        before, after,
        "reading loading metadata must not grant time"
    );
    assert_eq!(
        first, second,
        "stopped metadata reads must not release the loader or parser"
    );
    first
}
#[tokio::test]
async fn document_loading_probe() {
    let (binary, _) = browser_pin::engine::patched_binary(
        &support::repo_root(),
        browser_pin::current_platform().unwrap(),
    )
    .unwrap()
    .unwrap();
    {
        let browser = browser_pin::launch(&binary, &[], Duration::from_secs(10)).unwrap();
        let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
        let page = client
            .call(None, &target::CreateTargetParams::new("about:blank".into()))
            .await
            .unwrap();
        let native = attach(client, page.target_id).await;
        assert!(
            native
                .call(&States {})
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        assert!(
            native
                .call(&BodyHold {
                    frame_id: "unknown".into(),
                    loader_id: "unknown".into(),
                    hold: true
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        assert!(
            native
                .call(&ParserStop {
                    frame_id: "unknown".into(),
                    loader_id: "unknown".into(),
                    operation: "disabled".into()
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
    }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://loading-probe.test/start";
    let body = r#"<!doctype html><script>globalThis.appBooted=true</script><iframe id="right" src="https://loading-right.test/child"></iframe>"#;
    let rules = RuleSet::from_json(&json!([
        {"id":"app","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
        {"id":"children","match":{"url":"https://loading-*.test/child"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>globalThis.appBooted=true</script><p>child</p>"}}
    ]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(
        runs.path(),
        "document-loading-probe",
        url.into(),
        rules,
    ))
    .await
    .unwrap();
    let endpoint = g.endpoint();
    let mut native = attach(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
        target::TargetID(endpoint.target_id),
    )
    .await;
    native.call(&page::EnableParams::default()).await.unwrap();
    let before = snapshot(&native).await;
    let initial = &before["states"][0];
    assert_eq!(initial["url"], url);
    assert_eq!(initial["phase"], "committed");
    assert_eq!(initial["freezeMode"], "strict");
    assert_eq!(initial["parserReceivedData"], false);
    assert_eq!(initial["parserPhase"], "awaiting_input");
    assert_eq!(g.eval_main("typeof appBooted").await.unwrap(), "undefined");
    let arm = g.arm_event(ProgressEvent::Interception).await.unwrap();
    g.next_event(ProgressEvent::Interception, &arm, VirtualTime(1000))
        .await
        .unwrap();
    let held = g.held();
    assert_eq!(held.len(), 1);
    let mut committed = native.subscribe::<page::FrameNavigatedEvent>();
    g.respond(
        &held[0],
        MockSpec {
            status: 200,
            headers: [("content-type".into(), "text/html".into())].into(),
            body: "<!doctype html><script src=\"https://loading-right.test/parser.js\"></script><script>globalThis.appBooted=true</script><p>right</p>".into(),
            body_base64: None,
            latency_ms: 0,
        },
    )
    .await
    .unwrap();
    let right_frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = committed.next().await.unwrap();
            if event.frame.url == "https://loading-right.test/child" {
                break event.frame.id;
            }
        }
    })
    .await
    .unwrap();
    let child_frozen = snapshot(&native).await;
    let child = child_frozen["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["frameId"] == right_frame.0)
        .unwrap();
    assert_eq!(child["phase"], "committed");
    assert_eq!(child["freezeMode"], "strict");
    assert_eq!(child["hasBodyLoader"], true);
    assert_eq!(child["parserReceivedData"], false);
    let child_loader = child["loaderId"].as_str().unwrap().to_string();
    assert!(
        native
            .call(&BodyHold {
                frame_id: right_frame.0.clone(),
                loader_id: "stale-loader".into(),
                hold: true
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("no longer current")
    );
    native
        .call(&BodyHold {
            frame_id: right_frame.0.clone(),
            loader_id: child_loader.clone(),
            hold: true,
        })
        .await
        .unwrap();
    g.eval_main(
        "globalThis.holdProgress=false; setTimeout(()=>globalThis.holdProgress=true,0); void 0",
    )
    .await
    .unwrap();
    let held_at = native
        .call(&emulation::GetVirtualTimeParams::default())
        .await
        .unwrap()
        .unix_time_us;
    native
        .call(&emulation::SetVirtualTimePolicyParams {
            budget: Some(0.0),
            ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while g.eval_main("holdProgress").await.unwrap() != true {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    native
        .call(&emulation::SetVirtualTimePolicyParams::new(
            emulation::VirtualTimePolicy::Pause,
        ))
        .await
        .unwrap();
    assert_eq!(
        native
            .call(&emulation::GetVirtualTimeParams::default())
            .await
            .unwrap()
            .unix_time_us,
        held_at
    );
    let body_held = snapshot(&native).await;
    let retained = body_held["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["frameId"] == right_frame.0)
        .unwrap();
    assert_eq!(retained["bodyHeld"], true);
    assert_eq!(
        retained["parserReceivedData"], false,
        "another document's ready task must not release this body"
    );
    native.client.close();
    native = attach(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
        target::TargetID(g.endpoint().target_id),
    )
    .await;
    let adopted = snapshot(&native).await;
    assert_eq!(
        adopted, body_held,
        "document holds must survive controller replacement"
    );
    native
        .call(&BodyHold {
            frame_id: right_frame.0.clone(),
            loader_id: child_loader.clone(),
            hold: false,
        })
        .await
        .unwrap();
    let released = snapshot(&native).await;
    let pending = released["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["frameId"] == right_frame.0)
        .unwrap();
    assert_eq!(pending["bodyHeld"], false);
    assert_eq!(
        pending["parserReceivedData"], false,
        "release requires a separate explicit progress grant"
    );
    let parser_stop = ParserStop {
        frame_id: right_frame.0.clone(),
        loader_id: child_loader.clone(),
        operation: "first-body-pump".into(),
    };
    native.call(&parser_stop).await.unwrap();
    native.call(&parser_stop).await.unwrap();
    assert!(
        native
            .call(&ForgetParserStop(parser_stop.clone()))
            .await
            .unwrap_err()
            .to_string()
            .contains("no terminal parser stop")
    );
    let conflicting = ParserStop {
        operation: "conflicting-operation".into(),
        ..parser_stop.clone()
    };
    assert!(
        native
            .call(&conflicting)
            .await
            .unwrap_err()
            .to_string()
            .contains("previous parser stop")
    );
    g.eval_main("globalThis.afterParserStop=false; setTimeout(()=>globalThis.afterParserStop=true,1); void 0").await.unwrap();
    let parser_at = native
        .call(&emulation::GetVirtualTimeParams::default())
        .await
        .unwrap()
        .unix_time_us;
    native
        .call(&emulation::SetVirtualTimePolicyParams {
            budget: Some(1.0),
            ..emulation::SetVirtualTimePolicyParams::new(
                emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
            )
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let states = native.call(&States {}).await.unwrap();
            if states["states"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["frameId"] == right_frame.0 && v["parserStopState"] == "pump_returned")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        native
            .call(&emulation::GetVirtualTimeParams::default())
            .await
            .unwrap()
            .unix_time_us,
        parser_at,
        "native parser stop must precede the remaining grant"
    );
    assert_eq!(
        g.eval_main("afterParserStop").await.unwrap(),
        false,
        "the next timer must remain pending"
    );
    assert!(
        native
            .call(&ForgetParserStop(conflicting.clone()))
            .await
            .unwrap_err()
            .to_string()
            .contains("no terminal parser stop")
    );
    let parser_stopped = snapshot(&native).await;
    native.client.close();
    native = attach(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
        target::TargetID(g.endpoint().target_id),
    )
    .await;
    assert_eq!(
        snapshot(&native).await,
        parser_stopped,
        "parser receipt must survive controller replacement"
    );
    native.call(&parser_stop).await.unwrap();
    native
        .call(&ForgetParserStop(parser_stop.clone()))
        .await
        .unwrap();
    // A parser-blocking dependency must remain serviceable. Commit/body metadata alone
    // must not be interpreted as completed parsing or a safe whole-load admission fence.
    let arm = g.arm_event(ProgressEvent::Interception).await.unwrap();
    g.next_event(ProgressEvent::Interception, &arm, VirtualTime(1000))
        .await
        .unwrap();
    let dependencies = g.held();
    assert_eq!(dependencies.len(), 1);
    let parser_waiting = snapshot(&native).await;
    let waiting_child = parser_waiting["states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["frameId"] == right_frame.0)
        .unwrap();
    assert_eq!(waiting_child["parserReceivedData"], true);
    assert_eq!(waiting_child["parserPhase"], "waiting_script");
    assert_eq!(waiting_child["parserWaitingForScripts"], true);
    assert!(
        native
            .call(&BodyHold {
                frame_id: right_frame.0.clone(),
                loader_id: child_loader.clone(),
                hold: true
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("not frozen before first input")
    );
    assert_eq!(waiting_child["documentFinishedParsing"], false);
    assert_eq!(waiting_child["loadEventFinished"], false);
    let context = native
        .call(&page::CreateIsolatedWorldParams::new(right_frame.clone()))
        .await
        .unwrap()
        .execution_context_id;
    assert_eq!(
        native
            .eval(
                "document.querySelector('p') === null",
                Some(context.clone())
            )
            .await
            .unwrap(),
        true,
        "the parser must not pass its held blocking script"
    );
    g.respond(
        &dependencies[0],
        MockSpec {
            status: 200,
            headers: [("content-type".into(), "application/javascript".into())].into(),
            body: "globalThis.dependencyLoaded=true".into(),
            body_base64: None,
            latency_ms: 0,
        },
    )
    .await
    .unwrap();
    let resume_time = native
        .call(&emulation::GetVirtualTimeParams::default())
        .await
        .unwrap()
        .unix_time_us;
    g.advance(VirtualDuration::from_micros(0)).await.unwrap();
    assert_eq!(
        native
            .call(&emulation::GetVirtualTimeParams::default())
            .await
            .unwrap()
            .unix_time_us,
        resume_time,
        "servicing the answered parser dependency must not advance time"
    );
    let loaded = snapshot(&native).await;
    assert_eq!(loaded["states"].as_array().unwrap().len(), 2);
    assert!(
        loaded["states"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["documentFinishedParsing"] == true && s["loadEventFinished"] == true)
    );
    assert_eq!(g.eval_main("appBooted").await.unwrap(), true);
    assert_eq!(
        native
            .eval("document.querySelector('p').textContent", Some(context))
            .await
            .unwrap(),
        "right"
    );
    g.eval_main("document.querySelector('#right').remove(); void 0")
        .await
        .unwrap();
    assert!(
        native
            .call(&BodyHold {
                frame_id: right_frame.0.clone(),
                loader_id: child_loader.clone(),
                hold: false
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("no longer current")
    );
    let removed = snapshot(&native).await;
    assert_eq!(removed["states"].as_array().unwrap().len(), 1);
    assert!(
        removed["states"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["url"] != "https://loading-right.test/child")
    );
    g.navigate_action(&RequestId::new("reload").unwrap(), &Action::Reload {})
        .await
        .unwrap();
    let reloaded = snapshot(&native).await;
    assert_eq!(reloaded["states"].as_array().unwrap().len(), 1);
    assert_ne!(reloaded["states"][0]["loaderId"], initial["loaderId"]);
    assert_eq!(reloaded["states"][0]["freezeMode"], "strict");
    assert_eq!(reloaded["states"][0]["parserReceivedData"], false);
    assert_eq!(g.eval_main("typeof appBooted").await.unwrap(), "undefined");
    println!(
        "DOCUMENT_LOADING_PROBE {}",
        json!({"before":before,"child_frozen":child_frozen,"body_held":body_held,"released":released,"parser_stopped":parser_stopped,"parser_waiting":parser_waiting,"loaded":loaded,"removed":removed,"reloaded":reloaded,"feature_disabled":"refused"})
    );
    g.stop().await.unwrap();
}

#[tokio::test]
async fn parser_stylesheet_dependency_and_replaced_owner_are_distinct() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://parser-state.test/start";
    let body = "<!doctype html><link rel=stylesheet href='/gate.css'><script>globalThis.afterStyle=true</script><p>ready</p>";
    let rules = RuleSet::from_json(&json!([
        {"id":"app","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}
    ]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(
        runs.path(),
        "parser-state-probe",
        url.into(),
        rules,
    ))
    .await
    .unwrap();
    let endpoint = g.endpoint();
    let native = attach(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
        target::TargetID(endpoint.target_id),
    )
    .await;
    let arm = g.arm_event(ProgressEvent::Interception).await.unwrap();
    g.next_event(ProgressEvent::Interception, &arm, VirtualTime(1000))
        .await
        .unwrap();
    let waiting = snapshot(&native).await;
    let state = &waiting["states"][0];
    assert_eq!(state["parserPhase"], "waiting_stylesheet");
    assert_eq!(state["parserWaitingForStylesheets"], true);
    assert_eq!(g.eval_main("typeof afterStyle").await.unwrap(), "undefined");
    let held = g.held();
    assert_eq!(held.len(), 1);
    g.respond(
        &held[0],
        MockSpec {
            status: 200,
            headers: [("content-type".into(), "text/css".into())].into(),
            body: "p{color:green}".into(),
            body_base64: None,
            latency_ms: 0,
        },
    )
    .await
    .unwrap();
    g.advance(VirtualDuration::from_micros(0)).await.unwrap();
    let finished = snapshot(&native).await;
    assert_eq!(finished["states"][0]["parserPhase"], "finished");
    assert_eq!(g.eval_main("afterStyle").await.unwrap(), true);
    g.eval_main("document.open(); void 0").await.unwrap();
    let opened = snapshot(&native).await;
    let op = ParserStop {
        frame_id: opened["states"][0]["frameId"].as_str().unwrap().into(),
        loader_id: opened["states"][0]["loaderId"].as_str().unwrap().into(),
        operation: "replaced-parser".into(),
    };
    native.call(&op).await.unwrap();
    g.eval_main("document.open(); void 0").await.unwrap();
    let replaced = snapshot(&native).await;
    assert_eq!(
        replaced["states"][0]["loaderId"],
        opened["states"][0]["loaderId"]
    );
    assert_eq!(replaced["states"][0]["parserStopState"], "owner_lost");
    assert!(
        native
            .call(&op)
            .await
            .unwrap_err()
            .to_string()
            .contains("owner was lost")
    );
    native.call(&ForgetParserStop(op)).await.unwrap();
    assert_eq!(
        snapshot(&native).await["states"][0]["parserStopState"],
        "none"
    );
    g.navigate_action(
        &RequestId::new("restore-document").unwrap(),
        &Action::Reload {},
    )
    .await
    .unwrap();
    g.stop().await.unwrap();
    println!(
        "DOCUMENT_LOADING_PROBE {}",
        json!({"stylesheet_waiting":waiting,"finished":finished,"replaced_owner":replaced})
    );
}

#[tokio::test]
async fn non_html_tokenizer_refuses_parser_stop() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://parser-state.test/document.xml";
    let rules = RuleSet::from_json(&json!([
        {"id":"xml","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"application/xml"},"body":"<root><item>xml</item></root>"}}
    ]).to_string()).unwrap();
    let g = Generation::launch(support::options(
        runs.path(),
        "xml-parser-state",
        url.into(),
        rules,
    ))
    .await
    .unwrap();
    let endpoint = g.endpoint();
    let native = attach(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
        target::TargetID(endpoint.target_id),
    )
    .await;
    let states = snapshot(&native).await;
    let state = &states["states"][0];
    assert_eq!(state["parserPhase"], "unsupported");
    let op = ParserStop {
        frame_id: state["frameId"].as_str().unwrap().into(),
        loader_id: state["loaderId"].as_str().unwrap().into(),
        operation: "xml-is-not-html".into(),
    };
    assert!(
        native
            .call(&op)
            .await
            .unwrap_err()
            .to_string()
            .contains("no active HTML parser")
    );
    g.stop().await.unwrap();
    println!(
        "DOCUMENT_LOADING_PROBE {}",
        json!({"unsupported_parser":states})
    );
}
