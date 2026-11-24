//! Controlled popup construction against an opener whose navigation is committing.
mod support;
use cdp::protocol::{page, runtime, storage, target};
use serde_json::{Value, json};

async fn attach(client: &cdp::Client, id: &target::TargetID) -> String {
    let attached = target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(id.clone()) };
    client.call(None, &attached).await.unwrap().session_id.0
}

async fn eval(client: &cdp::Client, session: &str, expression: &str) -> Value {
    let params = runtime::EvaluateParams { return_by_value: Some(true), ..runtime::EvaluateParams::new(expression.into()) };
    let reply = client.call(Some(session), &params).await.unwrap();
    assert!(reply.exception_details.is_none(), "{reply:?}");
    reply.result.value.unwrap_or(Value::Null)
}

/// The opener the browser records for the page: the one native storage metadata names.
async fn browser_opener(client: &cdp::Client, session: &str) -> Option<String> {
    let native = client.call(Some(session), &storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    native.opener_frame_id.map(|id| id.0)
}

/// `Page.navigate` answers once the browser has sent the commit to the renderer, and the browser
/// finishes the commit only after the renderer reports it. A popup requested on that answer arrives
/// while the opener's commit is pending; the renderer may already have replaced the opener's frame.
/// The popup must start with the opener the browser records, and severing it in the page must end
/// it in both.
#[tokio::test]
async fn a_controlled_popup_requested_during_its_openers_commit_has_one_opener() {
    let root = support::repo_root();
    let Some((binary, _)) = browser_pin::engine::patched_binary(&root, browser_pin::current_platform().unwrap()).unwrap() else {
        assert!(!support::controlled_engine_or_check_baseline().await);
        return;
    };
    // Controlled popup construction needs the frozen-frame feature; pages share one renderer as in a
    // generation.
    let args = ["--enable-features=FrontcapFramesWhileFrozen", "--disable-site-isolation-trials", "--renderer-process-limit=1"]
        .map(String::from);
    let browser = browser_pin::launch(&binary, &args, std::time::Duration::from_secs(30)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let addr = support::serve();
    for round in 0..12 {
        let opener = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap().target_id;
        let opener_session = attach(&client, &opener).await;
        let url = format!("http://{addr}/frames_idle.html?round={round}");
        client.call(Some(&opener_session), &page::NavigateParams::new(url)).await.unwrap();
        let popup = client.call(None, &target::CreateTargetParams {
            frontcap_opener_target_id: Some(opener.clone()),
            frontcap_window_name: Some(format!("popup-{round}")),
            frontcap_no_opener: Some(false),
            ..target::CreateTargetParams::new("about:blank".into())
        }).await.unwrap().target_id;
        let popup_session = attach(&client, &popup).await;
        let opener_frame = client.call(Some(&opener_session), &page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id.0;
        assert_eq!(eval(&client, &popup_session, "!!opener").await, json!(true), "round {round}: the popup's renderer has no opener");
        assert_eq!(browser_opener(&client, &popup_session).await.as_deref(), Some(opener_frame.as_str()), "round {round}");
        assert_eq!(eval(&client, &popup_session, "window.opener=null;opener===null").await, json!(true), "round {round}");
        assert_eq!(browser_opener(&client, &popup_session).await, None, "round {round}: the browser kept the severed opener");
        for id in [popup, opener] {
            client.call(None, &target::CloseTargetParams::new(id)).await.unwrap();
        }
    }
}
