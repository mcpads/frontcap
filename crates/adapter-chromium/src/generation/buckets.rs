use super::*;

fn invalid(message: &str) -> ChromiumError {
    ChromiumError::InvalidArgument(message.into())
}

fn scope_resolution_error(error: ChromiumError) -> ChromiumError {
    match error {
        ChromiumError::Cdp(cdp::CdpError::Protocol {
            code: -32602,
            message,
            ..
        }) => invalid(&message),
        other => other,
    }
}

pub(super) fn validate_bucket_name(name: Option<&str>) -> Result<(), ChromiumError> {
    if let Some(name) = name
        && (name.is_empty()
            || name.len() >= 64
            || !name.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && matches!(b, b'-' | b'_'))
            }))
    {
        return Err(invalid(
            "storage bucket names require 1 to 63 lowercase ASCII letters, digits, hyphens or underscores, starting with a letter or digit",
        ));
    }
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EnsureBucket {
    storage_key: String,
    bucket_name: String,
}
impl cdp::Command for EnsureBucket {
    const METHOD: &'static str = "CacheStorage.frontcapEnsureBucket";
    type Returns = serde_json::Value;
}

impl Generation {
    pub(super) async fn ensure_storage_bucket(
        &self,
        key: &str,
        name: Option<&str>,
    ) -> Result<(), ChromiumError> {
        if let Some(name) = name {
            self.page
                .call(&EnsureBucket {
                    storage_key: key.into(),
                    bucket_name: name.into(),
                })
                .await?;
        }
        Ok(())
    }
}

pub(super) struct InitialStorageScope {
    pub key: String,
    pub explicit: bool,
    pub restored_policy: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveStorageKey<'a> {
    origin: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_level_site: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_cross_site_ancestor: Option<bool>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedStorageKey {
    storage_key: String,
}
impl cdp::Command for ResolveStorageKey<'_> {
    const METHOD: &'static str = "Storage.frontcapResolveStorageKey";
    type Returns = ResolvedStorageKey;
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GrantStorageScope<'a> {
    #[serde(flatten)]
    identity: ResolveStorageKey<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket_name: Option<&'a str>,
    create_if_missing: bool,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantedStorageScope {
    storage_key: String,
    scope_token: String,
}
impl cdp::Command for GrantStorageScope<'_> {
    const METHOD: &'static str = "Storage.frontcapGrantStorageScope";
    type Returns = GrantedStorageScope;
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BindStorageScope<'a> {
    storage_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_bucket_name: Option<&'a str>,
    scope_token: String,
}
impl cdp::Command for BindStorageScope<'_> {
    const METHOD: &'static str = "CacheStorage.frontcapBindStorageScope";
    type Returns = serde_json::Value;
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseStorageScope<'a> {
    storage_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_bucket_name: Option<&'a str>,
}
impl cdp::Command for ReleaseStorageScope<'_> {
    const METHOD: &'static str = "CacheStorage.frontcapReleaseStorageScope";
    type Returns = serde_json::Value;
}
fn identity<'a>(
    origin: &'a str,
    partition: Option<&'a frontcap_core::storage::StoragePartition>,
) -> ResolveStorageKey<'a> {
    ResolveStorageKey {
        origin,
        top_level_site: partition.map(|p| p.top_level_site.as_str()),
        has_cross_site_ancestor: partition.and_then(|p| p.has_cross_site_ancestor),
    }
}
pub(super) fn validate_partition(
    partition: Option<&frontcap_core::storage::StoragePartition>,
) -> Result<(), ChromiumError> {
    if let Some(partition) = partition {
        // Browser canonicalization also checks schemeful-site/registrable-domain semantics.
        crate::encode::canonical_origin_host(&partition.top_level_site)?;
    }
    Ok(())
}
impl Generation {
    pub(super) async fn resolve_initial_storage_scopes(
        &self,
    ) -> Result<(Vec<InitialStorageScope>, Vec<InitialStorageScope>, Vec<InitialStorageScope>), ChromiumError> {
        if self.profile.initial_caches.is_empty() && self.profile.initial_indexed_db.is_empty() && self.profile.initial_buckets.is_empty() {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        let frame = self
            .page
            .call(&page::GetFrameTreeParams::default())
            .await?
            .frame_tree
            .frame;
        let origin = origin_of(&frame.url)
            .ok_or_else(|| invalid("storage initialization requires an HTTP(S) document"))?;
        let actual = self
            .page
            .call(&cdp::protocol::storage::GetStorageKeyForFrameParams::new(
                frame.id,
            ))
            .await?
            .storage_key
            .0;
        let mut policies = Vec::new();
        let mut policy_keys = std::collections::BTreeSet::new();
        for bucket in &self.profile.initial_buckets {
            let explicit = bucket.partition.is_some() || bucket.origin != origin;
            let key = if explicit {
                self.page.call(&identity(&bucket.origin, bucket.partition.as_ref())).await
                    .map_err(scope_resolution_error)?.storage_key
            } else { actual.clone() };
            if !policy_keys.insert((key.clone(), bucket.name.clone())) {
                return Err(invalid("initial bucket policies resolve to the same storage key and bucket"));
            }
            policies.push(InitialStorageScope { key, explicit, restored_policy: true });
        }
        let mut caches = Vec::new();
        let mut databases = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for (provider, declared_origin, partition, bucket, name) in self
            .profile
            .initial_caches
            .iter()
            .map(|c| ("cache", &c.origin, c.partition.as_ref(), &c.bucket, &c.name))
            .chain(self.profile.initial_indexed_db.iter().map(|d| {
                (
                    "indexed_db",
                    &d.origin,
                    d.partition.as_ref(),
                    &d.bucket,
                    &d.name,
                )
            }))
        {
            let explicit = partition.is_some() || declared_origin != &origin;
            let key = if explicit {
                self.page
                    .call(&identity(declared_origin, partition))
                    .await
                    .map_err(scope_resolution_error)?
                    .storage_key
            } else {
                actual.clone()
            };
            if !identities.insert((provider, key.clone(), bucket.clone(), name.clone())) {
                return Err(invalid(
                    "initial storage declarations resolve to the same provider/key/bucket/name",
                ));
            }
            let restored_policy = policy_keys.contains(&(key.clone(), bucket.clone()));
            let scope = InitialStorageScope { key, explicit, restored_policy };
            if provider == "cache" {
                caches.push(scope);
            } else {
                databases.push(scope);
            }
        }
        Ok((caches, databases, policies))
    }
    pub(super) async fn bind_initial_storage_scope(
        &self,
        scope: &InitialStorageScope,
        origin: &str,
        partition: Option<&frontcap_core::storage::StoragePartition>,
        bucket: Option<&str>,
    ) -> Result<(), ChromiumError> {
        if !scope.explicit && !scope.restored_policy {
            return self.ensure_storage_bucket(&scope.key, bucket).await;
        }
        // A restored local default bucket also needs an exact inspector binding:
        // ordinary default-bucket resolution can expire and recreate saved data.
        let local_partition = if !scope.explicit {
            use cdp::protocol::storage;
            let native = self.page.call(&storage::FrontcapReadStorageScopeParams::new(
                storage::SerializedStorageKey(scope.key.clone()))).await?.scope;
            Some(frontcap_core::storage::StoragePartition {
                top_level_site: native.top_level_site,
                has_cross_site_ancestor: Some(native.has_cross_site_ancestor),
            })
        } else { None };
        let grant = self
            .page
            .call(&GrantStorageScope {
                identity: identity(origin, partition.or(local_partition.as_ref())),
                bucket_name: bucket,
                create_if_missing: !scope.restored_policy,
            })
            .await?;
        if grant.storage_key != scope.key {
            return Err(ChromiumError::InterventionUncertain(
                "storage scope changed between resolution and grant".into(),
            ));
        }
        self.page
            .call(&BindStorageScope {
                storage_key: &scope.key,
                storage_bucket_name: bucket,
                scope_token: grant.scope_token,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn release_initial_storage_scope(
        &self,
        scope: &InitialStorageScope,
        bucket: Option<&str>,
    ) -> Result<(), ChromiumError> {
        if scope.explicit || scope.restored_policy {
            self.page
                .call(&ReleaseStorageScope {
                    storage_key: &scope.key,
                    storage_bucket_name: bucket,
                })
                .await?;
        }
        Ok(())
    }
}

pub(super) fn validate_initial_buckets(buckets: &[frontcap_core::storage::InitialBucket]) -> Result<(), ChromiumError> {
    for bucket in buckets {
        crate::encode::canonical_origin_host(&bucket.origin)?;
        validate_partition(bucket.partition.as_ref())?;
        validate_bucket_name(bucket.name.as_deref())?;
        if bucket.name.is_none() && bucket.durability != frontcap_core::storage::BucketDurability::Relaxed {
            return Err(invalid("default bucket durability must be relaxed"));
        }
        let quota = bucket.quota.parse::<i64>().map_err(|_| invalid("bucket quota requires a signed-64-bit decimal integer"))?;
        if quota < 0 || quota.to_string() != bucket.quota { return Err(invalid("bucket quota requires canonical nonnegative decimal bytes")); }
        if let Some(text) = &bucket.expiration {
            let expiration = text.parse::<i64>().map_err(|_| invalid("bucket expiration requires signed-64-bit native microseconds"))?;
            if expiration == 0 || expiration.to_string() != *text { return Err(invalid("bucket expiration requires canonical nonzero native microseconds")); }
        }
    }
    Ok(())
}

fn native_bucket_policy(bucket: &frontcap_core::storage::InitialBucket) -> cdp::protocol::storage::FrontcapBucket {
    use cdp::protocol::storage;
    storage::FrontcapBucket {
        name: bucket.name.clone(), expiration: bucket.expiration.clone(), quota: bucket.quota.clone(), persistent: bucket.persistent,
        durability: match bucket.durability {
            frontcap_core::storage::BucketDurability::Strict => storage::StorageBucketsDurability::Strict,
            frontcap_core::storage::BucketDurability::Relaxed => storage::StorageBucketsDurability::Relaxed,
        },
    }
}

impl Generation {
    pub(super) async fn initialize_bucket_policies(&self, scopes: &[InitialStorageScope]) -> Result<(), ChromiumError> {
        use cdp::protocol::storage;
        if scopes.len() != self.profile.initial_buckets.len() { return Err(invalid("bucket policy scope count changed after resolution")); }
        for (bucket, scope) in self.profile.initial_buckets.iter().zip(scopes) {
            self.page.call(&storage::FrontcapRestoreBucketParams::new(
                storage::SerializedStorageKey(scope.key.clone()), native_bucket_policy(bucket))).await?;
        }
        self.verify_initial_bucket_policies(scopes).await
    }

    pub(super) async fn verify_initial_bucket_policies(&self, scopes: &[InitialStorageScope]) -> Result<(), ChromiumError> {
        use cdp::protocol::storage;
        if scopes.len() != self.profile.initial_buckets.len() { return Err(invalid("bucket policy scope count changed after resolution")); }
        for (bucket, scope) in self.profile.initial_buckets.iter().zip(scopes) {
            let policy = native_bucket_policy(bucket);
            let observed = self.page.call(&storage::FrontcapReadStorageScopeParams::new(
                storage::SerializedStorageKey(scope.key.clone()))).await?.scope;
            if observed.buckets.iter().find(|b| b.name == bucket.name) != Some(&policy) {
                return Err(ChromiumError::InterventionUncertain(format!("restored bucket policy differs for {} {:?}: expected {:?}, observed {:?}", bucket.origin, bucket.name, policy, observed.buckets.iter().find(|b| b.name == bucket.name))));
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct CapturedBucketStorage {
    pub buckets: Vec<frontcap_core::storage::InitialBucket>,
    pub caches: Vec<frontcap_core::cache::InitialCache>,
    pub databases: Vec<frontcap_core::indexed_db::InitialDatabase>,
    pub cache_excluded: Vec<String>,
    pub database_excluded: Vec<String>,
    pub excluded: Vec<String>,
}

impl Generation {
    pub(super) async fn live_bucket_storage(&mut self) -> Result<CapturedBucketStorage, ChromiumError> {
        use cdp::protocol::{cache_storage, storage};
        use frontcap_core::storage::{BucketDurability, InitialBucket, StoragePartition};
        let scopes = self.page.call(&storage::FrontcapReadStorageScopesParams::default()).await?.scopes;
        let mut capture = CapturedBucketStorage::default();
        for native in &scopes {
            if native.nonce_partition || crate::encode::canonical_origin_host(&native.origin).is_err() {
                capture.excluded.push(format!("Storage scope {}: opaque, non-HTTP(S) or nonce identity cannot be reconstructed", native.origin));
                continue;
            }
            let partition = StoragePartition { top_level_site: native.top_level_site.clone(), has_cross_site_ancestor: Some(native.has_cross_site_ancestor) };
            let scope = frontcap_core::intervention::StorageScope { origin: native.origin.clone(), storage_key: native.storage_key.0.clone() };
            for bucket in &native.buckets {
                let policy = InitialBucket {
                    origin: native.origin.clone(), partition: Some(partition.clone()), name: bucket.name.clone(),
                    expiration: bucket.expiration.clone(), quota: bucket.quota.clone(), persistent: bucket.persistent,
                    durability: match bucket.durability {
                        storage::StorageBucketsDurability::Strict => BucketDurability::Strict,
                        storage::StorageBucketsDurability::Relaxed => BucketDurability::Relaxed,
                    },
                };
                validate_initial_buckets(std::slice::from_ref(&policy))?;
                let grant = self.page.call(&storage::FrontcapGrantStorageScopeParams {
                    top_level_site: Some(partition.top_level_site.clone()),
                    has_cross_site_ancestor: partition.has_cross_site_ancestor,
                    bucket_name: bucket.name.clone(), create_if_missing: Some(false),
                    ..storage::FrontcapGrantStorageScopeParams::new(native.origin.clone())
                }).await?;
                if grant.storage_key != native.storage_key {
                    return Err(ChromiumError::InterventionUncertain("captured scope cannot be resolved to its native identity".into()));
                }
                self.page.call(&cache_storage::FrontcapBindStorageScopeParams {
                    storage_bucket_name: bucket.name.clone(),
                    ..cache_storage::FrontcapBindStorageScopeParams::new(native.storage_key.0.clone(), grant.scope_token)
                }).await?;
                let content = async {
                    let caches = self.live_caches(&scope, &native.origin, Some(&partition), bucket.name.as_deref()).await?;
                    let databases = self.live_indexed_db(&scope, &native.origin, Some(&partition), bucket.name.as_deref()).await?;
                    Ok::<_, ChromiumError>((caches, databases))
                }.await;
                let released = self.page.call(&cache_storage::FrontcapReleaseStorageScopeParams {
                    storage_bucket_name: bucket.name.clone(),
                    ..cache_storage::FrontcapReleaseStorageScopeParams::new(native.storage_key.0.clone())
                }).await;
                let ((caches, cache_excluded), (databases, database_excluded)) = content?;
                released?;
                let explain = |reason: String| format!("{reason} (origin {}, top-level site {}, bucket {})", native.origin, native.top_level_site, bucket.name.as_deref().unwrap_or("default"));
                capture.buckets.push(policy);
                capture.caches.extend(caches);
                capture.databases.extend(databases);
                capture.cache_excluded.extend(cache_excluded.into_iter().map(explain));
                capture.database_excluded.extend(database_excluded.into_iter().map(explain));
            }
        }
        if let Err(error) = super::cache::validate_initial_caches(&capture.caches) {
            capture.cache_excluded.push(format!("Cache Storage collection cannot be restored: {error}"));
            capture.caches.clear();
        }
        if let Err(error) = super::indexed_db::validate_initial_databases(&capture.databases) {
            capture.database_excluded.push(format!("IndexedDB collection cannot be restored: {error}"));
            capture.databases.clear();
        }
        let after = self.page.call(&storage::FrontcapReadStorageScopesParams::default()).await?.scopes;
        if after != scopes {
            return Err(ChromiumError::InterventionUncertain("storage inventory changed during recipe capture".into()));
        }
        Ok(capture)
    }
}
