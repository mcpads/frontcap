//! Opt-in observations of native and isolated-world storage work before app startup.
mod support;
use cdp::protocol::{cache_storage, emulation, indexed_db, page, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn storage_barrier_probe() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let origin = "https://storage.example.test";
    let url = format!("{origin}/start");
    let rules = RuleSet::from_json(&json!([{"id":"app","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>globalThis.appBooted=true</script><p>app</p>"}}]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "storage-barrier", url, rules)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let frame = native.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id;
    let key = native.call(&storage::GetStorageKeyForFrameParams::new(frame.clone())).await.unwrap().storage_key.0;
    let before = g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap();
    let databases = tokio::time::timeout(Duration::from_secs(2), native.call(&indexed_db::RequestDatabaseNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    let caches = tokio::time::timeout(Duration::from_secs(2), native.call(&cache_storage::RequestCacheNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    let isolated = native.call(&page::CreateIsolatedWorldParams {
        world_name: Some("frontcap-storage-diagnostic".into()), ..page::CreateIsolatedWorldParams::new(frame)
    }).await.unwrap().execution_context_id;
    let idb_write = tokio::time::timeout(Duration::from_secs(2), native.eval(
        "new Promise((resolve,reject)=>{const r=indexedDB.open('probe',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');r.onerror=()=>reject(String(r.error));r.onsuccess=()=>{const db=r.result;const t=db.transaction('items','readwrite');t.objectStore('items').put({value:'seed'},'key');t.oncomplete=()=>{db.close();resolve('committed')};t.onabort=()=>reject(String(t.error))}})", Some(isolated.clone())
    )).await;
    let cache_write = tokio::time::timeout(Duration::from_secs(2), native.eval(
        "caches.open('probe').then(c=>c.put('/seed',new Response('seed'))).then(()=> 'committed')", Some(isolated)
    )).await;
    let after = g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap();
    println!("STORAGE_BARRIER_PROBE {}", json!({"before":before,"after":after,
        "database_names":format!("{databases:?}"),"cache_names":format!("{caches:?}"),
        "isolated_idb_write":format!("{idb_write:?}"),"isolated_cache_write":format!("{cache_write:?}")}));
    assert_eq!(before, after, "diagnostic must not release the app or clock");
    assert_eq!(before["booted"], false);
    // A control phase distinguishes paused serviceability from unsupported APIs/origins.
    g.advance(VirtualDuration(10_000)).await.unwrap();
    let resumed_databases = tokio::time::timeout(Duration::from_secs(2), native.call(&indexed_db::RequestDatabaseNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    let resumed_caches = tokio::time::timeout(Duration::from_secs(2), native.call(&cache_storage::RequestCacheNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    println!("STORAGE_BARRIER_PROBE {}", json!({"control_after_advance":g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap(),
        "resumed_database_names":format!("{resumed_databases:?}"),"resumed_cache_names":format!("{resumed_caches:?}")}));
    native.call(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)).await.unwrap();
    let running_databases = tokio::time::timeout(Duration::from_secs(2), native.call(&indexed_db::RequestDatabaseNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    let running_caches = tokio::time::timeout(Duration::from_secs(2), native.call(&cache_storage::RequestCacheNamesParams {
        storage_key: Some(key.clone()), ..Default::default()
    })).await;
    native.call(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)).await.unwrap();
    println!("STORAGE_BARRIER_PROBE {}", json!({"running_database_names":format!("{running_databases:?}"),
        "running_cache_names":format!("{running_caches:?}")}));
    g.stop().await.unwrap();
}
