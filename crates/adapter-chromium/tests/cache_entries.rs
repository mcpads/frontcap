//! The engine owns CacheStorage matching and must not create caches during deletion.
mod support;
use cdp::protocol::{cache_storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn deleting_with_an_obsolete_cache_id_does_not_recreate_the_cache() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let addr=support::serve();
    let runs=tempfile::tempdir().unwrap();
    let origin=format!("http://{addr}");
    let mut options=support::options(runs.path(),"cache-delete",format!("{origin}/frames_idle.html"),RuleSet::default());
    options.profile.initial_caches=vec![frontcap_core::cache::InitialCache {
        origin:origin.clone(),partition:None,bucket:None,name:"temporary".into(),
        entries:vec![frontcap_core::cache::CacheEntry {request_url:format!("{origin}/item"),request_headers:Default::default(),status:200,status_text:String::new(),headers:Default::default(),body:"original".into(),body_base64:None,metadata:None}],
    }];
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let endpoint=g.endpoint();
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap();
    let native=Session::new(client,attached.session_id.0);
    let names=cache_storage::RequestCacheNamesParams {security_origin:Some(origin.clone()),storage_key:None,storage_bucket:None};
    let caches=native.call(&names).await.unwrap().caches;
    assert_eq!(caches.len(),1);
    let cache_id=caches[0].cache_id.clone();
    let before=native.eval("performance.now()",None).await.unwrap();
    assert!(native.call(&cache_storage::FrontcapReadEntryParams {cache_id:cache_id.clone(),request_url:format!("{origin}/item"),request_headers:vec![]}).await.unwrap().response.is_some());
    native.call(&cache_storage::DeleteCacheParams::new(cache_id.clone())).await.unwrap();
    for _ in 0..2 {
        native.call(&cache_storage::FrontcapDeleteEntryParams {cache_id:cache_id.clone(),request_url:format!("{origin}/item"),request_headers:vec![]}).await.unwrap();
        assert!(native.call(&names).await.unwrap().caches.is_empty(),"deleting through a stale identity must not open a new cache");
        assert!(native.call(&cache_storage::FrontcapReadEntryParams {cache_id:cache_id.clone(),request_url:format!("{origin}/item"),request_headers:vec![]}).await.unwrap().response.is_none());
    }
    assert_eq!(native.eval("performance.now()",None).await.unwrap(),before);
    native.client.close();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_cache_body_limit_refuses_oversized_page_content_without_progress() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let addr=support::serve();
    let runs=tempfile::tempdir().unwrap();
    let origin=format!("http://{addr}");
    let mut g=Generation::launch(support::options(runs.path(),"cache-body",format!("{origin}/frames_idle.html"),RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    g.eval_main("globalThis.largeDone=false; caches.open('large').then(c=>c.put('/body',new Response(new Uint8Array(4*1024*1024+1)))).then(()=>largeDone=true); true").await.unwrap();
    for _ in 0..40 {
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        if g.eval_main("largeDone").await.unwrap()==serde_json::json!(true) { break; }
    }
    assert_eq!(g.eval_main("largeDone").await.unwrap(),serde_json::json!(true));
    let endpoint=g.endpoint();
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap();
    let native=Session::new(client,attached.session_id.0);
    let caches=native.call(&cache_storage::RequestCacheNamesParams {security_origin:Some(origin.clone()),storage_key:None,storage_bucket:None}).await.unwrap().caches;
    let cache_id=caches.into_iter().find(|c|c.cache_name=="large").unwrap().cache_id;
    let before=native.eval("performance.now()",None).await.unwrap();
    let error=native.call(&cache_storage::FrontcapReadEntryParams {cache_id,request_url:format!("{origin}/body"),request_headers:vec![]}).await.unwrap_err();
    assert!(error.to_string().contains("4 MiB"),"{error}");
    assert_eq!(native.eval("performance.now()",None).await.unwrap(),before);
    native.client.close();g.stop().await.unwrap();
}

// Append to adapter-chromium/tests/cache_entries.rs when the new engine is installed.
#[tokio::test]
async fn native_empty_cache_creation_is_idempotent_and_keeps_existing_entries() {
    let addr=support::serve();
    let runs=tempfile::tempdir().unwrap();
    let origin=format!("http://{addr}");
    let mut options=support::options(runs.path(),"cache-empty",format!("{origin}/frames_idle.html"),RuleSet::default());
    options.profile.initial_caches=vec![frontcap_core::cache::InitialCache {
        origin:origin.clone(),partition:None,bucket:None,name:"existing".into(),
        entries:vec![frontcap_core::cache::CacheEntry {request_url:format!("{origin}/item"),request_headers:Default::default(),status:200,status_text:String::new(),headers:Default::default(),body:"original".into(),body_base64:None,metadata:None}],
    }];
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let scope=g.storage_scope(&frontcap_core::predicate::Locator::Document {selector:None}).await.unwrap().scope;
    let endpoint=g.endpoint();
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap();
    let native=Session::new(client,attached.session_id.0);
    let before=native.eval("performance.now()",None).await.unwrap();
    for name in ["empty","empty","existing"] {
        let created=native.call(&cache_storage::FrontcapPutEntriesParams {
            storage_key:scope.storage_key.clone(),storage_bucket_name:None,cache_name:name.into(),entries:vec![],
        }).await.unwrap();
        let inventory=native.call(&cache_storage::RequestEntriesParams::new(created.cache_id)).await.unwrap();
        assert_eq!(inventory.cache_data_entries.len(),if name=="existing" {1} else {0});
        if name=="existing" {assert_eq!(inventory.cache_data_entries[0].request_url,format!("{origin}/item"));}
    }
    let caches=native.call(&cache_storage::RequestCacheNamesParams {security_origin:Some(origin),storage_key:None,storage_bucket:None}).await.unwrap().caches;
    assert_eq!(caches.len(),2);
    assert_eq!(native.eval("performance.now()",None).await.unwrap(),before);
    native.client.close();g.stop().await.unwrap();
}

#[tokio::test]
async fn native_cache_inventory_preserves_key_order_and_never_recreates_a_missing_cache() {
    let addr=support::serve();let runs=tempfile::tempdir().unwrap();let origin=format!("http://{addr}");
    let mut options=support::options(runs.path(),"cache-order",format!("{origin}/frames_idle.html"),RuleSet::default());
    options.profile.initial_caches=vec![frontcap_core::cache::InitialCache {origin:origin.clone(),partition:None,bucket:None,name:"ordered".into(),entries:["z","a","m"].iter().map(|name|frontcap_core::cache::CacheEntry {
        request_url:format!("{origin}/{name}"),request_headers:Default::default(),status:200,status_text:String::new(),headers:Default::default(),body:name.to_string(),body_base64:None,metadata:None,
    }).collect()}];
    let mut g=Generation::launch(options).await.unwrap();g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let endpoint=g.endpoint();let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap();let native=Session::new(client,attached.session_id.0);
    let names=cache_storage::RequestCacheNamesParams {security_origin:Some(origin.clone()),storage_key:None,storage_bucket:None};
    let cache_id=native.call(&names).await.unwrap().caches[0].cache_id.clone();
    let before=native.eval("performance.now()",None).await.unwrap();
    for (preserve,expected) in [(false,["a","m","z"]),(true,["z","a","m"])] {
        let mut request=cache_storage::RequestEntriesParams::new(cache_id.clone());request.frontcap_preserve_order=Some(preserve);
        let entries=native.call(&request).await.unwrap();
        assert_eq!(entries.cache_data_entries.iter().map(|e|e.request_url.clone()).collect::<Vec<_>>(),expected.iter().map(|s|format!("{origin}/{s}")).collect::<Vec<_>>());
    }
    let mut request=cache_storage::RequestEntriesParams::new(cache_id.clone());request.frontcap_preserve_order=Some(true);request.skip_count=Some(1);request.page_size=Some(1);
    let page=native.call(&request).await.unwrap();assert_eq!(page.return_count,3.0);assert_eq!(page.cache_data_entries[0].request_url,format!("{origin}/a"));
    native.call(&cache_storage::DeleteCacheParams::new(cache_id)).await.unwrap();
    assert!(native.call(&request).await.is_err());assert!(native.call(&names).await.unwrap().caches.is_empty());
    assert_eq!(native.eval("performance.now()",None).await.unwrap(),before);
    native.client.close();g.stop().await.unwrap();
}
