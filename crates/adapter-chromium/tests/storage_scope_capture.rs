//! Complete native bucket inventory and non-creating read connections.
mod support;
use cdp::protocol::{cache_storage, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

async fn attach(g: &Generation) -> Session {
    let endpoint=g.endpoint();
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {
        flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    Session::new(client,attached.session_id.0)
}

#[tokio::test]
async fn complete_inventory_retains_removed_frame_unvisited_and_expired_scopes() {
    let origin="https://capture.test";
    let frame="https://removed-capture.test";
    let body=format!(r#"<!doctype html><iframe id=child src='{frame}/child'></iframe><script>
      window.ready=false;addEventListener('message',e=>{{if(e.data==='stored'){{document.querySelector('#child').remove();ready=true}}}});
    </script>"#);
    let child=r#"<!doctype html><script>(async()=>{
      const b=await navigator.storageBuckets.open('from-frame');
      await(await b.caches.open('kept')).put('/item',new Response('child data'));
      const db=await new Promise((resolve,reject)=>{const r=b.indexedDB.open('records',1);r.onupgradeneeded=()=>{const store=r.result.createObjectStore('items');store.createIndex('by-kind','kind')};r.onerror=()=>reject(r.error);r.onsuccess=()=>resolve(r.result)});
      await new Promise((resolve,reject)=>{const t=db.transaction('items','readwrite');t.objectStore('items').put({kind:'kept',value:7},'child');t.oncomplete=resolve;t.onabort=()=>reject(t.error)});db.close();
      parent.postMessage('stored','*');
    })();</script>"#;
    let rules=RuleSet::from_json(&json!([
      {"id":"top","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
      {"id":"child","match":{"url":format!("{frame}/child")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child}}
    ]).to_string()).unwrap();
    let runs=tempfile::tempdir().unwrap();
    let mut options=support::options(runs.path(),"complete-inventory",format!("{origin}/start"),rules);
    options.profile.initial_buckets=serde_json::from_value(json!([
      {"origin":"https://unvisited-capture.test","name":"expired","expiration":"12591158399999000","quota":"2097152","persistent":true,"durability":"strict"},
      {"origin":"https://unvisited-capture.test","partition":{"top_level_site":origin,"has_cross_site_ancestor":true},"name":"partitioned","quota":"0","persistent":false,"durability":"relaxed"}
    ])).unwrap();
    let mut g=Generation::launch(options).await.unwrap();g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("ready && document.querySelector('iframe')===null").await.unwrap(),true);
    let native=attach(&g).await;
    let scopes=native.call(&storage::FrontcapReadStorageScopesParams::default()).await.unwrap().scopes;
    let snapshot=serde_json::to_value(&scopes).unwrap();
    assert_eq!(serde_json::to_value(native.call(&storage::FrontcapReadStorageScopesParams::default()).await.unwrap().scopes).unwrap(),snapshot);
    let removed=scopes.iter().find(|s|s.origin==frame).unwrap();
    assert_eq!(removed.top_level_site,origin);assert!(removed.has_cross_site_ancestor);
    assert!(removed.buckets.iter().any(|b|b.name.as_deref()==Some("from-frame")));
    let unvisited:Vec<_>=scopes.iter().filter(|s|s.origin=="https://unvisited-capture.test").collect();
    assert_eq!(unvisited.len(),2);
    let expired=unvisited.iter().find(|s|s.first_party).unwrap().buckets.iter().find(|b|b.name.as_deref()==Some("expired")).unwrap();
    assert_eq!(expired.expiration.as_deref(),Some("12591158399999000"));
    assert_eq!(expired.durability,storage::StorageBucketsDurability::Strict);assert!(expired.persistent);
    assert_eq!(expired.quota,"2097152");
    assert_eq!(g.now().0,100_000);assert_eq!(g.eval_main("performance.now()").await.unwrap(),100);
    let captured=g.live_state().await.unwrap();
    assert!(captured.storage_inventory_complete);
    assert!(captured.storage_excluded.is_empty(),"{:?}",captured.storage_excluded);
    assert!(captured.cache_excluded.is_empty(),"{:?}",captured.cache_excluded);
    assert!(captured.indexed_db_excluded.is_empty(),"{:?}",captured.indexed_db_excluded);
    let kept=captured.caches.iter().find(|c|c.origin==frame && c.name=="kept").unwrap();
    assert_eq!(kept.bucket.as_deref(),Some("from-frame"));
    assert_eq!(kept.partition.as_ref().unwrap().top_level_site,origin);
    assert_eq!(kept.entries.len(),1);
    assert_eq!(kept.entries[0].body_bytes().unwrap(),b"child data");
    assert!(captured.buckets.iter().any(|b|b.origin=="https://unvisited-capture.test" && b.name.as_deref()==Some("expired") && b.expiration.as_deref()==Some("12591158399999000")));
    assert_eq!(serde_json::to_value(native.call(&storage::FrontcapReadStorageScopesParams::default()).await.unwrap().scopes).unwrap(),snapshot);
    assert_eq!(g.now().0,100_000);
    let db=captured.indexed_db.iter().find(|d|d.origin==frame && d.name=="records").unwrap();
    assert_eq!(db.bucket.as_deref(),Some("from-frame"));
    assert_eq!(db.stores[0].records.len(),1);
    assert_eq!(db.stores[0].records[0].value,json!({"kind":"kept","value":7}));
    assert_eq!(db.stores[0].indexes[0].name,"by-kind");
    let rules=RuleSet::from_json(&json!([{"id":"blank","match":{"url":format!("{origin}/restored")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let mut options=support::options(runs.path(),"restored-capture",format!("{origin}/restored"),rules);
    options.profile.initial_buckets=captured.buckets.clone();
    options.profile.initial_caches=captured.caches.clone();
    options.profile.initial_indexed_db=captured.indexed_db.clone();
    let mut restored=Generation::launch(options).await.unwrap();
    restored.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let recaptured=restored.live_state().await.unwrap();
    assert_eq!(recaptured.buckets,captured.buckets);
    assert_eq!(recaptured.caches,captured.caches);
    assert_eq!(recaptured.indexed_db,captured.indexed_db);
    assert_eq!(restored.now().0,1_000);
    restored.stop().await.unwrap();
    native.client.close();g.stop().await.unwrap();
}

#[tokio::test]
async fn read_connection_refuses_missing_bucket_and_preserves_expired_policy() {
    let origin="https://readonly-capture.test";
    let rules=RuleSet::from_json(&json!([{ "id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let runs=tempfile::tempdir().unwrap();
    let mut options=support::options(runs.path(),"readonly-capture",format!("{origin}/start"),rules);
    options.profile.initial_buckets=serde_json::from_value(json!([{"origin":"https://stored-capture.test","name":"expired","expiration":"12591158399999000","quota":"1048576","persistent":true,"durability":"strict"},{"origin":"https://expired-default.test","expiration":"12591158399999000","quota":"1048576","persistent":true,"durability":"relaxed"}])).unwrap();
    let g=Generation::launch(options).await.unwrap();let native=attach(&g).await;
    let snapshot=serde_json::to_value(native.call(&storage::FrontcapReadStorageScopesParams::default()).await.unwrap().scopes).unwrap();
    for (origin,name,present) in [("https://absent-capture.test",None,false),("https://stored-capture.test",Some("missing"),false),("https://stored-capture.test",Some("expired"),true),("https://expired-default.test",None,true)] {
        let grant=native.call(&storage::FrontcapGrantStorageScopeParams {
            bucket_name:name.map(str::to_owned),create_if_missing:Some(false),
            ..storage::FrontcapGrantStorageScopeParams::new(origin.into())
        }).await.unwrap();
        let bind=cache_storage::FrontcapBindStorageScopeParams {
            storage_bucket_name:name.map(str::to_owned),
            ..cache_storage::FrontcapBindStorageScopeParams::new(grant.storage_key.0.clone(),grant.scope_token)
        };
        assert_eq!(native.call(&bind).await.is_ok(),present,"{origin} {name:?}");
        if present {
            let caches=native.call(&cache_storage::RequestCacheNamesParams {security_origin:None,storage_key:None,storage_bucket:Some(storage::StorageBucket {storage_key:grant.storage_key.clone(),name:name.map(str::to_owned)})}).await.unwrap();
            assert!(caches.caches.is_empty());
            native.call(&cache_storage::FrontcapReleaseStorageScopeParams {storage_bucket_name:name.map(str::to_owned),..cache_storage::FrontcapReleaseStorageScopeParams::new(grant.storage_key.0)}).await.unwrap();
        }
        assert!(native.call(&bind).await.is_err(),"the capability must be consumed once, including after release");
        assert_eq!(serde_json::to_value(native.call(&storage::FrontcapReadStorageScopesParams::default()).await.unwrap().scopes).unwrap(),snapshot);
    }
    assert_eq!(g.now().0,0);native.client.close();g.stop().await.unwrap();
}
