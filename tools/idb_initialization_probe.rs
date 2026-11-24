//! Opt-in acceptance of native schema/record transactions, not the default suite.
mod support;
use cdp::protocol::{indexed_db, page, storage, target, tracing};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::time::Duration;

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
fn store(records: Vec<Value>) -> Value {
    json!({"schema":{"name":"items","keyPath":{"type":"string","string":"id"},
        "autoIncrement":true,"indexes":[
            {"name":"email","keyPath":{"type":"string","string":"email"},"unique":true,"multiEntry":false},
            {"name":"tags","keyPath":{"type":"string","string":"tags"},"unique":false,"multiEntry":true},
            {"name":"id","keyPath":{"type":"string","string":"id"},"unique":true,"multiEntry":false}
        ]},"records":records})
}
fn record(value: Value) -> Value { json!({"valueJson":value.to_string()}) }
async fn initialize(native: &Session, key: &str, name: &str, version: u64, stores: Vec<Value>)
    -> Result<Value, frontcap_chromium::ChromiumError> {
    println!("IDB_INITIALIZATION_STAGE {name} version={version}");
    let trace = std::env::var_os("FRONTCAP_IDB_TRACE").is_some() && name == "rollback";
    let mut data = native.client.subscribe::<tracing::DataCollectedEvent>(None);
    let mut done = native.client.subscribe::<tracing::TracingCompleteEvent>(None);
    if trace {
        native.client.call(None, &tracing::StartParams {
            categories: Some("-*,IndexedDB".into()), ..Default::default()
        }).await.unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), native.call(&InitializeDatabase {
        storage_bucket: json!({"storageKey":key}), database_name: name.into(), version, stores,
    })).await;
    if trace {
        native.client.call(None,&tracing::EndParams::default()).await.unwrap();
        let mut collected = Vec::new();
        tokio::time::timeout(Duration::from_secs(5),async {
            loop { tokio::select! {
                event = data.next() => collected.extend(event.unwrap().value),
                complete = done.next() => { assert!(!complete.unwrap().data_loss_occurred); break; }
            }}
            while let Some(event) = data.next_timeout(Duration::from_millis(20)).await.unwrap() {
                collected.extend(event.value);
            }
        }).await.unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/browser-finish-checks");
        std::fs::write(root.join("idb-native-trace.json"),serde_json::to_vec(&collected).unwrap()).unwrap();
        println!("IDB_INITIALIZATION_TRACE events={}",collected.len());
    }
    result.unwrap_or_else(|_| panic!("native transaction {name} version={version} must settle while app time is stopped"))
}
async fn names(native: &Session, key: &str) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(3), native.call(&indexed_db::RequestDatabaseNamesParams {
        storage_key: Some(key.into()), ..Default::default()
    })).await.unwrap().unwrap().database_names
}

#[tokio::test]
async fn idb_initialization_probe() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://storage.example.test/start";
    let html = r#"<!doctype html><script>
      globalThis.appBooted=true;
      const result=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
      (async()=>{
        const db=await result(indexedDB.open('seed'));
        const tx=db.transaction(['items','pairs','external']);
        const items=tx.objectStore('items');
        const [values,tags,id,pairs,keys]=await Promise.all([
          result(items.getAll()), result(items.index('tags').getAll('blue')),
          result(items.index('id').get(1)),result(tx.objectStore('pairs').get(['a',2])),
          result(tx.objectStore('external').getAllKeys())]);
        globalThis.bootValue={version:db.version,names:Array.from(db.objectStoreNames),
          values,tags,id,pairs,keys:keys.map(k=>k instanceof Date?{date:k.getTime()}:k)};
        db.close();
      })().catch(e=>globalThis.bootError=String(e));
    </script>"#;
    let rules = RuleSet::from_json(&json!([{"id":"app","match":{"url":url},"action":{
        "type":"mock","status":200,"headers":{"content-type":"text/html"},"body":html
    }}]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "idb-initialization", url.into(), rules)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let frame = native.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame.id;
    let key = native.call(&storage::GetStorageKeyForFrameParams::new(frame)).await.unwrap().storage_key.0;
    let before = g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap();
    let invalid = initialize(&native, &key, "invalid", 1, vec![store(vec![
        record(json!({"email":"ok"})), json!({"valueJson":"{"})
    ])]).await;
    assert!(matches!(invalid, Err(frontcap_chromium::ChromiumError::Cdp(cdp::CdpError::Protocol {code:-32602,..}))), "{invalid:?}");
    assert!(names(&native,&key).await.is_empty());
    // Distinct backend constraints must roll back schema and earlier records.
    for records in [
        vec![record(json!({"email":"same"})),record(json!({"email":"same"}))],
        vec![record(json!({"id":1,"email":"a"})),record(json!({"id":1,"email":"b"}))],
    ] {
        assert!(initialize(&native,&key,"rollback",1,vec![store(records)]).await.is_err());
        assert!(names(&native,&key).await.is_empty(), "failed upgrade must leave no database");
    }
    let records = vec![record(json!({"email":"a","tags":["blue","blue",null]})),
        record(json!({"id":5,"email":"b","tags":["blue","red"]}))];
    initialize(&native,&key,"seed",7,vec![store(records),
        json!({"schema":{"name":"empty","keyPath":{"type":"null"},"autoIncrement":false,"indexes":[]},"records":[]}),
        json!({"schema":{"name":"pairs","keyPath":{"type":"array","array":["team","id"]},"autoIncrement":false,"indexes":[]},
            "records":[record(json!({"team":"a","id":2,"value":"compound"}))]}),
        json!({"schema":{"name":"external","keyPath":{"type":"null"},"autoIncrement":false,"indexes":[]},
            "records":[{"key":{"type":"date","date":1234},"valueJson":"\"date\""}]})
    ]).await.unwrap();
    assert_eq!(names(&native,&key).await,vec!["seed"]);
    initialize(&native,&key,"empty-database",2,vec![]).await.unwrap();
    for version in [7,8] {
        assert!(initialize(&native,&key,"seed",version,vec![]).await.is_err(), "existing DB cannot be overwritten");
    }
    assert_eq!(g.eval_main("({booted:globalThis.appBooted===true,now:Date.now()})").await.unwrap(),before);
    assert_eq!(before["booted"],false);
    let mut boot = Value::Null;
    for _ in 0..40 {
        g.advance(VirtualDuration(50_000)).await.unwrap();
        let observed = g.eval_main("({value:globalThis.bootValue??null,error:globalThis.bootError??null})").await.unwrap();
        assert!(observed["error"].is_null(),"{observed}");
        boot = observed["value"].clone();
        if !boot.is_null() { break; }
    }
    let first = json!({"id":1,"email":"a","tags":["blue","blue",null]});
    let second = json!({"id":5,"email":"b","tags":["blue","red"]});
    assert_eq!(boot,json!({"version":7,"names":["empty","external","items","pairs"],
        "values":[first.clone(),second.clone()],"tags":[first.clone(),second],"id":first,
        "pairs":{"team":"a","id":2,"value":"compound"},"keys":[{"date":1234}]}));
    println!("IDB_INITIALIZATION_PROBE {}",json!({"invalid_preflight":true,"transaction_rollback":true,
        "existing_database_unchanged":true,"frozen_boundary":before,"first_app_reads":boot}));
    g.stop().await.unwrap();
}
