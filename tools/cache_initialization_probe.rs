//! Opt-in native cache initialization acceptance probe for the storage-control draft.
mod support;
use cdp::protocol::{cache_storage, page, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PutEntries { storage_key: String, cache_name: String, entries: Vec<Value> }
impl cdp::Command for PutEntries {
    const METHOD: &'static str = "CacheStorage.frontcapPutEntries";
    type Returns = Value;
}

#[tokio::test]
async fn cache_initialization_probe() {
    let runs = tempfile::tempdir().unwrap();
    let origin = "https://storage.example.test";
    let url = format!("{origin}/start");
    let rules = RuleSet::from_json(&json!([{"id":"app","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>globalThis.appBooted=true;caches.open('seed').then(c=>c.match('/seed')).then(r=>r.text()).then(v=>globalThis.bootValue=v)</script>"}}]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "cache-initialization", url, rules)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let frame = native.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id;
    let key = native.call(&storage::GetStorageKeyForFrameParams::new(frame)).await.unwrap().storage_key.0;
    let before = g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap();
    let entry = json!({"requestURL":format!("{origin}/seed"),"requestHeaders":[],"responseStatus":200,"responseStatusText":"OK","responseHeaders":[{"name":"content-type","value":"text/plain"}],"body":"c2VlZA=="});
    let mut invalid = entry.clone(); invalid["responseStatus"] = json!(206);
    let rejected = tokio::time::timeout(Duration::from_secs(3), native.call(&PutEntries {
        storage_key: key.clone(), cache_name: "rejected".into(), entries: vec![entry.clone(), invalid],
    })).await.expect("invalid batch must finish while stopped");
    assert!(matches!(rejected, Err(frontcap_chromium::ChromiumError::Cdp(cdp::CdpError::Protocol { code: -32602, .. }))), "{rejected:?}");
    let names = tokio::time::timeout(Duration::from_secs(3), native.call(&cache_storage::RequestCacheNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await.unwrap().unwrap();
    assert!(names.caches.is_empty(), "preflight failure must not create the cache");
    let installed = tokio::time::timeout(Duration::from_secs(3), native.call(&PutEntries {
        storage_key: key, cache_name: "seed".into(), entries: vec![entry],
    })).await.expect("native cache batch must finish while stopped").unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), native.call(&cache_storage::RequestCachedResponseParams::new(
        cache_storage::CacheId(installed["cacheId"].as_str().unwrap().into()), format!("{origin}/seed"), vec![]
    ))).await.expect("body readback must finish while stopped").unwrap();
    assert_eq!(response.response.body.0, "c2VlZA==");
    assert_eq!(g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap(), before);
    assert_eq!(before["booted"], false);
    g.advance(VirtualDuration(100_000)).await.unwrap();
    assert_eq!(g.eval_main("bootValue").await.unwrap(), json!("seed"));
    println!("CACHE_INITIALIZATION_PROBE {}", json!({"preflight_no_cache":true,"native_body":"seed","app_boot_value":"seed","frozen_boundary":before}));
    g.stop().await.unwrap();
}
