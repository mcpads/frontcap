//! A native scope inventory has a completion boundary and never creates or expires buckets.
mod support;
use cdp::protocol::{page, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

#[tokio::test]
async fn bucket_inventory_preserves_frame_partitions_policies_and_missing_scopes() {
    let origin = "https://inventory.test";
    let child = "https://other-inventory.test";
    let parent = r#"<!doctype html><iframe src="https://other-inventory.test/child"></iframe><script>
    window.topReady=false;window.childReady=false;window.durabilityAvailable=null;addEventListener('message',e=>{if(e.data==='ready')childReady=true});
    (async()=>{await(await caches.open('default')).put('/item',new Response('top'));
    const b=await navigator.storageBuckets.open('alpha',{durability:'strict',quota:2097152,expires:4102444800123});
    durabilityAvailable=typeof b.durability==='function';
    await(await b.caches.open('named')).put('/item',new Response('top-alpha'));
    await navigator.storageBuckets.open('empty');topReady=true;})();</script>"#;
    let frame = r#"<!doctype html><script>(async()=>{
    await(await caches.open('default')).put('/item',new Response('child'));
    const b=await navigator.storageBuckets.open('alpha',{quota:1048576});
    await(await b.caches.open('named')).put('/item',new Response('child-alpha'));
    parent.postMessage('ready','*');})();</script>"#;
    let rules=RuleSet::from_json(&json!([
      {"id":"top","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":parent}},
      {"id":"child","match":{"url":format!("{child}/child")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":frame}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(
        runs.path(),
        "storage-inventory",
        format!("{origin}/start"),
        rules,
    ))
    .await
    .unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("topReady && childReady").await.unwrap(), true);
    assert_eq!(
        g.eval_main("durabilityAvailable").await.unwrap(),
        false,
        "durability is experimental and disabled in the product profile"
    );
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
            },
        )
        .await
        .unwrap();
    let native = Session::new(client, attached.session_id.0);
    let tree = native
        .call(&page::GetFrameTreeParams::default())
        .await
        .unwrap()
        .frame_tree;
    let children = tree.child_frames.unwrap();
    assert_eq!(children.len(), 1);
    let mut snapshots = Vec::new();
    for (frame, expected_origin, first_party) in [
        (tree.frame, origin, true),
        (children.into_iter().next().unwrap().frame, child, false),
    ] {
        let key = native
            .call(&storage::GetStorageKeyForFrameParams::new(frame.id))
            .await
            .unwrap()
            .storage_key;
        let request = storage::FrontcapReadStorageScopeParams::new(key.clone());
        let scope = native.call(&request).await.unwrap().scope;
        assert_eq!(scope.storage_key, key);
        assert_eq!(scope.origin, expected_origin);
        assert_eq!(scope.top_level_site, origin);
        assert_eq!(scope.first_party, first_party);
        assert_eq!(scope.has_cross_site_ancestor, !first_party);
        assert!(!scope.nonce_partition);
        let expected = if first_party {
            vec![None, Some("alpha"), Some("empty")]
        } else {
            vec![None, Some("alpha")]
        };
        assert_eq!(
            scope
                .buckets
                .iter()
                .map(|b| b.name.as_deref())
                .collect::<Vec<_>>(),
            expected
        );
        let alpha = &scope.buckets[1];
        assert_eq!(alpha.quota, if first_party { "2097152" } else { "1048576" });
        assert_eq!(serde_json::to_value(alpha.durability).unwrap(), "relaxed");
        assert_eq!(
            alpha.expiration,
            first_party.then(|| "15746918400123000".to_string())
        );
        let snapshot = serde_json::to_value(&scope).unwrap();
        assert_eq!(
            serde_json::to_value(native.call(&request).await.unwrap().scope).unwrap(),
            snapshot
        );
        snapshots.push(snapshot);
    }
    let absent = native
        .call(&storage::FrontcapResolveStorageKeyParams {
            origin: "https://absent-inventory.test".into(),
            top_level_site: None,
            has_cross_site_ancestor: None,
        })
        .await
        .unwrap()
        .storage_key;
    for _ in 0..2 {
        let scope = native
            .call(&storage::FrontcapReadStorageScopeParams::new(
                absent.clone(),
            ))
            .await
            .unwrap()
            .scope;
        assert!(
            scope.buckets.is_empty(),
            "reading an absent scope must not create a default bucket"
        );
    }
    assert!(
        native
            .call(&storage::FrontcapReadStorageScopeParams::new(
                storage::SerializedStorageKey("invalid".into())
            ))
            .await
            .is_err()
    );
    assert_eq!(g.now().0, 100_000);
    assert_eq!(g.eval_main("performance.now()").await.unwrap(), 100);
    println!("STORAGE_INVENTORY={}", json!(snapshots));
    native.client.close();
    g.stop().await.unwrap();
}

/// Test-only Blink opt-in exercises strict native policy without changing the product profile.
#[tokio::test]
async fn inventory_reports_strict_policy_when_the_browser_accepts_it() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", server.server_addr().to_ip().unwrap());
    let serving = std::thread::spawn(move || {
        if let Ok(Some(request)) = server.recv_timeout(std::time::Duration::from_secs(10)) {
            let body = r#"<!doctype html><script>window.ready=(async()=>{
              const b=await navigator.storageBuckets.open('strict-policy',{durability:'strict',quota:4194304});
              return await b.durability();
            })();</script>"#;
            request
                .respond(tiny_http::Response::from_string(body).with_header(
                    tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap(),
                ))
                .unwrap();
        }
    });
    let root = support::repo_root();
    let (binary, _) =
        browser_pin::engine::patched_binary(&root, browser_pin::current_platform().unwrap())
            .unwrap()
            .expect("built controlled engine");
    let browser = browser_pin::launch(
        &binary,
        &[
            "--enable-features=FrontcapFramesWhileFrozen".into(),
            "--enable-blink-features=StorageBucketsDurability".into(),
        ],
        std::time::Duration::from_secs(20),
    )
    .unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let created = client
        .call(None, &target::CreateTargetParams::new("about:blank".into()))
        .await
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(created.target_id)
            },
        )
        .await
        .unwrap();
    let native = Session::new(client, attached.session_id.0);
    native.call(&page::EnableParams::default()).await.unwrap();
    let mut loaded = native.subscribe::<page::LoadEventFiredEvent>();
    native
        .call(&page::NavigateParams::new(format!("{origin}/")))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), loaded.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(native.eval("ready", None).await.unwrap(), "strict");
    let frame = native
        .call(&page::GetFrameTreeParams::default())
        .await
        .unwrap()
        .frame_tree
        .frame;
    let key = native
        .call(&storage::GetStorageKeyForFrameParams::new(frame.id))
        .await
        .unwrap()
        .storage_key;
    let request = storage::FrontcapReadStorageScopeParams::new(key);
    let scope = native.call(&request).await.unwrap().scope;
    let bucket = scope
        .buckets
        .iter()
        .find(|b| b.name.as_deref() == Some("strict-policy"))
        .unwrap();
    assert_eq!(serde_json::to_value(bucket.durability).unwrap(), "strict");
    assert_eq!(bucket.quota, "4194304");
    assert!(!bucket.persistent);
    assert!(bucket.expiration.is_none());
    assert_eq!(
        serde_json::to_value(&scope).unwrap(),
        serde_json::to_value(native.call(&request).await.unwrap().scope).unwrap()
    );
    native.client.close();
    drop(browser);
    serving.join().unwrap();
}
