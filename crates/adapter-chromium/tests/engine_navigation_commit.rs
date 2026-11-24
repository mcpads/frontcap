//! Browser page commands after a navigation's commit, which the renderer reports before the browser
//! has finished it.
mod support;
use cdp::protocol::{page, target};

async fn attach(client: &cdp::Client, id: &target::TargetID) -> String {
    let attached = target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(id.clone()) };
    client.call(None, &attached).await.unwrap().session_id.0
}

/// `Page.navigate` answers once the browser has handed the commit to the renderer, and the browser
/// finishes the commit only after the renderer reports it. A browser command sent on that answer
/// addresses a frame that is not current yet. Once the navigation's commit has been awaited, the
/// browser's history names the committed document as current.
#[tokio::test]
async fn browser_commands_after_an_awaited_commit_address_the_committed_document() {
    let root = support::repo_root();
    let Some((binary, _)) = browser_pin::engine::patched_binary(&root, browser_pin::current_platform().unwrap()).unwrap() else {
        assert!(!support::controlled_engine_or_check_baseline().await);
        return;
    };
    // Pages share one renderer, as in a generation.
    let args = ["--enable-features=FrontcapFramesWhileFrozen", "--disable-site-isolation-trials", "--renderer-process-limit=1"]
        .map(String::from);
    let browser = browser_pin::launch(&binary, &args, std::time::Duration::from_secs(30)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let addr = support::serve();
    for round in 0..12 {
        let id = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap().target_id;
        let session = attach(&client, &id).await;
        let url = format!("http://{addr}/frames_idle.html?round={round}");
        let navigated = client.call(Some(&session), &page::NavigateParams::new(url.clone())).await.unwrap();
        let loader = navigated.loader_id.expect("a cross-document navigation names its loader");
        client.call(Some(&session), &page::FrontcapAwaitCommitParams::new(loader)).await.unwrap();
        let history = client.call(Some(&session), &page::GetNavigationHistoryParams::default()).await
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        let current = usize::try_from(history.current_index).ok().and_then(|i| history.entries.get(i));
        assert_eq!(current.map(|e| e.url.as_str()), Some(url.as_str()), "round {round}");
        client.call(None, &target::CloseTargetParams::new(id)).await.unwrap();
    }
}
