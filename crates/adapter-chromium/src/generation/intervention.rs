use super::*;
use frontcap_core::intervention::{AppliedState, IndexedDbRecord, ChangeReceipt, StateChange, StateField, InterventionRecord, StateValue, StorageArea, StorageScope, ScopedStorage};
use frontcap_core::observation::ObservationBoundary;

pub fn validate_initial_storage(initial: Option<&frontcap_core::intervention::InitialStorage>) -> Result<(), ChromiumError> {
    let Some(initial) = initial else { return Ok(()) };
    if !initial.origin.starts_with("http://") && !initial.origin.starts_with("https://")
        || origin_of(&initial.origin).as_deref() != Some(initial.origin.as_str()) {
        return Err(ChromiumError::InvalidArgument("initial storage requires an explicit canonical HTTP(S) origin".into()));
    }
    if initial.local.len() + initial.session.len() > 64
        || initial.local.iter().chain(&initial.session).any(|(key, value)| key.len() > 1024 || value.len() > 65536)
        || serde_json::to_vec(initial).unwrap().len() > 262144 {
        return Err(ChromiumError::InvalidArgument("initial storage exceeds 64 keys, 1 KiB keys, 64 KiB values or 256 KiB total".into()));
    }
    Ok(())
}

/// Resolved once before dispatch, then persisted by the recorder. Recovery never resolves a locator
/// again into a different node or re-evaluates a batch's initial boundary.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatePlan {
    pub document: u64,
    pub changes: Vec<StateChange>,
    pub nodes: Vec<i64>,
    /// Per change, the virtual time its app provider's write may be given: the call's grant within the
    /// provider's `max_ms`; 0 for other fields.
    #[serde(default)]
    pub progress_us: Vec<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct StateStep {
    phase: String,
    before: Option<serde_json::Value>,
    error: Option<String>,
    receipt: Option<ChangeReceipt>,
    /// The token of an app provider's write whose promise had not settled when the call returned.
    #[serde(default)]
    pending: Option<String>,
    /// When the write's progress began, in virtual microseconds (a string), once it has.
    #[serde(default)]
    progress_from: Option<String>,
}

/// An IndexedDB key path as JSON: null, a string or an array of strings.
fn key_path(path: &cdp::protocol::indexed_db::KeyPath) -> serde_json::Value {
    use cdp::protocol::indexed_db::KeyPathType;
    match path.r#type {
        KeyPathType::Null => serde_json::Value::Null,
        KeyPathType::String => serde_json::json!(path.string),
        KeyPathType::Array => serde_json::json!(path.array),
    }
}

/// An IndexedDB key path as a recipe's; `None` for no key path.
fn core_key_path(path: &cdp::protocol::indexed_db::KeyPath) -> Option<frontcap_core::indexed_db::KeyPath> {
    use cdp::protocol::indexed_db::KeyPathType;
    use frontcap_core::indexed_db::KeyPath;
    match path.r#type {
        KeyPathType::Null => None,
        KeyPathType::String => path.string.clone().map(KeyPath::String),
        KeyPathType::Array => path.array.clone().map(KeyPath::Array),
    }
}

/// A JSON key as IndexedDB's: a number, a string or an array of keys.
fn native_key(key: &serde_json::Value) -> Option<cdp::protocol::indexed_db::Key> {
    use cdp::protocol::indexed_db::{Key, KeyType};
    fn valid(key: &serde_json::Value, depth: usize, nodes: &mut usize) -> bool {
        *nodes += 1;
        if depth > 32 || *nodes > 4096 { return false; }
        match key {
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|v| v.is_finite() && (v.fract() != 0.0 || v.abs() <= 9_007_199_254_740_991.0)),
            serde_json::Value::String(s) => s.encode_utf16().count() <= 8192,
            serde_json::Value::Array(a) => a.iter().all(|k| valid(k, depth + 1, nodes)),
            _ => false,
        }
    }
    if !valid(key, 0, &mut 0) { return None; }
    let empty = |r#type| Key { r#type, number: None, string: None, date: None, array: None };
    Some(match key {
        serde_json::Value::Number(n) => Key { number: Some(n.as_f64()?), ..empty(KeyType::Number) },
        serde_json::Value::String(s) => Key { string: Some(s.clone()), ..empty(KeyType::String) },
        serde_json::Value::Array(items) => Key { array: Some(items.iter().map(native_key).collect::<Option<Vec<_>>>()?), ..empty(KeyType::Array) },
        _ => return None,
    })
}

/// Tokens of app provider reads: unique within the renderer's outcome table.
static READ_TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The most zero-length grants a write's promise is served at its instant.
const INSTANT_SERVINGS: usize = 8;

/// How a provider write's promise settled: whether the instant was served for it, the virtual time it
/// was given, and its error if it failed or did not settle.
struct Settled {
    served: bool,
    progressed_us: Option<u64>,
    error: Option<String>,
}

fn field_expression(field: &StateField, write: Option<&serde_json::Value>) -> Result<String, ChromiumError> {
    let quoted = |s: &str| serde_json::to_string(s).unwrap();
    let named = |name: &str| -> Result<String, ChromiumError> {
        if name.is_empty() || name.len() > 256 || name.chars().any(|c| c.is_control()) {
            return Err(ChromiumError::InvalidArgument("field name must contain 1 to 256 non-control characters".into()));
        }
        Ok(quoted(name))
    };
    let storage = |area: &StorageArea| match area { StorageArea::Local => "localStorage", StorageArea::Session => "sessionStorage" };
    if write.is_some() && !field.writable() {
        return Err(ChromiumError::InvalidArgument("storage keys and cookies are read-only fields".into()));
    }
    let read = match field {
        StateField::StorageKeys { area, .. } => {
            format!("(s => Array.from({{length: s.length}}, (_, i) => s.key(i)))(this.ownerDocument.defaultView.{})", storage(area))
        }
        StateField::Cookies | StateField::Cookie { .. } | StateField::IndexedDb { .. } | StateField::IndexedDbRecord { .. } | StateField::CacheEntry { .. } => return Err(ChromiumError::InvalidArgument("storage records and cookies are read natively, not by expression".into())),
        StateField::AppProviders => "globalThis.__frontcap.appState(this.ownerDocument, 'list').providers".into(),
        StateField::App { provider } => {
            if provider.is_empty() || provider.len() > 64 || !provider.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                return Err(ChromiumError::InvalidArgument("a state provider id is 1 to 64 letters, digits, '.', '_' or '-'".into()));
            }
            format!("globalThis.__frontcap.appState(this.ownerDocument, 'read', {}, null, 'read-{}')", quoted(provider),
                READ_TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }
        StateField::Storage { area, key, .. } => {
            if key.len() > 1024 { return Err(ChromiumError::InvalidArgument("storage key exceeds 1024 bytes".into())); }
            format!("this.ownerDocument.defaultView.{}.getItem({})", storage(area), quoted(key))
        }
        StateField::Value => "this.value".into(),
        StateField::Checked => "this.checked".into(),
        StateField::Text => "this.textContent".into(),
        StateField::Attribute { name } => format!("this.getAttribute({})", named(name)?),
        StateField::Style { name } => format!("this.style.getPropertyValue({})", named(name)?),
    };
    let Some(value) = write else { return Ok(read) };
    let valid = match field {
        StateField::Checked => value.is_boolean(),
        StateField::Attribute { .. } | StateField::Storage { .. } => value.is_string() || value.is_null(),
        // Any JSON value: the provider's own.
        StateField::App { .. } => true,
        _ => value.is_string(),
    };
    if !valid || value.to_string().len() > 65536 {
        return Err(ChromiumError::InvalidArgument("field value has the wrong type or exceeds 64 KiB".into()));
    }
    let value = value.to_string();
    Ok(match field {
        StateField::Storage { area, key, .. } => if value == "null" {
            format!("this.ownerDocument.defaultView.{}.removeItem({})", storage(area), quoted(key))
        } else { format!("this.ownerDocument.defaultView.{}.setItem({}, {value})", storage(area), quoted(key)) },
        StateField::Attribute { name } => if value == "null" {
            format!("this.removeAttribute({})", quoted(name))
        } else { format!("this.setAttribute({}, {value})", quoted(name)) },
        StateField::Style { name } => format!("this.style.setProperty({}, {value})", quoted(name)),
        // `token` is the dispatching call's, naming this item.
        StateField::App { provider } => format!("globalThis.__frontcap.appState(this.ownerDocument, 'write', {}, {}, token)", quoted(provider), quoted(&value)),
        _ => format!("{read} = {value}"),
    })
}

impl Generation {
    /// The initial navigation has committed under paused time, but no app script has run. Use native
    /// browser storage commands here; no sacrificial origin page or per-navigation injection script.
    pub(super) async fn initialize_storage(&mut self) -> Result<(), ChromiumError> {
        use cdp::protocol::{dom_storage, storage};
        let Some(initial) = self.profile.initial_storage.clone() else { return Ok(()) };
        let frame = self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
        let origin = origin_of(&frame.url).ok_or_else(|| ChromiumError::Unsupported("initial document has no HTTP(S) storage origin".into()))?;
        if origin != initial.origin {
            return Err(ChromiumError::InvalidArgument(format!("initial storage origin {} does not match committed document {origin}", initial.origin)));
        }
        let key = self.page.call(&storage::GetStorageKeyForFrameParams::new(frame.id)).await?.storage_key.0;
        for (local, values) in [(true, &initial.local), (false, &initial.session)] {
            let id = dom_storage::StorageId { security_origin: None, storage_key: Some(dom_storage::SerializedStorageKey(key.clone())), is_local_storage: local };
            for (key, value) in values {
                self.page.call(&dom_storage::SetDomStorageItemParams::new(id.clone(), key.clone(), value.clone())).await?;
            }
            let actual: BTreeMap<String, String> = self.page.call(&dom_storage::GetDomStorageItemsParams::new(id)).await?.entries
                .into_iter().filter_map(|item| { let [key, value] = item.0.as_slice() else { return None }; Some((key.clone(), value.clone())) }).collect();
            if values.iter().any(|(key, value)| actual.get(key) != Some(value)) {
                return Err(ChromiumError::InterventionUncertain("initial storage readback did not match; generation is not ready".into()));
            }
        }
        self.record(self.now, EventKind::Initialization(frontcap_core::intervention::InitializationRecord { storage: initial, storage_key: key }));
        self.commit()?;
        Ok(())
    }

    async fn storage_scope_node(&mut self, node: dom::BackendNodeId) -> Result<StorageScope, ChromiumError> {
        let object = self.observed_object(node).await?;
        // Window.origin is the effective inherited origin for about:blank/srcdoc, unlike the
        // frame URL or the CDP frame-tree securityOrigin for those documents. Read its isolated wrapper.
        let origin = self.call_on(&object, "function() { return this.ownerDocument.defaultView.origin; }", vec![]).await?
            .as_str().ok_or_else(|| ChromiumError::Unsupported("storage context has no origin".into()))?.to_string();
        if origin == "null" || origin.is_empty() {
            return Err(ChromiumError::Unsupported("opaque-origin documents have no supported web storage scope".into()));
        }
        let owner = self.page.call(&runtime::CallFunctionOnParams {
            object_id: Some(object), return_by_value: Some(false),
            ..runtime::CallFunctionOnParams::new("function() { return this.ownerDocument.defaultView.frameElement; }".into())
        }).await?;
        if let Some(error) = owner.exception_details { return Err(ChromiumError::Unsupported(format!("storage context: {}", error.text))); }
        let frame = if let Some(object) = owner.result.object_id {
            let described = self.page.call(&dom::DescribeNodeParams { object_id: Some(object.clone()), ..Default::default() }).await;
            let _ = self.page.call(&runtime::ReleaseObjectParams::new(object)).await;
            described?.node.frame_id.ok_or_else(|| ChromiumError::Unsupported("storage context has no frame identity".into()))?
        } else { self.main_frame.clone() };
        let key = self.page.call(&cdp::protocol::storage::GetStorageKeyForFrameParams::new(frame)).await?.storage_key.0;
        Ok(StorageScope { origin, storage_key: key })
    }

    pub async fn storage_scope(&mut self, target: &Locator) -> Result<ScopedStorage, ChromiumError> {
        let boundary = self.observation_boundary().await?;
        let node = self.resolve(target).await?;
        let scope = self.storage_scope_node(node.clone()).await?;
        self.expect_boundary(&boundary).await?;
        Ok(ScopedStorage { boundary, r#ref: self.ref_for(node.0), scope })
    }

    async fn read_state_field(&mut self, node: dom::BackendNodeId, field: &StateField) -> Result<serde_json::Value, ChromiumError> {
        if let StateField::Cookies = field {
            return self.document_cookies(node).await;
        }
        if let StateField::Cookie { .. } = field {
            return self.cookie_read(node, field).await;
        }
        if let StateField::IndexedDb { scope, bucket, database, store, key } = field {
            if &self.storage_scope_node(node).await? != scope {
                return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
            }
            return self.indexed_db_read(scope, bucket.as_deref(), database.as_deref(), store.as_deref(), key.as_ref()).await;
        }
        if let StateField::IndexedDbRecord { scope, bucket, database, store, key } = field {
            if &self.storage_scope_node(node).await? != scope {
                return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
            }
            return self.indexed_db_record(scope, bucket.as_deref(), database, store, key).await;
        }
        if let StateField::CacheEntry { scope, .. } = field {
            if !self.patched { return Err(ChromiumError::Unsupported("native cache reads require the patched engine".into())); }
            if &self.storage_scope_node(node).await? != scope {
                return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
            }
            return self.cache_record(field).await;
        }
        let expression = field_expression(field, None)?;
        if let StateField::Storage { scope, .. } | StateField::StorageKeys { scope, .. } = field
            && &self.storage_scope_node(node.clone()).await? != scope {
                return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
            }
        let object = self.observed_object(node).await?;
        let allowed = match field {
            StateField::Value => "['input','textarea','select'].includes(this.localName)",
            StateField::Checked => "this.localName === 'input' && ['checkbox','radio'].includes(this.type)",
            _ => "this.nodeType === 1",
        };
        if self.call_on(&object, &format!("function() {{ return {allowed}; }}"), vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::InvalidArgument("field is not supported by the target element".into()));
        }
        if let StateField::App { .. } | StateField::AppProviders = field {
            // The app's provider answers: its failure is reported with its message, a missing provider
            // as the caller's.
            let answer = self.call_on(&object, &format!("function() {{ try {{ return {{ value: {expression} }}; }} catch (e) {{ return {{ error: String(e && e.message || e) }}; }} }}"), vec![]).await?;
            let provided = match answer["error"].as_str() {
                Some(e) if e.starts_with("no state provider") || e.starts_with("the document has no state provider") => return Err(ChromiumError::InvalidArgument(e.into())),
                Some(e) => return Err(ChromiumError::Script(e.into())),
                None => answer["value"].clone(),
            };
            let StateField::App { provider } = field else { return Ok(provided) };
            // A read is an observation: a promise is asked for again in another task, after the call's
            // microtasks, and no page task runs for it.
            let text = match (provided["value"].as_str(), provided["pending"].as_str()) {
                (Some(text), _) => text.to_string(),
                (None, Some(token)) => match self.app_outcome(&object, token).await? {
                    (Some(text), None) => text,
                    (_, Some(error)) => return Err(ChromiumError::Script(error)),
                    (None, None) => return Err(ChromiumError::Script(format!(
                        "{provider}'s read() did not settle on microtasks; a provider reads what it holds, without the page's tasks"))),
                },
                _ => return Err(ChromiumError::Shape(format!("state provider reply {provided}"))),
            };
            return serde_json::from_str(&text).map_err(|e| ChromiumError::Script(format!("{provider}'s read() value: {e}")));
        }
        self.call_on(&object, &format!("function() {{ return {expression}; }}"), vec![]).await
    }

    /// IndexedDB of `scope`, read through the inspector's own connection to the database service, which
    /// runs no page task (engine patch 61): the databases, a database's stores, a store's first records, or
    /// one record.
    async fn indexed_db_read(&mut self, scope: &frontcap_core::intervention::StorageScope, bucket: Option<&str>, database: Option<&str>, store: Option<&str>, key: Option<&serde_json::Value>) -> Result<serde_json::Value, ChromiumError> {
        use cdp::protocol::{indexed_db, storage};
        const RECORDS: i64 = 100;
        let bucket = Some(storage::StorageBucket { storage_key: storage::SerializedStorageKey(scope.storage_key.clone()), name: bucket.map(str::to_string) });
        let describe = |name: &str| indexed_db::RequestDatabaseParams { security_origin: None, storage_key: None, storage_bucket: bucket.clone(), database_name: name.into() };
        let Some(database) = database else {
            let mut names = self.page.call(&indexed_db::RequestDatabaseNamesParams { security_origin: None, storage_key: None, storage_bucket: bucket.clone() }).await?.database_names;
            names.sort();
            let mut databases = Vec::new();
            for name in names {
                let version = self.page.call(&describe(&name)).await.map_err(|e| idb_refusal(e, &name, None))?.database_with_object_stores.version;
                databases.push(serde_json::json!({"name": name, "version": version as u64}));
            }
            return Ok(serde_json::Value::Array(databases));
        };
        let count = |store: &str| indexed_db::GetMetadataParams { security_origin: None, storage_key: None, storage_bucket: bucket.clone(), database_name: database.into(), object_store_name: store.into() };
        let Some(store) = store else {
            let mut described = self.page.call(&describe(database)).await.map_err(|e| idb_refusal(e, database, None))?.database_with_object_stores;
            // The engine lists stores in its map's order.
            described.object_stores.sort_by(|a, b| a.name.cmp(&b.name));
            let mut stores = Vec::new();
            for s in described.object_stores {
                let records = self.page.call(&count(&s.name)).await.map_err(|e| idb_refusal(e, database, Some(&s.name)))?.entries_count as u64;
                stores.push(serde_json::json!({
                    "name": s.name, "key_path": key_path(&s.key_path), "auto_increment": s.auto_increment,
                    "indexes": s.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(), "count": records,
                }));
            }
            return Ok(serde_json::json!({"name": described.name, "version": described.version as u64, "stores": stores}));
        };
        let key_range = match key {
            Some(k) => {
                let k = native_key(k).ok_or_else(|| ChromiumError::InvalidArgument("an IndexedDB key is a number, a string or an array of keys".into()))?;
                Some(indexed_db::KeyRange { lower: Some(k.clone()), upper: Some(k), lower_open: false, upper_open: false })
            }
            None => None,
        };
        let data = self.page.call(&indexed_db::RequestDataParams {
            security_origin: None, storage_key: None, storage_bucket: bucket.clone(), database_name: database.into(),
            object_store_name: store.into(), index_name: None, skip_count: 0, page_size: if key.is_some() { 1 } else { RECORDS }, key_range,
        }).await.map_err(|e| idb_refusal(e, database, Some(store)))?;
        let mut records = Vec::new();
        for entry in data.object_store_data_entries {
            let key = self.remote_value(entry.primary_key).await?;
            let value = self.remote_value(entry.value).await?;
            records.push(serde_json::json!({"key": key, "value": value}));
        }
        if key.is_some() {
            return Ok(records.pop().map(|r| r["value"].clone()).unwrap_or(serde_json::Value::Null));
        }
        let total = self.page.call(&count(store)).await.map_err(|e| idb_refusal(e, database, Some(store)))?.entries_count as u64;
        Ok(serde_json::json!({"records": records, "total": total}))
    }

    async fn indexed_db_record(&mut self, scope: &StorageScope, bucket: Option<&str>, database: &str, store: &str, key: &serde_json::Value) -> Result<serde_json::Value, ChromiumError> {
        use cdp::protocol::{indexed_db, storage};
        let key = native_key(key).ok_or_else(|| ChromiumError::InvalidArgument("invalid IndexedDB key".into()))?;
        let mut result = self.page.call(&indexed_db::RequestDataParams {
            security_origin: None, storage_key: None,
            storage_bucket: Some(storage::StorageBucket { storage_key: storage::SerializedStorageKey(scope.storage_key.clone()), name: bucket.map(str::to_owned) }),
            database_name: database.into(), object_store_name: store.into(), index_name: None,
            skip_count: 0, page_size: 1,
            key_range: Some(indexed_db::KeyRange { lower: Some(key.clone()), upper: Some(key), lower_open: false, upper_open: false }),
        }).await.map_err(|e| idb_refusal(e, database, Some(store)))?;
        let record = match result.object_store_data_entries.pop() {
            None => IndexedDbRecord::Absent,
            Some(entry) => {
                let value = self.faithful_json(entry.value, &mut 65536).await?
                    .ok_or_else(|| ChromiumError::InvalidArgument("IndexedDB record is not a bounded JSON value".into()))?;
                IndexedDbRecord::Present { value }
            }
        };
        Ok(serde_json::to_value(record).expect("JSON record"))
    }

    async fn native_state_write(&mut self, node: dom::BackendNodeId, field: &StateField, value: &serde_json::Value) -> Result<(), ChromiumError> {
        use cdp::protocol::{indexed_db, storage};
        if let StateField::CacheEntry { scope, .. } = field {
            if &self.storage_scope_node(node).await? != scope {
                return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
            }
            return self.write_cache_record(field, value).await;
        }
        let StateField::IndexedDbRecord { scope, bucket, database, store, key } = field else {
            return self.cookie_write(node, field, value).await;
        };
        validate_record_write(field, value)?;
        if &self.storage_scope_node(node).await? != scope {
            return Err(ChromiumError::InvalidArgument("storage origin/partition does not match the target document".into()));
        }
        let record: IndexedDbRecord = serde_json::from_value(value.clone()).expect("validated record");
        let value_json = match record { IndexedDbRecord::Present { value } => Some(value.to_string()), IndexedDbRecord::Absent => None };
        self.page.call(&indexed_db::FrontcapWriteRecordParams {
            storage_bucket: storage::StorageBucket { storage_key: storage::SerializedStorageKey(scope.storage_key.clone()), name: bucket.clone() },
            database_name: database.clone(), object_store_name: store.clone(), key: native_key(key).expect("validated key"),
            remove: value_json.is_none(), value_json,
        }).await.map_err(|e| idb_refusal(e, database, Some(store)))?;
        Ok(())
    }

    /// A remote object's value, by value; reading it runs no page task.
    async fn remote_value(&mut self, object: runtime::RemoteObject) -> Result<serde_json::Value, ChromiumError> {
        let Some(id) = object.object_id else { return Ok(object.value.unwrap_or(serde_json::Value::Null)) };
        let value = self.page.call(&runtime::CallFunctionOnParams {
            object_id: Some(id), return_by_value: Some(true), ..runtime::CallFunctionOnParams::new("function() { return this; }".into())
        }).await?;
        Ok(value.result.value.unwrap_or(serde_json::Value::Null))
    }

    /// A cookie field's identity in the node's document: its origin, host, name, path, domain and
    /// partition.
    async fn cookie_identity(&mut self, node: dom::BackendNodeId, field: &StateField) -> Result<(String, String, String, String, Option<String>, Option<frontcap_core::intervention::CookiePartition>), ChromiumError> {
        let StateField::Cookie { name, path, domain, partition } = field else { unreachable!("a cookie field") };
        let object = self.observed_object(node).await?;
        let url = self.call_on(&object, "function() { return this.ownerDocument.URL; }", vec![]).await?
            .as_str().map(str::to_string).ok_or_else(|| ChromiumError::Unsupported("the target document has no URL".into()))?;
        let origin = crate::encode::origin_of(&url).ok_or_else(|| ChromiumError::InvalidArgument("the target document has no origin for cookies".into()))?;
        let host = crate::encode::canonical_origin_host(&origin)?.to_string();
        Ok((origin, host, name.clone(), path.clone().unwrap_or_else(|| "/".into()), domain.clone(), partition.clone()))
    }

    /// One cookie as the field reads it: `{value, expires_at_ms?, secure, http_only, same_site}`, or null.
    async fn cookie_read(&mut self, node: dom::BackendNodeId, field: &StateField) -> Result<serde_json::Value, ChromiumError> {
        let (_, host, name, path, domain, partition) = self.cookie_identity(node, field).await?;
        let domain = domain.unwrap_or(host);
        let cookies = self.page.call(&cdp::protocol::storage::GetCookiesParams::default()).await?.cookies;
        let found = cookies.into_iter().find(|c| c.name == name && c.domain == domain && c.path == path && match (&c.partition_key, &partition) {
            (None, None) => true,
            (Some(a), Some(p)) => a.top_level_site == p.top_level_site && a.has_cross_site_ancestor == p.has_cross_site_ancestor,
            _ => false,
        });
        Ok(match found {
            None => serde_json::Value::Null,
            Some(c) => {
                let mut v = serde_json::json!({
                    "value": c.value, "secure": c.secure, "http_only": c.http_only,
                    "same_site": c.same_site.as_ref().and_then(|s| serde_json::to_value(s).ok()).and_then(|v| v.as_str().map(str::to_lowercase)).unwrap_or_else(|| "lax".into()),
                });
                if !c.session { v["expires_at_ms"] = serde_json::json!((c.expires * 1000.0).round() as u64) }
                v
            }
        })
    }

    /// A cookie write's value as read back (attributes at their defaults) and the cookie it sets, none
    /// for a deletion; refused as a launch's initial cookie would be, its expiry after now.
    async fn cookie_change(&mut self, node: dom::BackendNodeId, field: &StateField, value: &serde_json::Value) -> Result<(serde_json::Value, Option<frontcap_core::intervention::InitialCookie>), ChromiumError> {
        use frontcap_core::intervention::{CookieSameSite, InitialCookie};
        if value.is_null() { return Ok((serde_json::Value::Null, None)) }
        let invalid = || ChromiumError::InvalidArgument("a cookie is null or {value, expires_at_ms?, secure?, http_only?, same_site?}".into());
        let fields = value.as_object().ok_or_else(invalid)?;
        if fields.keys().any(|k| !["value", "expires_at_ms", "secure", "http_only", "same_site"].contains(&k.as_str())) { return Err(invalid()) }
        let (origin, _, name, path, domain, partition) = self.cookie_identity(node, field).await?;
        let flag = |k: &str| fields.get(k).map(|v| v.as_bool().ok_or_else(invalid)).transpose().map(|b| b.unwrap_or(false));
        let same_site = match fields.get("same_site").map(|v| v.as_str()) {
            None => CookieSameSite::Lax,
            Some(Some("strict")) => CookieSameSite::Strict,
            Some(Some("lax")) => CookieSameSite::Lax,
            Some(Some("none")) => CookieSameSite::None,
            Some(Some("unspecified")) => CookieSameSite::Unspecified,
            _ => return Err(invalid()),
        };
        let cookie = InitialCookie {
            origin, name, path, domain, partition, same_site, priority: Default::default(), source: None, timing: None,
            value: fields.get("value").and_then(|v| v.as_str()).ok_or_else(invalid)?.to_string(),
            expires_at_ms: fields.get("expires_at_ms").map(|v| v.as_u64().ok_or_else(invalid)).transpose()?,
            secure: flag("secure")?, http_only: flag("http_only")?,
        };
        let now_ms = self.profile.initial_date_ms + self.now.0 / 1000;
        super::cookies::validate_initial_cookies(std::slice::from_ref(&cookie), now_ms)?;
        let mut normalized = serde_json::json!({
            "value": cookie.value, "secure": cookie.secure, "http_only": cookie.http_only,
            "same_site": match cookie.same_site { CookieSameSite::Strict => "strict", CookieSameSite::Lax => "lax", CookieSameSite::None => "none", CookieSameSite::Unspecified => "unspecified" },
        });
        if let Some(ms) = cookie.expires_at_ms { normalized["expires_at_ms"] = serde_json::json!(ms) }
        Ok((normalized, Some(cookie)))
    }

    /// Sets the field's cookie in the browser's store, or deletes it for null.
    async fn cookie_write(&mut self, node: dom::BackendNodeId, field: &StateField, value: &serde_json::Value) -> Result<(), ChromiumError> {
        let (_, cookie) = self.cookie_change(node.clone(), field, value).await?;
        match cookie {
            Some(c) => { self.page.call(&cdp::protocol::storage::SetCookiesParams::new(vec![super::cookies::cookie_param(&c, self.profile.initial_date_ms)?])).await?; }
            None => {
                let (_, host, name, path, domain, partition) = self.cookie_identity(node, field).await?;
                self.page.call(&cdp::protocol::network::DeleteCookiesParams {
                    name, url: None, domain: Some(domain.unwrap_or(host)), path: Some(path),
                    partition_key: partition.map(|p| cdp::protocol::network::CookiePartitionKey { top_level_site: p.top_level_site, has_cross_site_ancestor: p.has_cross_site_ancestor }),
                }).await?;
            }
        }
        Ok(())
    }

    /// The cookies the browser would send to the node's document URL, read natively (HttpOnly ones
    /// included), ordered by name, domain and path.
    async fn document_cookies(&mut self, node: dom::BackendNodeId) -> Result<serde_json::Value, ChromiumError> {
        let object = self.observed_object(node).await?;
        let url = self.call_on(&object, "function() { return this.ownerDocument.URL; }", vec![]).await?
            .as_str().map(str::to_string).ok_or_else(|| ChromiumError::Unsupported("the target document has no URL".into()))?;
        let params = cdp::protocol::network::GetCookiesParams { urls: Some(vec![url]) };
        let mut cookies = self.page.call(&params).await?.cookies;
        cookies.sort_by(|a, b| (&a.name, &a.domain, &a.path).cmp(&(&b.name, &b.domain, &b.path)));
        Ok(serde_json::Value::Array(cookies.iter().map(|c| serde_json::json!({
            "name": c.name, "value": c.value, "domain": c.domain, "path": c.path,
            "expires_at_ms": (!c.session).then(|| (c.expires * 1000.0).round() as u64),
            "http_only": c.http_only, "secure": c.secure,
            "same_site": c.same_site.as_ref().and_then(|s| serde_json::to_value(s).ok()).and_then(|v| v.as_str().map(str::to_lowercase)),
            "partition": c.partition_key.as_ref().map(|p| serde_json::json!({"top_level_site": p.top_level_site, "has_cross_site_ancestor": p.has_cross_site_ancestor})),
        })).collect()))
    }

    /// The element a state target names: a snapshot's text reference stands for the element it is in.
    async fn state_element(&mut self, node: dom::BackendNodeId) -> Result<dom::BackendNodeId, ChromiumError> {
        let object = self.observed_object(node.clone()).await?;
        if self.call_on(&object, "function() { return this.nodeType === 3 && !!this.parentElement; }", vec![]).await? != serde_json::Value::Bool(true) {
            return Ok(node);
        }
        let parent = self.page.call(&runtime::CallFunctionOnParams {
            object_id: Some(object), return_by_value: Some(false),
            ..runtime::CallFunctionOnParams::new("function() { return this.parentElement; }".into())
        }).await?;
        let parent = parent.result.object_id.ok_or_else(|| ChromiumError::NotFound("the text has no element".into()))?;
        self.page.backend_id_of(parent).await
    }

    /// What became of the provider request named `token`: its JSON text once settled, its error once
    /// failed, neither while pending.
    async fn app_outcome(&mut self, object: &runtime::RemoteObjectId, token: &str) -> Result<(Option<String>, Option<String>), ChromiumError> {
        let token = serde_json::to_string(token).unwrap();
        let answer = self.call_on(object, &format!("function() {{ try {{ return globalThis.__frontcap.appState(this.ownerDocument, 'outcome', null, null, {token}); }} catch (e) {{ return {{ state: 'failed', error: String(e && e.message || e) }}; }} }}"), vec![]).await?;
        Ok(match answer["state"].as_str() {
            Some("settled") => (Some(answer["value"].as_str().unwrap_or("null").to_string()), None),
            Some("failed") => (None, Some(answer["error"].as_str().unwrap_or("the promise failed").to_string())),
            _ => (None, None),
        })
    }

    /// A provider write's promise, settled: first at this instant, the page's work queued for it running
    /// under zero-length grants, then, when `allowed` grants it, frame tick by frame tick for at most that
    /// much virtual time from `from` (or now, kept in `slot` so a relaunched recorder goes on from there).
    async fn settle_app_write(&mut self, object: &runtime::RemoteObjectId, token: &str, slot: &str, allowed: u64, from: Option<VirtualTime>) -> Result<Settled, ChromiumError> {
        let mut settled = Settled { served: false, progressed_us: None, error: None };
        let mut servings = 0;
        let mut from = from;
        loop {
            match self.app_outcome(object, token).await? {
                (_, Some(error)) => { settled.error = Some(error); break }
                (Some(_), None) => break,
                (None, None) if from.is_none() && servings < INSTANT_SERVINGS => {
                    self.finish_current_deliveries().await?;
                    settled.served = true;
                    servings += 1;
                }
                (None, None) if allowed == 0 => {
                    settled.error = Some("hook timeout: the write's promise did not settle at this instant, and no progress was granted".into());
                    break;
                }
                (None, None) => {
                    let start = match from {
                        Some(start) => start,
                        None => {
                            self.world_eval(&format!("{slot}.progress_from = '{}'; true", self.now.0)).await?;
                            from = Some(self.now);
                            self.now
                        }
                    };
                    let end = start.saturating_add(VirtualDuration(allowed));
                    settled.progressed_us = Some(self.now.0 - start.0);
                    if self.now >= end {
                        settled.error = Some(format!("hook timeout: the write's promise did not settle within {} ms", allowed / 1000));
                        break;
                    }
                    let frame = VirtualDuration(self.profile.frame_interval_us.max(1));
                    let tick = frontcap_core::clock::frame_tick(frontcap_core::clock::frame_slot(self.now, frame) + 1, frame).min(end);
                    self.advance(VirtualDuration(tick.0 - self.now.0)).await?;
                    self.finish_current_deliveries().await?;
                    settled.progressed_us = Some(self.now.0 - start.0);
                }
            }
        }
        Ok(settled)
    }

    pub async fn state_read(&mut self, target: &Locator, field: &StateField) -> Result<StateValue, ChromiumError> {
        let boundary = self.observation_boundary().await?;
        let node = self.resolve(target).await?;
        let node = self.state_element(node).await?;
        let value = self.read_state_field(node.clone(), field).await?;
        self.expect_boundary(&boundary).await?;
        Ok(StateValue { boundary, r#ref: self.ref_for(node.0), field: field.clone(), value })
    }

    pub async fn state_plan(&mut self, expected: &ObservationBoundary, changes: &[StateChange]) -> Result<StatePlan, ChromiumError> {
        self.state_plan_with(expected, changes, 0).await
    }

    /// A plan whose app provider writes may be given up to `progress_us` of virtual time each, within the
    /// provider's own `max_ms`.
    pub async fn state_plan_with(&mut self, expected: &ObservationBoundary, changes: &[StateChange], progress_us: u64) -> Result<StatePlan, ChromiumError> {
        if changes.is_empty() || changes.len() > 64 {
            return Err(ChromiumError::InvalidArgument("state apply accepts 1 to 64 document changes".into()));
        }
        self.expect_boundary(expected).await?;
        // Resolve every target, validate every field/type and check every old value before any write.
        let mut prepared = Vec::new();
        let mut progress = Vec::new();
        let mut planned = changes.to_vec();
        for (item, change) in changes.iter().enumerate() {
            if matches!(change.field, StateField::IndexedDbRecord { .. }) {
                if !self.patched { return Err(ChromiumError::Unsupported("native IndexedDB writes require the patched engine".into())); }
                validate_record_write(&change.field, &change.value)?;
                normalize_json_numbers(&mut planned[item].value);
                if let Some(expected) = &mut planned[item].expected {
                    validate_record_write(&change.field, &expected.value)?;
                    normalize_json_numbers(&mut expected.value);
                }
            } else if matches!(change.field, StateField::CacheEntry { .. }) {
                if !self.patched { return Err(ChromiumError::Unsupported("native cache writes require the patched engine".into())); }
                planned[item].value = super::cache::normalize_cache_record(&change.field, &change.value, true)?;
                if let Some(expected) = &mut planned[item].expected {
                    expected.value = super::cache::normalize_cache_record(&change.field, &expected.value, false)?;
                }
            } else if !matches!(change.field, StateField::Cookie { .. }) {
                field_expression(&change.field, Some(&change.value))?;
            }
            let node = self.resolve(&change.target).await?;
            let node = self.state_element(node).await?;
            if let StateField::Cookie { .. } = &change.field {
                // As read back: a write leaves its attributes to their defaults.
                planned[item].value = self.cookie_change(node.clone(), &change.field, &change.value).await?.0;
                // A session cookie reads without an expiry; the cookie list gives it as null.
                if let Some(cookie) = planned[item].expected.as_mut().and_then(|e| e.value.as_object_mut())
                    && cookie.get("expires_at_ms").is_some_and(serde_json::Value::is_null)
                {
                    cookie.remove("expires_at_ms");
                }
            }
            let before = self.read_state_field(node.clone(), &change.field).await?;
            if let StateField::Attribute { name } = &change.field {
                let object = self.observed_object(node.clone()).await?;
                let name = serde_json::to_string(name).unwrap();
                let valid = self.call_on(&object, &format!("function() {{ try {{ this.ownerDocument.createAttribute({name}); return true; }} catch (_) {{ return false; }} }}"), vec![]).await?;
                if valid != serde_json::Value::Bool(true) { return Err(ChromiumError::InvalidArgument("invalid attribute name".into())); }
            }
            let mut allowed = 0;
            if let StateField::App { provider } = &change.field {
                let providers = self.read_state_field(node.clone(), &StateField::AppProviders).await?;
                let entry = providers.as_array().and_then(|list| list.iter().find(|p| p["id"] == provider.as_str()));
                if entry.map(|p| p["writable"] == true) != Some(true) {
                    return Err(ChromiumError::InvalidArgument(format!("state provider {provider} is read only")));
                }
                allowed = progress_us.min(entry.and_then(|p| p["max_ms"].as_u64()).unwrap_or(0).saturating_mul(1000));
            }
            progress.push(allowed);
            if let Some(e) = planned[item].expected.as_ref().filter(|e| e.value != before) {
                let short = |v: &serde_json::Value| v.to_string().chars().take(120).collect::<String>();
                return Err(ChromiumError::NotExpected(format!("change {item} is now {}, not its expected {}", short(&before), short(&e.value))));
            }
            prepared.push(node.0);
        }
        self.expect_boundary(expected).await?;
        Ok(StatePlan { document: self.documents, changes: planned, nodes: prepared, progress_us: progress })
    }

    // Only the accepted operation owns these receipts; its predecessor is durably completed before
    // another operation can begin. Keep no unbounded history inside the renderer.
    pub async fn state_begin(&mut self, request: &RequestId) -> Result<(), ChromiumError> {
        let key = serde_json::to_string(request.as_str()).unwrap();
        self.world_eval(&format!("globalThis.__frontcap.stateReceipts = Object.create(null); globalThis.__frontcap.stateReceipts[{key}] = []; true")).await?;
        Ok(())
    }

    pub async fn state_apply(&mut self, request: RequestId, expected: &ObservationBoundary, changes: &[StateChange]) -> Result<AppliedState, ChromiumError> {
        let plan = self.state_plan(expected, changes).await?;
        self.state_begin(&request).await?;
        self.state_run(&request, &plan, |_, _, _| Ok(())).await
    }

    pub async fn state_run<F>(&mut self, request: &RequestId, plan: &StatePlan, mut progress: F) -> Result<AppliedState, ChromiumError>
    where F: FnMut(&str, usize, &[ChangeReceipt]) -> Result<(), ChromiumError> {
        let key = serde_json::to_string(request.as_str()).unwrap();
        let bucket = format!("globalThis.__frontcap.stateReceipts?.[{key}]");
        if plan.document != self.documents || self.world_eval(&format!("Array.isArray({bucket})")).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::InterventionUncertain("the document holding state receipts was replaced".into()));
        }
        let changes = &plan.changes;
        let mut receipts = Vec::new();
        for (index, (change, backend)) in changes.iter().zip(&plan.nodes).enumerate() {
            let slot = format!("globalThis.__frontcap.stateReceipts[{key}][{index}]");
            let value = self.world_eval(&format!("{slot} ?? null")).await?;
            let mut step: Option<StateStep> = serde_json::from_value(value).map_err(|e| ChromiumError::Shape(e.to_string()))?;
            if let Some(receipt) = step.as_ref().and_then(|s| s.receipt.clone()) {
                let verified = receipt.verified;
                receipts.push(receipt);
                progress("state_saved", index, &receipts)?;
                if !verified { break; }
                continue;
            }
            let node = dom::BackendNodeId(*backend);
            if let (Some(s), StateField::Cookie { .. } | StateField::IndexedDbRecord { .. } | StateField::CacheEntry { .. }) = (step.as_mut(), &change.field)
                && s.phase != "written"
            {
                // Explicit cookie/record setters are idempotent at this stopped boundary.
                // Retry only these native setters after an interrupted dispatch.
                s.error = self.native_state_write(node.clone(), &change.field, &change.value).await.err().map(|e| e.to_string());
                s.phase = "written".into();
            }
            if step.as_ref().is_some_and(|s| s.phase != "written") {
                return Err(ChromiumError::InterventionUncertain(format!("item {index} was dispatched without a completion receipt")));
            }
            let receipt = if step.is_none() {
                let before = self.read_state_field(node.clone(), &change.field).await;
                match before {
                    Err(e) => Some(ChangeReceipt { index, attempted: false, before: None, after: None, verified: false, error: Some(e.to_string()), served_instant: false, progressed_us: None }),
                    Ok(before) if change.expected.as_ref().is_some_and(|e| e.value != before) => Some(ChangeReceipt { index, attempted: false, before: Some(before), after: None, verified: false, error: Some("expected value changed after an earlier item".into()), served_instant: false, progressed_us: None }),
                    Ok(before) if matches!(change.field, StateField::Cookie { .. } | StateField::IndexedDbRecord { .. } | StateField::CacheEntry { .. }) => {
                        // Native store: preserve dispatch, completion and readback around the setter.
                        let before_text = serde_json::to_string(&before).unwrap();
                        progress("state_before_write", index, &receipts)?;
                        self.world_eval(&format!("{slot} = {{phase:'dispatched', before:{before_text}, error:null, receipt:null}}; true")).await?;
                        let error = self.native_state_write(node.clone(), &change.field, &change.value).await.err().map(|e| e.to_string());
                        progress("state_native_written", index, &receipts)?;
                        let v = self.world_eval(&format!("{slot}.error = {}; {slot}.phase = 'written'; {slot}", serde_json::to_string(&error).unwrap())).await?;
                        step = Some(serde_json::from_value(v).map_err(|e| ChromiumError::Shape(e.to_string()))?);
                        progress("state_after_write", index, &receipts)?;
                        None
                    }
                    Ok(before) => {
                        let object = self.observed_object(node.clone()).await?;
                        let expression = field_expression(&change.field, Some(&change.value))?;
                        let before = serde_json::to_string(&before).unwrap();
                        progress("state_before_write", index, &receipts)?;
                        // Dispatch and completion marks live with the target. A recorder can die on
                        // either side of this call without causing a setter to run twice.
                        let token = serde_json::to_string(&format!("{}/{index}", request.as_str())).unwrap();
                        let v = self.call_on(&object, &format!("function() {{ const token = {token}; const s = {slot} = {{phase:'dispatched', before:{before}, error:null, receipt:null}}; try {{ const r = {expression}; if (r && typeof r === 'object' && typeof r.pending === 'string') s.pending = r.pending; }} catch(e) {{ s.error = String(e); }} s.phase = 'written'; return s; }}"), vec![]).await?;
                        step = Some(serde_json::from_value(v).map_err(|e| ChromiumError::Shape(e.to_string()))?);
                        progress("state_after_write", index, &receipts)?;
                        None
                    }
                }
            } else { None };
            let receipt = match receipt {
                Some(r) => r,
                None => {
                    // This separate task reads after the setter's microtask checkpoint. If the
                    // recorder died after the setter, recovery resumes here without reapplying it.
                    let mut step = step.expect("written step");
                    let mut served_instant = false;
                    let mut progressed_us = None;
                    if let Some(token) = step.pending.clone() {
                        let object = self.observed_object(node.clone()).await?;
                        let allowed = plan.progress_us.get(index).copied().unwrap_or(0);
                        let from = step.progress_from.as_deref().and_then(|t| t.parse().ok()).map(VirtualTime);
                        let settled = self.settle_app_write(&object, &token, &slot, allowed, from).await?;
                        served_instant = settled.served;
                        progressed_us = settled.progressed_us;
                        if let Some(error) = settled.error { step.error = Some(error); }
                    }
                    let (after, read_error) = match self.read_state_field(node, &change.field).await {
                        Ok(v) => (Some(v), None), Err(e) => (None, Some(e.to_string())),
                    };
                    let error = step.error.or(read_error);
                    let verified = error.is_none() && after.as_ref().is_some_and(|after| {
                        if matches!(change.field, StateField::CacheEntry { .. }) {
                            super::cache::cache_write_matches(&change.value, after)
                        } else { after == &change.value }
                    });
                    ChangeReceipt { index, attempted: true, before: step.before, after, verified, error, served_instant, progressed_us }
                }
            };
            let encoded = serde_json::to_string(&receipt).unwrap();
            self.world_eval(&format!("{slot} = {{phase:'complete', receipt:{encoded}}}; true")).await?;
            let verified = receipt.verified;
            receipts.push(receipt);
            progress("state_after_receipt", index, &receipts)?;
            progress("state_saved", index, &receipts)?;
            if !verified { break; }
        }
        self.state_finish(request, plan, receipts).await
    }

    pub async fn state_finish(&mut self, request: &RequestId, plan: &StatePlan, receipts: Vec<ChangeReceipt>) -> Result<AppliedState, ChromiumError> {
        let changes = &plan.changes;
        let completed = receipts.len() == changes.len() && receipts.iter().all(|r| r.verified);
        if self.recorded_operations.insert(request.clone()) {
            self.record(self.now, EventKind::Intervention(InterventionRecord { request_id: request.clone(), changes: changes.to_vec(), receipts: receipts.clone(), completed }));
        }
        let boundary = self.observation_boundary().await?;
        let presentation = self.presented_relation().await?;
        Ok(AppliedState { boundary, completed, presentation, receipts })
    }
}

impl Generation {
    /// What a recipe saved from this generation can reconstruct, read at its frozen boundary without
    /// progress: the top document's address, its windows and every origin's local and session storage,
    /// the cookie store, the native partition's buckets/cache/IndexedDB, and the values of the top
    /// document's writable app state providers.
    pub async fn live_state(&mut self) -> Result<frontcap_core::basis::LiveState, ChromiumError> {
        if self.popup_id().is_some() {
            return Err(ChromiumError::InvalidArgument("a recipe is saved from the launch page".into()));
        }
        let boundary = self.observation_boundary().await?;
        let windows = self.read_context_storage_inputs().await?;
        let cookies = self.live_cookies().await?;
        let root = self.resolve(&Locator::Document { selector: None }).await?;
        let captured = self.live_bucket_storage().await?;
        let mut app = Vec::new();
        let providers = self.read_state_field(root.clone(), &StateField::AppProviders).await?;
        for provider in providers.as_array().into_iter().flatten().filter(|p| p["writable"] == true) {
            let Some(id) = provider["id"].as_str() else { continue };
            let value = self.read_state_field(root.clone(), &StateField::App { provider: id.into() }).await?;
            app.push(frontcap_core::basis::AppValue { provider: id.into(), value });
        }
        self.expect_boundary(&boundary).await?;
        Ok(frontcap_core::basis::LiveState {
            boundary, url: windows.url, window_name: windows.window_name, contexts: windows.contexts,
            dom_storage: windows.storage, cookies, app, cookie_inventory_complete: true,
            buckets: captured.buckets, storage_inventory_complete: true,
            storage_excluded: captured.excluded,
            indexed_db: captured.databases, indexed_db_excluded: captured.database_excluded,
            caches: captured.caches, cache_excluded: captured.cache_excluded,
        })
    }

    /// The selected bucket's IndexedDB as initial databases a recipe recreates, read without
    /// page tasks, and what it cannot recreate: a database holding a value or key a JSON record does not
    /// reproduce (a date, binary data, a map), or more than a recipe holds.
    pub(super) async fn live_indexed_db(&mut self, scope: &frontcap_core::intervention::StorageScope, origin: &str, partition: Option<&frontcap_core::storage::StoragePartition>, bucket_name: Option<&str>) -> Result<(Vec<frontcap_core::indexed_db::InitialDatabase>, Vec<String>), ChromiumError> {
        use cdp::protocol::{indexed_db, storage};
        use frontcap_core::indexed_db::{Index, InitialDatabase, ObjectStore, Record};
        const PAGE: i64 = 100;
        const RECORDS: usize = 1024;
        let bucket = Some(storage::StorageBucket { storage_key: storage::SerializedStorageKey(scope.storage_key.clone()), name: bucket_name.map(str::to_owned) });
        let mut names = self.page.call(&indexed_db::RequestDatabaseNamesParams { security_origin: None, storage_key: None, storage_bucket: bucket.clone() }).await?.database_names;
        names.sort();
        let (mut databases, mut excluded) = (Vec::new(), Vec::new());
        let mut budget = 20_000usize;
        'databases: for name in names {
            let described = self.page.call(&indexed_db::RequestDatabaseParams { security_origin: None, storage_key: None, storage_bucket: bucket.clone(), database_name: name.clone() }).await?.database_with_object_stores;
            let mut stores = Vec::new();
            for s in described.object_stores {
                let key_path = core_key_path(&s.key_path);
                let mut indexes = Vec::new();
                for i in &s.indexes {
                    let Some(path) = core_key_path(&i.key_path) else { continue };
                    indexes.push(Index { name: i.name.clone(), key_path: path, unique: i.unique, multi_entry: i.multi_entry });
                }
                let mut records = Vec::new();
                let mut skip = 0;
                loop {
                    let data = self.page.call(&indexed_db::RequestDataParams {
                        security_origin: None, storage_key: None, storage_bucket: bucket.clone(), database_name: name.clone(),
                        object_store_name: s.name.clone(), index_name: None, skip_count: skip, page_size: PAGE, key_range: None,
                    }).await?;
                    for entry in data.object_store_data_entries {
                        let Some(value) = self.faithful_json(entry.value, &mut budget).await? else {
                            excluded.push(format!("IndexedDB database {name}: store {} holds a value a JSON record does not reproduce (a date, binary data, a map, a set)", s.name));
                            continue 'databases;
                        };
                        let key = match key_path {
                            Some(_) => None,
                            None => match self.faithful_key(entry.primary_key, &mut budget).await? {
                                Some(key) => Some(key),
                                None => {
                                    excluded.push(format!("IndexedDB database {name}: store {} has a key a recipe does not reproduce (a date or binary key)", s.name));
                                    continue 'databases;
                                }
                            },
                        };
                        records.push(Record { key, value });
                    }
                    if records.len() > RECORDS {
                        excluded.push(format!("IndexedDB database {name}: store {} holds more than {RECORDS} records", s.name));
                        continue 'databases;
                    }
                    if !data.has_more { break }
                    skip += PAGE;
                }
                stores.push(ObjectStore { name: s.name, key_path, auto_increment: s.auto_increment, indexes, records });
            }
            let database = InitialDatabase { origin: origin.into(), partition: partition.cloned(), bucket: bucket_name.map(str::to_owned), name: name.clone(), version: described.version as u64, stores };
            if super::indexed_db::validate_initial_databases(std::slice::from_ref(&database)).is_err() {
                excluded.push(format!("IndexedDB database {name}: beyond what a recipe recreates (sizes, names or counts)"));
                continue;
            }
            databases.push(database);
        }
        if !databases.is_empty() && super::indexed_db::validate_initial_databases(&databases).is_err() {
            excluded.push("IndexedDB: the databases together are beyond what a recipe recreates".into());
            databases.clear();
        }
        let _ = self.page.call(&runtime::ReleaseObjectGroupParams::new("indexeddb".into())).await;
        Ok((databases, excluded))
    }

    /// A remote object's JSON value when a JSON record reproduces it exactly (plain objects and arrays of
    /// strings, finite numbers, booleans and null), read through the inspector's property listing, which
    /// runs no page script; `None` otherwise. `budget` bounds the objects read.
    async fn faithful_json(&mut self, object: runtime::RemoteObject, budget: &mut usize) -> Result<Option<serde_json::Value>, ChromiumError> {
        use runtime::{RemoteObjectSubtype, RemoteObjectType};
        Ok(match object.r#type {
            RemoteObjectType::String | RemoteObjectType::Boolean => object.value,
            RemoteObjectType::Number if object.unserializable_value.is_none() => object.value,
            RemoteObjectType::Object if object.subtype == Some(RemoteObjectSubtype::Null) => Some(serde_json::Value::Null),
            RemoteObjectType::Object => {
                let array = object.subtype == Some(RemoteObjectSubtype::Array);
                let plain = array || (object.subtype.is_none() && object.class_name.as_deref() == Some("Object"));
                let (Some(id), true) = (object.object_id, plain) else { return Ok(None) };
                if *budget == 0 { return Ok(None) }
                *budget -= 1;
                let properties = self.page.call(&runtime::GetPropertiesParams {
                    object_id: id, own_properties: Some(true), accessor_properties_only: None, generate_preview: Some(false), non_indexed_properties_only: None,
                }).await?.result;
                let mut fields = serde_json::Map::new();
                let mut length = None;
                for p in properties {
                    if p.symbol.is_some() { continue }
                    if array && p.name == "length" {
                        length = p.value.and_then(|v| v.value).and_then(|v| v.as_u64());
                        continue;
                    }
                    let (Some(value), true, None, None) = (p.value, p.enumerable, &p.get, &p.set) else { return Ok(None) };
                    let Some(json) = Box::pin(self.faithful_json(value, budget)).await? else { return Ok(None) };
                    fields.insert(p.name, json);
                }
                if !array { return Ok(Some(serde_json::Value::Object(fields))) }
                // A sparse array keeps a length its elements do not fill: refused before anything is
                // sized by it.
                let Some(length) = length.filter(|&n| usize::try_from(n).is_ok_and(|n| n == fields.len())) else { return Ok(None) };
                let mut items = Vec::with_capacity(fields.len());
                for i in 0..length {
                    // A hole is no JSON element.
                    let Some(item) = fields.remove(&i.to_string()) else { return Ok(None) };
                    items.push(item);
                }
                if !fields.is_empty() { return Ok(None) }
                Some(serde_json::Value::Array(items))
            }
            _ => None,
        })
    }

    /// A primary key as a recipe's: a number, a string or an array of them; `None` for a date or binary key.
    async fn faithful_key(&mut self, object: runtime::RemoteObject, budget: &mut usize) -> Result<Option<frontcap_core::indexed_db::Key>, ChromiumError> {
        use frontcap_core::indexed_db::Key;
        use runtime::{RemoteObjectSubtype, RemoteObjectType};
        Ok(match (object.r#type, object.subtype) {
            (RemoteObjectType::Number, _) => object.value.and_then(|v| v.as_number().cloned()).map(Key::Number),
            (RemoteObjectType::String, _) => object.value.and_then(|v| v.as_str().map(|s| Key::String(s.to_string()))),
            (RemoteObjectType::Object, Some(RemoteObjectSubtype::Array)) => {
                let Some(serde_json::Value::Array(items)) = self.faithful_json(object, budget).await? else { return Ok(None) };
                let mut keys = Vec::new();
                for item in items {
                    keys.push(match item {
                        serde_json::Value::Number(n) => Key::Number(n),
                        serde_json::Value::String(s) => Key::String(s),
                        _ => return Ok(None),
                    });
                }
                Some(Key::Array(keys))
            }
            _ => None,
        })
    }
}

/// An IndexedDB read's refusals by the engine's message (engine patches 61 and 62): a read the page's
/// unfinished work would hold, and a missing database or store.
// JSON crosses JavaScript's Number representation: integral floats and negative
// zero return as ordinary integer JSON. Retain that readback form in the plan.
fn normalize_json_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_f64().filter(|v| v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0) {
                *n = serde_json::Number::from(v as i64);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize_json_numbers),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(normalize_json_numbers),
        _ => {}
    }
}

fn validate_record_write(field: &StateField, value: &serde_json::Value) -> Result<(), ChromiumError> {
    let StateField::IndexedDbRecord { bucket, database, store, key, .. } = field else { unreachable!("record field") };
    super::buckets::validate_bucket_name(bucket.as_deref())?;
    if database.encode_utf16().count() > 1024 || store.encode_utf16().count() > 1024 || native_key(key).is_none() {
        return Err(ChromiumError::InvalidArgument("invalid IndexedDB database, store or key".into()));
    }
    let record: IndexedDbRecord = serde_json::from_value(value.clone()).map_err(|e| ChromiumError::InvalidArgument(e.to_string()))?;
    if let IndexedDbRecord::Present { value } = record
        && (!super::indexed_db::value_valid(&value, 0) || value.to_string().len() > 65536) {
        return Err(ChromiumError::InvalidArgument("IndexedDB record exceeds 64 KiB, JSON depth or safe number bounds".into()));
    }
    Ok(())
}

fn idb_refusal(e: ChromiumError, database: &str, store: Option<&str>) -> ChromiumError {
    let ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }) = &e else { return e };
    if message.starts_with("IndexedDB read refused:") || message.starts_with("IndexedDB write refused:") {
        let store = store.map(|s| format!(", store {s:?}")).unwrap_or_default();
        return ChromiumError::PageWorkPending(format!("{message} (database {database:?}{store})"));
    }
    match (message.as_str(), store) {
        ("Could not open database: it does not exist.", _) => ChromiumError::StorageAbsent(format!("no IndexedDB database {database:?} in this scope")),
        ("Could not get object store.", Some(store)) => ChromiumError::StorageAbsent(format!("no object store {store:?} in IndexedDB database {database:?}")),
        _ => e,
    }
}
