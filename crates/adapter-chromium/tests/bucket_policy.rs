//! Exact bucket policy restoration must preserve existing data and reject conflicts.
mod support;
use cdp::protocol::{page, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

#[tokio::test]
async fn restore_bucket_policy_is_exact_idempotent_and_non_destructive() {
    let origin = "https://bucket-policy.test";
    let body = r#"<!doctype html><script>window.ready=false;(async()=>{
      const b=await navigator.storageBuckets.open('kept');
      await(await b.caches.open('data')).put('/item',new Response('preserved'));
      window.ready=true;
    })();</script>"#;
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(
        runs.path(),
        "bucket-policy",
        format!("{origin}/start"),
        rules,
    ))
    .await
    .unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("ready").await.unwrap(), true);
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
    let inventory = storage::FrontcapReadStorageScopeParams::new(key.clone());
    let scope = native.call(&inventory).await.unwrap().scope;
    let kept = scope
        .buckets
        .into_iter()
        .find(|b| b.name.as_deref() == Some("kept"))
        .unwrap();
    for _ in 0..2 {
        native
            .call(&storage::FrontcapRestoreBucketParams::new(
                key.clone(),
                kept.clone(),
            ))
            .await
            .unwrap();
    }
    // Test every independent policy field against a bucket containing real data.
    for field in ["expiration", "quota", "persistent", "durability"] {
        let mut conflict = kept.clone();
        match field {
            "expiration" => conflict.expiration = Some("15746918400000000".into()),
            "quota" => conflict.quota = "1048576".into(),
            "persistent" => conflict.persistent = !kept.persistent,
            "durability" => conflict.durability = storage::StorageBucketsDurability::Strict,
            _ => unreachable!(),
        }
        let before = serde_json::to_value(native.call(&inventory).await.unwrap().scope).unwrap();
        let error = native
            .call(&storage::FrontcapRestoreBucketParams::new(
                key.clone(),
                conflict,
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("conflicts"), "{field}: {error}");
        assert_eq!(
            serde_json::to_value(native.call(&inventory).await.unwrap().scope).unwrap(),
            before
        );
    }
    // Native restore accepts exact expired metadata and strict policy without changing Blink defaults.
    let restored_key = native
        .call(&storage::FrontcapResolveStorageKeyParams {
            origin: "https://restored-policy.test".into(),
            top_level_site: None,
            has_cross_site_ancestor: None,
        })
        .await
        .unwrap()
        .storage_key;
    let restored_inventory = storage::FrontcapReadStorageScopeParams::new(restored_key.clone());
    assert!(
        native
            .call(&restored_inventory)
            .await
            .unwrap()
            .scope
            .buckets
            .is_empty()
    );
    let mut invalid_default = kept.clone();
    invalid_default.name = None;
    invalid_default.durability = storage::StorageBucketsDurability::Strict;
    assert!(
        native
            .call(&storage::FrontcapRestoreBucketParams::new(
                restored_key.clone(),
                invalid_default
            ))
            .await
            .is_err()
    );
    assert!(
        native
            .call(&restored_inventory)
            .await
            .unwrap()
            .scope
            .buckets
            .is_empty()
    );
    for name in [None, Some("expired"), Some("strict")] {
        let mut policy = kept.clone();
        policy.name = name.map(str::to_owned);
        policy.expiration = name
            .filter(|n| *n == "expired")
            .map(|_| "11644473600000000".to_string());
        policy.quota = "2097152".into();
        policy.persistent = true;
        policy.durability = if name.is_none() {
            storage::StorageBucketsDurability::Relaxed
        } else {
            storage::StorageBucketsDurability::Strict
        };
        for _ in 0..2 {
            native
                .call(&storage::FrontcapRestoreBucketParams::new(
                    restored_key.clone(),
                    policy.clone(),
                ))
                .await
                .unwrap();
        }
        let found = native
            .call(&restored_inventory)
            .await
            .unwrap()
            .scope
            .buckets
            .into_iter()
            .find(|b| b.name == policy.name)
            .unwrap();
        assert_eq!(
            serde_json::to_value(found).unwrap(),
            serde_json::to_value(policy).unwrap()
        );
    }
    let snapshot = serde_json::to_value(native.call(&inventory).await.unwrap().scope).unwrap();
    for (field, value) in [
        ("name", "_default"),
        ("name", ""),
        ("quota", "-1"),
        ("quota", "01"),
        ("quota", "9223372036854775808"),
        ("expiration", "0"),
        ("expiration", "+1"),
    ] {
        let mut invalid = kept.clone();
        invalid.name = Some("invalid".into());
        match field {
            "name" => invalid.name = Some(value.into()),
            "quota" => invalid.quota = value.into(),
            "expiration" => invalid.expiration = Some(value.into()),
            _ => unreachable!(),
        }
        assert!(
            native
                .call(&storage::FrontcapRestoreBucketParams::new(
                    key.clone(),
                    invalid
                ))
                .await
                .is_err(),
            "{field}: {value}"
        );
        assert_eq!(
            serde_json::to_value(native.call(&inventory).await.unwrap().scope).unwrap(),
            snapshot
        );
    }
    assert_eq!(g.now().0, 100_000);
    assert_eq!(g.eval_main("performance.now()").await.unwrap(), 100);
    g.eval_main("window.readback=null;setTimeout(async()=>{const b=await navigator.storageBuckets.open('kept');readback=await(await(await b.caches.open('data')).match('/item')).text()},1)").await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    assert_eq!(g.eval_main("readback").await.unwrap(), "preserved");
    native.client.close();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn initial_policy_survives_data_initialization_in_document_and_explicit_scopes() {
    let origin = "https://initial-policy.test";
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>ready</p>"}}]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(
        runs.path(),
        "initial-policy",
        format!("{origin}/start"),
        rules,
    );
    let scopes = [
        (origin, None),
        ("https://explicit-policy.test", None),
        (
            "https://partitioned-policy.test",
            Some(json!({"top_level_site":origin,"has_cross_site_ancestor":true})),
        ),
    ];
    for (origin, partition) in &scopes {
        options.profile.initial_buckets.push(serde_json::from_value(json!({
            "origin":origin,"partition":partition,"name":"saved","expiration":"12591158399999000",
            "quota":"2097152","persistent":true,"durability":"strict"
        })).unwrap());
        options.profile.initial_caches.push(serde_json::from_value(json!({
            "origin":origin,"partition":partition,"bucket":"saved","name":"contents","entries":[{
                "request_url":format!("{origin}/item"),"status":200,"body":"saved body"
            }]
        })).unwrap());
    }
    for (origin, partition) in &scopes {
        options.profile.initial_indexed_db.push(serde_json::from_value(json!({
            "origin":origin,"partition":partition,"bucket":"saved","name":"records","version":1,
            "stores":[{"name":"items","records":[]}]
        })).unwrap());
    }
    let g = Generation::launch(options).await.unwrap();
    assert_eq!(g.now().0, 0);
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
    for (origin, partition) in scopes {
        let key = native
            .call(&storage::FrontcapResolveStorageKeyParams {
                origin: origin.into(),
                top_level_site: partition
                    .as_ref()
                    .map(|p| p["top_level_site"].as_str().unwrap().into()),
                has_cross_site_ancestor: partition.as_ref().map(|_| true),
            })
            .await
            .unwrap()
            .storage_key;
        let scope = native
            .call(&storage::FrontcapReadStorageScopeParams::new(key))
            .await
            .unwrap()
            .scope;
        let policy = scope
            .buckets
            .iter()
            .find(|b| b.name.as_deref() == Some("saved"))
            .unwrap();
        assert_eq!(
            policy.expiration.as_deref(),
            Some("12591158399999000"),
            "{origin}"
        );
        assert_eq!(policy.quota, "2097152");
        assert!(policy.persistent);
        assert_eq!(policy.durability, storage::StorageBucketsDurability::Strict);
    }
    native.client.close();
    g.stop().await.unwrap();
}
