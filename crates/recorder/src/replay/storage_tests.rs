use super::*;

async fn check(with_cache: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = crate::launcher::Workspace {
        root: tmp.path().join(".frontcap"),
    };
    std::fs::create_dir_all(&ws.root).unwrap();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let partition = StoragePartition {
        top_level_site: "https://source.test".into(),
        has_cross_site_ancestor: Some(false),
    };
    let mut original = DeterminismProfile::default();
    original
        .initial_buckets
        .push(frontcap_core::storage::InitialBucket {
            origin: "https://widget.source.test".into(),
            partition: Some(partition.clone()),
            name: Some("saved".into()),
            expiration: Some("15746918400000000".into()),
            quota: "2097152".into(),
            persistent: true,
            durability: frontcap_core::storage::BucketDurability::Strict,
        });
    if with_cache {
        original
            .initial_caches
            .push(frontcap_core::cache::InitialCache {
                origin: "https://widget.source.test".into(),
                partition: Some(partition),
                bucket: Some("saved".into()),
                name: "empty".into(),
                entries: Vec::new(),
            });
    }
    let mut moved = original.clone();
    let relocation = StorageRelocation::prepare(
        &ws,
        &root,
        &original,
        &mut moved,
        "https://app.source.test",
        "https://app.destination.test",
    )
    .await
    .unwrap();
    assert!(
        relocation.is_some(),
        "policy-only declarations also need native site relocation"
    );
    let policy = &moved.initial_buckets[0];
    assert_eq!(
        policy.partition.as_ref().unwrap(),
        &StoragePartition {
            top_level_site: "https://destination.test".into(),
            has_cross_site_ancestor: Some(true),
        }
    );
    let mut expected = original.initial_buckets[0].clone();
    expected.partition = policy.partition.clone();
    assert_eq!(
        policy, &expected,
        "relocation must preserve exact policy and fixed sibling origin"
    );
    if with_cache {
        assert_eq!(moved.initial_caches[0].partition, policy.partition);
    }
}

#[tokio::test]
async fn policy_only_scope_moves_and_gains_cross_site_ancestry() {
    check(false).await;
}

#[tokio::test]
async fn policy_and_cache_remain_in_the_same_relocated_partition() {
    check(true).await;
}
