use super::*;
use crate::encode::canonical_origin_host;
use cdp::protocol::cache_storage;
use frontcap_core::cache::{CacheInitializationRecord, InitialCache, CacheEntry, CacheRecord, CacheResponse, CacheResponseType, CacheResponseMetadata};
use frontcap_core::intervention::{StateField, StorageScope};
use frontcap_core::encode::unbase64;

fn invalid(message: &str) -> ChromiumError { ChromiumError::InvalidArgument(message.into()) }
fn headers_valid(headers: &BTreeMap<String, String>, native: bool, opaque: bool) -> bool {
    let mut names = std::collections::BTreeSet::new();
    headers.len() <= 64 && headers.iter().all(|(name, value)| {
        let lower = name.to_ascii_lowercase();
        !name.is_empty() && name.len() <= 1024
            && name.bytes().all(|b| b > 32 && b < 127 && !b"()<>@,;:\\\"/[]?={}\t".contains(&b))
            && value.len() <= 8192 && value.bytes().all(|b| b == b'\t' || (32..127).contains(&b))
            && names.insert(lower.clone()) && (native || !matches!(lower.as_str(), "set-cookie" | "set-cookie2"))
            && !(!opaque && lower == "vary" && value.split(',').any(|v| v.trim() == "*"))
    })
}

fn cache_origin_trustworthy(origin: &str) -> Result<bool, ChromiumError> {
    let host=canonical_origin_host(origin)?;
    let loopback=host.trim_matches(['[',']']).parse::<std::net::IpAddr>().is_ok_and(|ip|ip.is_loopback());
    Ok(origin.starts_with("https://") || host=="localhost" || host.ends_with(".localhost") || loopback)
}

pub fn validate_initial_caches(caches: &[InitialCache]) -> Result<(), ChromiumError> {
    if caches.len() > 16 || serde_json::to_vec(caches).unwrap().len() > 8 * 1024 * 1024 {
        return Err(invalid("initial caches exceed 16 caches or 8 MiB serialized inputs"));
    }
    let mut names = std::collections::BTreeSet::new();
    let mut count = 0;
    let mut bytes = 0usize;
    for cache in caches {
        super::buckets::validate_bucket_name(cache.bucket.as_deref())?;
        super::buckets::validate_partition(cache.partition.as_ref())?;
        if !cache_origin_trustworthy(&cache.origin)? {
            return Err(invalid("initial cache origin requires HTTPS or a trustworthy loopback host"));
        }
        if cache.name.is_empty() || cache.name.len() > 1024 || cache.name.contains('|') || cache.name.chars().any(char::is_control)
            || !names.insert((&cache.origin, &cache.partition, &cache.bucket, &cache.name)) {
            return Err(invalid("initial caches require unique origin/partition/bucket/name tuples, 1 to 1024 non-control name bytes without |,"));
        }
        let mut urls = std::collections::BTreeSet::new();
        for entry in &cache.entries {
            count += 1;
            let origin = origin_of(&entry.request_url).ok_or_else(|| invalid("cache request URL must be absolute HTTP(S)"))?;
            canonical_origin_host(&origin)?;
            let path = &entry.request_url[origin.len()..];
            let url_bytes = entry.request_url.as_bytes();
            if !path.starts_with('/') || entry.request_url.len() > 8192 || !url_bytes.iter().all(|b| (33..127).contains(b))
                || entry.request_url.contains(['#', '\\']) || !urls.insert(&entry.request_url)
                || url_bytes.iter().enumerate().any(|(i,b)| *b == b'%' && (i + 2 >= url_bytes.len() || !url_bytes[i+1].is_ascii_hexdigit() || !url_bytes[i+2].is_ascii_hexdigit()))
                || path.split('?').next().unwrap().split('/').any(|p| matches!(p.to_ascii_lowercase().replace("%2e", ".").as_str(), "." | "..")) {
                return Err(invalid("cache request URLs must be unique canonical HTTP(S) URLs with an absolute path and no fragment, credentials or dot segments"));
            }
            if let Some(metadata) = &entry.metadata { validate_metadata(metadata)?; }
            let opaque = entry.metadata.as_ref().is_some_and(|m| matches!(m.response_type, CacheResponseType::OpaqueResponse | CacheResponseType::OpaqueRedirect));
            let body = entry.body_bytes().ok_or_else(|| invalid("cache entry requires body text or valid body_base64, not both"))?;
            bytes = bytes.saturating_add(body.len());
            if count > 64 || bytes > 4 * 1024 * 1024 || !(200..=599).contains(&entry.status) || (entry.status == 206 && !opaque)
                || (matches!(entry.status, 204 | 205 | 304) && !body.is_empty())
                || entry.status_text.len() > 1024 || !entry.status_text.bytes().all(|b| b == b'\t' || (32..127).contains(&b))
                || !headers_valid(&entry.request_headers, false, false) || !headers_valid(&entry.headers, entry.metadata.is_some(), opaque) {
                return Err(invalid("invalid cache response, headers or body; initial caches allow 64 entries and 4 MiB decoded bodies"));
            }
        }
    }
    Ok(())
}

fn header_params(headers: &BTreeMap<String,String>) -> Vec<cache_storage::Header> {
    headers.iter().map(|(name,value)| cache_storage::Header { name: name.to_ascii_lowercase(), value: value.clone() }).collect()
}
fn header_values(headers: &[cache_storage::Header]) -> BTreeMap<String,String> {
    headers.iter().map(|h| (h.name.to_ascii_lowercase(), h.value.clone())).collect()
}

impl Generation {
    pub(super) async fn initialize_caches(&mut self, scopes: &[super::buckets::InitialStorageScope]) -> Result<(), ChromiumError> {
        let caches = self.profile.initial_caches.clone();
        if caches.is_empty() { return Ok(()); }
        if scopes.len() != caches.len() { return Err(invalid("cache scope count changed after resolution")); }
        for (cache, scope) in caches.into_iter().zip(scopes) {
            let key = &scope.key;
            self.bind_initial_storage_scope(scope, &cache.origin, cache.partition.as_ref(), cache.bucket.as_deref()).await?;
            let result = self.page.call(&cache_storage::FrontcapPutEntriesParams {
                storage_key: key.clone(), storage_bucket_name: cache.bucket.clone(),
                cache_name: cache.name.clone(), entries: cache.entries.iter().map(entry_params).collect(),
            }).await?;
            let inventory = self.page.call(&cache_storage::RequestEntriesParams::new(result.cache_id.clone())).await?;
            if inventory.cache_data_entries.len() != cache.entries.len() || inventory.return_count != cache.entries.len() as f64 {
                return Err(ChromiumError::InterventionUncertain("initial cache entry count did not match".into()));
            }
            for entry in &cache.entries {
                let matches: Vec<_> = inventory.cache_data_entries.iter().filter(|a| a.request_url == entry.request_url).collect();
                if matches.len() != 1 || !matches.iter().all(|a| a.request_method == "GET" && a.response_status == i64::from(entry.status)
                    && a.response_status_text == entry.status_text && a.response_type == native_type(&entry.metadata.as_ref().map(|m| m.response_type.clone()).unwrap_or_default())
                    && header_values(&a.request_headers) == header_values(&header_params(&entry.request_headers))
                    && header_values(&a.response_headers) == header_values(&header_params(&entry.headers))) {
                    return Err(ChromiumError::InterventionUncertain("initial cache metadata did not match".into()));
                }
                if let Some(expected) = &entry.metadata {
                    let full = self.page.call(&cache_storage::FrontcapReadEntryParams { cache_id: result.cache_id.clone(), request_url: entry.request_url.clone(), request_headers: header_params(&entry.request_headers) }).await?.response;
                    if full.map(|r| metadata_from_native(r.metadata)).as_ref() != Some(expected) {
                        return Err(ChromiumError::InterventionUncertain("initial native cache metadata did not match".into()));
                    }
                }
                let actual = self.page.call(&cache_storage::RequestCachedResponseParams::new(result.cache_id.clone(), entry.request_url.clone(), header_params(&entry.request_headers))).await?;
                if unbase64(&actual.response.body.0) != entry.body_bytes() {
                    return Err(ChromiumError::InterventionUncertain("initial cache body did not match".into()));
                }
            }
            let bucket = cache.bucket.clone();
            self.record(self.now, EventKind::CacheInitialized(CacheInitializationRecord { cache, storage_key: key.clone() }));
            self.commit()?;
            self.release_initial_storage_scope(scope, bucket.as_deref()).await?;
        }
        Ok(())
    }
}

fn entry_params(entry: &CacheEntry) -> cache_storage::FrontcapCacheEntry {
    cache_storage::FrontcapCacheEntry {
        request_url: entry.request_url.clone(), request_headers: header_params(&entry.request_headers),
        response_status: i64::from(entry.status), response_status_text: entry.status_text.clone(),
        response_headers: header_params(&entry.headers),
        body: cdp::Binary(base64(&entry.body_bytes().expect("validated cache body"))),
        metadata: entry.metadata.as_ref().map(metadata_params),
    }
}

fn field_cache(field: &StateField, response: Option<&CacheResponse>) -> Result<InitialCache, ChromiumError> {
    let StateField::CacheEntry { scope, bucket, cache, request_url, request_headers } = field else {
        return Err(invalid("expected a cache entry field"));
    };
    let entry = CacheEntry {
        request_url: request_url.clone(), request_headers: request_headers.clone(),
        status: response.map_or(200, |r| r.status),
        status_text: response.map(|r| r.status_text.clone()).unwrap_or_default(),
        headers: response.map(|r| r.headers.clone()).unwrap_or_default(),
        body: response.map(|r| r.body.clone()).unwrap_or_default(),
        body_base64: response.and_then(|r| r.body_base64.clone()),
        metadata: response.and_then(|r| r.metadata.clone()),
    };
    let cache = InitialCache { origin: scope.origin.clone(), partition: None, bucket: bucket.clone(), name: cache.clone(), entries: vec![entry] };
    validate_initial_caches(std::slice::from_ref(&cache))?;
    Ok(cache)
}

/// Normalize the public body representation for exact native readback and expected-value guards.
pub(super) fn normalize_cache_record(field: &StateField, value: &serde_json::Value, writing: bool) -> Result<serde_json::Value, ChromiumError> {
    field_cache(field, None)?;
    let mut record: CacheRecord = serde_json::from_value(value.clone()).map_err(|e| invalid(&e.to_string()))?;
    if let CacheRecord::Present { response } = &mut record {
        if writing {
            match &response.metadata {
                Some(metadata) if metadata.response_type == response.response_type && metadata.urls == response.urls => validate_metadata(metadata)?,
                Some(_) => return Err(invalid("cache metadata type and URL chain must match the response")),
                None if response.response_type == CacheResponseType::Default && response.urls.is_empty() => (),
                None => return Err(invalid("a fetched cache response requires its complete native metadata")),
            }
            field_cache(field, Some(response))?;
        }
        if response.body.len() > 4 * 1024 * 1024 || response.body_base64.as_ref().is_some_and(|s| s.len() > (4 * 1024 * 1024usize).div_ceil(3) * 4) {
            return Err(invalid("cache response exceeds 4 MiB"));
        }
        let body = match &response.body_base64 {
            Some(encoded) if response.body.is_empty() => unbase64(encoded).ok_or_else(|| invalid("invalid cache body_base64"))?,
            Some(_) => return Err(invalid("cache response accepts body or body_base64, not both")),
            None => response.body.as_bytes().to_vec(),
        };
        if body.len() > 4 * 1024 * 1024 { return Err(invalid("cache response exceeds 4 MiB")); }
        let mut headers = BTreeMap::new();
        for (name, value) in &response.headers {
            if headers.insert(name.to_ascii_lowercase(), value.clone()).is_some() {
                return Err(invalid("cache response contains duplicate header names"));
            }
        }
        response.headers = headers;
        response.body.clear();
        response.body_base64 = Some(base64(&body));
    }
    Ok(serde_json::to_value(record).expect("cache record"))
}

impl Generation {
    async fn cache_id(&self, scope: &StorageScope, bucket: Option<&str>, name: &str) -> Result<Option<cache_storage::CacheId>, ChromiumError> {
        use cdp::protocol::storage;
        let caches = self.page.call(&cache_storage::RequestCacheNamesParams {
            security_origin: None, storage_key: None,
            storage_bucket: Some(storage::StorageBucket { storage_key: storage::SerializedStorageKey(scope.storage_key.clone()), name: bucket.map(str::to_owned) }),
        }).await?.caches;
        Ok(caches.into_iter().find(|cache| cache.cache_name == name).map(|cache| cache.cache_id))
    }

    pub(super) async fn cache_record(&self, field: &StateField) -> Result<serde_json::Value, ChromiumError> {
        field_cache(field, None)?;
        let StateField::CacheEntry { scope, bucket, cache, request_url, request_headers } = field else { unreachable!() };
        let response = if let Some(cache_id) = self.cache_id(scope, bucket.as_deref(), cache).await? {
            self.page.call(&cache_storage::FrontcapReadEntryParams {
                cache_id, request_url: request_url.clone(), request_headers: header_params(request_headers),
            }).await?.response
        } else { None };
        let record = match response {
            None => CacheRecord::Absent,
            Some(response) => CacheRecord::Present { response: Box::new(CacheResponse {
                status: u16::try_from(response.response_status).map_err(|_| invalid("invalid native cache status"))?,
                status_text: response.response_status_text, headers: header_values(&response.response_headers),
                response_type: match response.response_type {
                    cache_storage::CachedResponseType::Basic => CacheResponseType::Basic,
                    cache_storage::CachedResponseType::Cors => CacheResponseType::Cors,
                    cache_storage::CachedResponseType::Default => CacheResponseType::Default,
                    cache_storage::CachedResponseType::Error => CacheResponseType::Error,
                    cache_storage::CachedResponseType::OpaqueResponse => CacheResponseType::OpaqueResponse,
                    cache_storage::CachedResponseType::OpaqueRedirect => CacheResponseType::OpaqueRedirect,
                },
                urls: response.response_ur_ls, body: String::new(), body_base64: Some(response.body.0),
                metadata: Some(metadata_from_native(response.metadata)),
            }) },
        };
        Ok(serde_json::to_value(record).expect("native cache record"))
    }

    pub(super) async fn write_cache_record(&self, field: &StateField, value: &serde_json::Value) -> Result<(), ChromiumError> {
        let value = normalize_cache_record(field, value, true)?;
        let record: CacheRecord = serde_json::from_value(value).expect("validated cache record");
        let StateField::CacheEntry { scope, bucket, cache, request_url, request_headers } = field else { unreachable!() };
        match record {
            CacheRecord::Absent => {
                if let Some(cache_id) = self.cache_id(scope, bucket.as_deref(), cache).await? {
                    self.page.call(&cache_storage::FrontcapDeleteEntryParams {
                        cache_id, request_url: request_url.clone(), request_headers: header_params(request_headers),
                    }).await?;
                }
            }
            CacheRecord::Present { response } => {
                let cache = field_cache(field, Some(&response))?;
                self.page.call(&cache_storage::FrontcapPutEntriesParams {
                    storage_key: scope.storage_key.clone(), storage_bucket_name: bucket.clone(),
                    cache_name: cache.name, entries: cache.entries.iter().map(entry_params).collect(),
                }).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

fn native_type(value: &CacheResponseType) -> cache_storage::CachedResponseType {
    match value {
        CacheResponseType::Default => cache_storage::CachedResponseType::Default,
        CacheResponseType::Basic => cache_storage::CachedResponseType::Basic,
        CacheResponseType::Cors => cache_storage::CachedResponseType::Cors,
        CacheResponseType::Error => cache_storage::CachedResponseType::Error,
        CacheResponseType::OpaqueResponse => cache_storage::CachedResponseType::OpaqueResponse,
        CacheResponseType::OpaqueRedirect => cache_storage::CachedResponseType::OpaqueRedirect,
    }
}
fn metadata_params(value: &CacheResponseMetadata) -> cache_storage::FrontcapResponseMetadata {
    cache_storage::FrontcapResponseMetadata {
        response_type: native_type(&value.response_type), response_ur_ls: value.urls.clone(),
        cors_exposed_header_names: value.cors_exposed_header_names.clone(), mime_type: value.mime_type.clone(),
        request_method: value.request_method.clone(), response_time: value.response_time.clone(),
        padding: value.padding.clone(), connection_info: value.connection_info,
        alpn_negotiated_protocol: value.alpn_negotiated_protocol.clone(), was_fetched_via_spdy: value.was_fetched_via_spdy,
        request_include_credentials: value.request_include_credentials,
    }
}
fn metadata_from_native(value: cache_storage::FrontcapResponseMetadata) -> CacheResponseMetadata {
    CacheResponseMetadata {
        response_type: match value.response_type {
            cache_storage::CachedResponseType::Default => CacheResponseType::Default,
            cache_storage::CachedResponseType::Basic => CacheResponseType::Basic,
            cache_storage::CachedResponseType::Cors => CacheResponseType::Cors,
            cache_storage::CachedResponseType::Error => CacheResponseType::Error,
            cache_storage::CachedResponseType::OpaqueResponse => CacheResponseType::OpaqueResponse,
            cache_storage::CachedResponseType::OpaqueRedirect => CacheResponseType::OpaqueRedirect,
        },
        urls: value.response_ur_ls, cors_exposed_header_names: value.cors_exposed_header_names,
        mime_type: value.mime_type, request_method: value.request_method,
        response_time: value.response_time, padding: value.padding, connection_info: value.connection_info,
        alpn_negotiated_protocol: value.alpn_negotiated_protocol, was_fetched_via_spdy: value.was_fetched_via_spdy,
        request_include_credentials: value.request_include_credentials,
    }
}
fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b > 32 && b < 127 && !b"()<>@,;:\\\"/[]?={}\t".contains(&b))
}
fn valid_header_value(s: &str) -> bool { s.bytes().all(|b| b == b'\t' || (32..127).contains(&b)) }
fn validate_metadata(value: &CacheResponseMetadata) -> Result<(), ChromiumError> {
    let decimal = |s: &str| s.parse::<i64>().ok().filter(|n| n.to_string() == s);
    let padding = decimal(&value.padding);
    let opaque = matches!(value.response_type, CacheResponseType::OpaqueResponse | CacheResponseType::OpaqueRedirect);
    if value.response_type == CacheResponseType::Error || decimal(&value.response_time).is_none()
        || padding.is_none_or(|p| !(0..32*1024*1024).contains(&p) || (!opaque && p != 0))
        || !(0..=42).contains(&value.connection_info) || value.urls.len() > 20
        || (value.response_type != CacheResponseType::Default && value.urls.is_empty())
        || value.cors_exposed_header_names.len() > 64
        || value.cors_exposed_header_names.iter().any(|s| s.len()>1024 || !valid_token(s))
        || value.mime_type.as_ref().is_some_and(|s| s.len()>1024 || !valid_header_value(s))
        || value.request_method.as_ref().is_some_and(|s| s.len()>32 || !valid_token(s))
        || value.alpn_negotiated_protocol.len()>1024 || !valid_header_value(&value.alpn_negotiated_protocol) {
        return Err(invalid("invalid native cache response metadata"));
    }
    for url in &value.urls {
        let origin = origin_of(url).ok_or_else(|| invalid("cache response URL must be absolute HTTP(S)"))?;
        canonical_origin_host(&origin)?;
        let path = &url[origin.len()..];
        let bytes = url.as_bytes();
        if !path.starts_with('/') || url.len()>8192 || url.contains(['#','\\'])
            || !bytes.iter().all(|b| (33..127).contains(b))
            || bytes.iter().enumerate().any(|(i,b)| *b == b'%' && (i+2>=bytes.len() || !bytes[i+1].is_ascii_hexdigit() || !bytes[i+2].is_ascii_hexdigit()))
            || path.split('?').next().unwrap().split('/').any(|p| matches!(p.to_ascii_lowercase().replace("%2e", ".").as_str(), "." | "..")) {
            return Err(invalid("invalid native cache response URL"));
        }
    }
    Ok(())
}
/// New synthetic responses let the engine choose native metadata; restoration compares it exactly.
pub(super) fn cache_write_matches(wanted: &serde_json::Value, actual: &serde_json::Value) -> bool {
    if wanted == actual { return true; }
    if wanted["state"] != "present" || wanted["response"].get("metadata").is_some() { return false; }
    let mut comparable = actual.clone();
    if let Some(response) = comparable.get_mut("response").and_then(serde_json::Value::as_object_mut) { response.remove("metadata"); }
    &comparable == wanted
}

impl Generation {
    /// Capture one bucket in native key order. Never substitute a partial cache for its source.
    pub(super) async fn live_caches(&self, scope: &StorageScope, origin: &str, partition: Option<&frontcap_core::storage::StoragePartition>, bucket: Option<&str>) -> Result<(Vec<InitialCache>, Vec<String>), ChromiumError> {
        use cdp::protocol::storage;
        if !cache_origin_trustworthy(origin)? {
            return Ok((vec![],vec!["Cache Storage is unavailable for this untrustworthy origin".into()]));
        }
        let names=self.page.call(&cache_storage::RequestCacheNamesParams {
            security_origin:None,storage_key:None,
            storage_bucket:Some(storage::StorageBucket {storage_key:storage::SerializedStorageKey(scope.storage_key.clone()),name:bucket.map(str::to_owned)}),
        }).await?.caches;
        if names.len()>16 {return Ok((vec![],vec!["Cache Storage: more than 16 caches".into()]));}
        let (mut caches,mut excluded)=(Vec::new(),Vec::new());
        'caches: for cache in names {
            let declaration=InitialCache {origin:origin.into(),partition:partition.cloned(),bucket:bucket.map(str::to_owned),name:cache.cache_name.clone(),entries:vec![]};
            if let Err(error)=validate_initial_caches(std::slice::from_ref(&declaration)) {
                excluded.push(format!("Cache Storage cache {}: {error}",cache.cache_name));continue;
            }
            let mut params=cache_storage::RequestEntriesParams::new(cache.cache_id.clone());
            params.page_size=Some(65);params.frontcap_preserve_order=Some(true);
            let inventory=self.page.call(&params).await?;
            if inventory.return_count>64.0 || inventory.return_count!=inventory.cache_data_entries.len() as f64 {
                excluded.push(format!("Cache Storage cache {}: more than 64 entries or incomplete inventory",cache.cache_name));continue;
            }
            let mut entries=Vec::new();
            for entry in inventory.cache_data_entries {
                if entry.request_method!="GET" {
                    excluded.push(format!("Cache Storage cache {}: non-GET request cannot be restored",cache.cache_name));continue 'caches;
                }
                let mut request_check=declaration.clone();
                request_check.entries.push(CacheEntry {request_url:entry.request_url.clone(),request_headers:header_values(&entry.request_headers),
                    status:200,status_text:String::new(),headers:BTreeMap::new(),body:String::new(),body_base64:None,metadata:None});
                if let Err(error)=validate_initial_caches(std::slice::from_ref(&request_check)) {
                    excluded.push(format!("Cache Storage cache {}: {error}",cache.cache_name));continue 'caches;
                }
                let response=self.page.call(&cache_storage::FrontcapReadEntryParams {
                    cache_id:cache.cache_id.clone(),request_url:entry.request_url.clone(),request_headers:entry.request_headers.clone(),
                }).await?.response.ok_or_else(|| ChromiumError::InterventionUncertain("cache entry disappeared during recipe capture".into()))?;
                entries.push(CacheEntry {request_url:entry.request_url,request_headers:header_values(&entry.request_headers),
                    status:u16::try_from(response.response_status).map_err(|_|invalid("invalid stored cache status"))?,
                    status_text:response.response_status_text,headers:header_values(&response.response_headers),
                    body:String::new(),body_base64:Some(response.body.0),metadata:Some(metadata_from_native(response.metadata)),
                });
            }
            let captured=InitialCache {origin:origin.into(),partition:partition.cloned(),bucket:bucket.map(str::to_owned),name:cache.cache_name,entries};
            if let Err(error)=validate_initial_caches(std::slice::from_ref(&captured)) {
                excluded.push(format!("Cache Storage cache {}: {error}",captured.name));continue;
            }
            caches.push(captured);
        }
        if let Err(error)=validate_initial_caches(&caches) {
            excluded.push(format!("Cache Storage collection cannot be restored: {error}"));caches.clear();
        }
        Ok((caches,excluded))
    }
}
