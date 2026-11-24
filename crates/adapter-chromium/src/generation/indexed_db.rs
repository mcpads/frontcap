use super::*;
use crate::encode::canonical_origin_host;
use frontcap_core::indexed_db::{DatabaseInitializationRecord, InitialDatabase, Key, KeyPath};
use serde_json::{Value, json};

fn invalid(message: &str) -> ChromiumError {
    ChromiumError::InvalidArgument(message.into())
}
fn short_name(value: &str) -> bool {
    value.encode_utf16().count() <= 1024
}
fn path_valid(path: &KeyPath) -> bool {
    match path {
        KeyPath::String(s) => short_name(s),
        KeyPath::Array(paths) => paths.len() <= 32 && paths.iter().all(|s| short_name(s)),
    }
}
fn number_valid(n: &serde_json::Number) -> bool {
    n.as_f64()
        .is_some_and(|n| n.is_finite() && (n.fract() != 0.0 || n.abs() <= 9_007_199_254_740_991.0))
        && n.as_i64()
            .is_none_or(|n| n.unsigned_abs() <= 9_007_199_254_740_991)
        && n.as_u64().is_none_or(|n| n <= 9_007_199_254_740_991)
}
pub(super) fn value_valid(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Number(n) => number_valid(n),
        Value::Array(a) => a.iter().all(|v| value_valid(v, depth + 1)),
        Value::Object(o) => o.values().all(|v| value_valid(v, depth + 1)),
        _ => true,
    }
}
fn key_valid(key: &Key, depth: usize, count: &mut usize) -> bool {
    *count += 1;
    if depth > 32 || *count > 4096 {
        return false;
    }
    match key {
        Key::Number(n) => number_valid(n),
        Key::Date(n) => n.unsigned_abs() <= 8_640_000_000_000_000,
        Key::String(s) => s.encode_utf16().count() <= 8192,
        Key::Array(a) => a.iter().all(|k| key_valid(k, depth + 1, count)),
    }
}

pub fn validate_initial_databases(databases: &[InitialDatabase]) -> Result<(), ChromiumError> {
    if databases.len() > 16 || serde_json::to_vec(databases).unwrap().len() > 8 * 1024 * 1024 {
        return Err(invalid(
            "initial IndexedDB exceeds 16 databases or 8 MiB serialized inputs",
        ));
    }
    let mut identities = std::collections::BTreeSet::new();
    let (mut records, mut bytes, mut key_nodes) = (0, 0usize, 0);
    for db in databases {
        super::buckets::validate_bucket_name(db.bucket.as_deref())?;
        super::buckets::validate_partition(db.partition.as_ref())?;
        let host = canonical_origin_host(&db.origin)?;
        let loopback = host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
        if !db.origin.starts_with("https://")
            && host != "localhost"
            && !host.ends_with(".localhost")
            && !loopback
        {
            return Err(invalid(
                "initial IndexedDB requires HTTPS or a trustworthy loopback origin",
            ));
        }
        if !short_name(&db.name)
            || !identities.insert((&db.origin, &db.partition, &db.bucket, &db.name))
            || db.version == 0
            || db.version > 9_007_199_254_740_991
            || db.stores.len() > 16
        {
            return Err(invalid(
                "initial IndexedDB requires unique origin/partition/bucket/name tuples, safe positive versions and at most 16 stores per database",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for store in &db.stores {
            if !short_name(&store.name)
                || !names.insert(&store.name)
                || store.indexes.len() > 64
                || store.key_path.as_ref().is_some_and(|p| !path_valid(p))
                || (store.auto_increment && matches!(&store.key_path, Some(KeyPath::Array(_))))
                || (store.auto_increment
                    && matches!(&store.key_path,Some(KeyPath::String(s)) if s.is_empty()))
            {
                return Err(invalid(
                    "invalid IndexedDB store name, key path or auto-increment schema",
                ));
            }
            let mut indexes = std::collections::BTreeSet::new();
            for index in &store.indexes {
                if !short_name(&index.name)
                    || !indexes.insert(&index.name)
                    || !path_valid(&index.key_path)
                    || (index.multi_entry && matches!(index.key_path, KeyPath::Array(_)))
                {
                    return Err(invalid("invalid IndexedDB index name or key path"));
                }
            }
            for record in &store.records {
                records += 1;
                bytes += record.value.to_string().encode_utf16().count() * 2;
                if records > 1024
                    || bytes > 4 * 1024 * 1024
                    || !value_valid(&record.value, 0)
                    || record
                        .key
                        .as_ref()
                        .is_some_and(|k| !key_valid(k, 0, &mut key_nodes))
                    || (store.key_path.is_some() && record.key.is_some())
                    || (store.key_path.is_none() && !store.auto_increment && record.key.is_none())
                {
                    return Err(invalid(
                        "invalid IndexedDB record/key or input bounds; values are JSON with safe integer numbers, at most 1024 records and 4 MiB UTF-16 JSON text",
                    ));
                }
            }
        }
    }
    Ok(())
}
fn native_path(path: Option<&KeyPath>) -> Value {
    match path {
        None => json!({"type":"null"}),
        Some(KeyPath::String(s)) => json!({"type":"string","string":s}),
        Some(KeyPath::Array(a)) => json!({"type":"array","array":a}),
    }
}
fn native_key(key: &Key) -> Value {
    match key {
        Key::Number(n) => json!({"type":"number","number":n}),
        Key::String(s) => json!({"type":"string","string":s}),
        Key::Date(n) => json!({"type":"date","date":n}),
        Key::Array(a) => {
            json!({"type":"array","array":a.iter().map(native_key).collect::<Vec<_>>()})
        }
    }
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeDatabase {
    storage_bucket: Value,
    database_name: String,
    version: u64,
    stores: Vec<Value>,
}
impl cdp::Command for InitializeDatabase {
    const METHOD: &'static str = "IndexedDB.frontcapInitializeDatabase";
    type Returns = Value;
}
impl Generation {
    pub(super) async fn initialize_databases(
        &mut self,
        scopes: &[super::buckets::InitialStorageScope],
    ) -> Result<(), ChromiumError> {
        let databases = self.profile.initial_indexed_db.clone();
        if databases.is_empty() {
            return Ok(());
        }
        if scopes.len() != databases.len() {
            return Err(invalid("database scope count changed after resolution"));
        }
        for (database, scope) in databases.into_iter().zip(scopes) {
            let key = &scope.key;
            self.bind_initial_storage_scope(
                scope,
                &database.origin,
                database.partition.as_ref(),
                database.bucket.as_deref(),
            )
            .await?;
            let mut storage_bucket = json!({"storageKey":key});
            if let Some(name) = &database.bucket {
                storage_bucket["name"] = json!(name);
            }
            let stores = database.stores.iter().map(|s| json!({
                "schema":{"name":s.name,"keyPath":native_path(s.key_path.as_ref()),"autoIncrement":s.auto_increment,
                    "indexes":s.indexes.iter().map(|i|json!({"name":i.name,"keyPath":native_path(Some(&i.key_path)),
                        "unique":i.unique,"multiEntry":i.multi_entry})).collect::<Vec<_>>()},
                "records":s.records.iter().map(|r| {
                    let mut v=json!({"valueJson":r.value.to_string()});
                    if let Some(k)=&r.key {v["key"]=native_key(k);}
                    v
                }).collect::<Vec<_>>()
            })).collect();
            // Native success requires upgrade commit, schema comparison, exact key/value reads,
            // store counts and completion of the separate readonly verification transaction.
            self.page
                .call(&InitializeDatabase {
                    storage_bucket,
                    database_name: database.name.clone(),
                    version: database.version,
                    stores,
                })
                .await?;
            let bucket = database.bucket.clone();
            self.record(
                self.now,
                EventKind::IndexedDbInitialized(DatabaseInitializationRecord {
                    database,
                    storage_key: key.clone(),
                }),
            );
            self.commit()?;
            self.release_initial_storage_scope(scope, bucket.as_deref())
                .await?;
        }
        Ok(())
    }
}
