mod support;
use serde_json::{Value, json};

fn database(origin: &str) -> Value {
    json!({"origin":origin,"name":"boot","version":1,"stores":[{
        "name":"items","key_path":"id","auto_increment":true,
        "indexes":[{"name":"label","key_path":"label"}],
        "records":[{"value":{"id":1,"label":"seed"}},{"value":{"label":"second"}}]
    }]})
}
fn app_rules(origin: &str) -> Value {
    let body = r#"<!doctype html><button id="change">Change</button><output id="status">loading</output><script>
const result=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
async function open(){const r=indexedDB.open('boot',1);r.onupgradeneeded=()=>{const s=r.result.createObjectStore('items',{keyPath:'id',autoIncrement:true});s.createIndex('label','label')};return result(r)}
async function workerRead(){const worker=new Worker('/storage-worker.js');try{return await new Promise((ok,no)=>{worker.onmessage=e=>ok(e.data);worker.onerror=no})}finally{worker.terminate()}}
async function read(){const db=await open();const tx=db.transaction('items');const s=tx.objectStore('items');const [items,match]=await Promise.all([result(s.getAll()),result(s.index('label').get('seed'))]);db.close();const start=Date.now();const [names,otherNames]=await Promise.all([indexedDB.databases(),indexedDB.databases()]);const metadataElapsedMs=Date.now()-start;const worker=await workerRead();const flag=await caches.match('/flag');globalThis.boot={items,match:match??null,local:localStorage.getItem('token'),cache:flag?await flag.text():null,names,otherNames,metadataElapsedMs,worker};document.querySelector('#status').textContent='ready'}
document.querySelector('#change').onclick=async()=>{try{const db=await open();const tx=db.transaction('items','readwrite');const done=new Promise((ok,no)=>{tx.oncomplete=ok;tx.onabort=()=>no(tx.error)});tx.objectStore('items').put({id:1,label:'changed'});await done;db.close();document.querySelector('#status').textContent='changed'}catch(e){document.querySelector('#status').textContent=String(e)}};
read().catch(e=>document.querySelector('#status').textContent=String(e));
</script>"#;
    let worker = r#"const result=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});(async()=>{const db=await result(indexedDB.open('boot',1));const tx=db.transaction('items');const done=new Promise((ok,no)=>{tx.oncomplete=ok;tx.onabort=()=>no(tx.error)});const items=await result(tx.objectStore('items').getAll());await done;db.close();const start=Date.now();const names=await indexedDB.databases();const metadataElapsedMs=Date.now()-start;const cacheStart=Date.now();const response=await caches.match('/flag');const cache=response?await response.text():null;postMessage({items,names,metadataElapsedMs,cache,cacheElapsedMs:Date.now()-cacheStart})})().catch(e=>postMessage({error:String(e)}));"#;
    json!([
        {"id":"app","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
        {"id":"storage-worker","match":{"url":format!("{origin}/storage-worker.js")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":worker}}
    ])
}
fn expected_boot(label: &str, seeded: bool) -> Value {
    let items = if seeded {
        json!([{"id":1,"label":label},{"id":2,"label":"second"}])
    } else {
        json!([])
    };
    let names = json!([{"name":"boot","version":1}]);
    json!({"items":items,"match":if label=="seed" {json!({"id":1,"label":"seed"})} else {Value::Null},
        "local":if seeded {json!("local")} else {Value::Null},
        "cache":if seeded {json!("cache")} else {Value::Null},
        "names":names,"otherNames":names,"metadataElapsedMs":0,
        "worker":{"items":items,"names":names,"metadataElapsedMs":0,"cache":if seeded{json!("cache")}else{Value::Null},"cacheElapsedMs":0}})
}
async fn evaluate(m: &mut support::Mcp, id: &Value, expression: &str) -> Value {
    m.call_tool(
        "debug",
        json!({"launch_id":id,"operation":"evaluate","expression":expression}),
    )
    .await["value"]
        .clone()
}
async fn ready(m: &mut support::Mcp, id: &Value, text: &str) {
    let result=m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"document","selector":"#status"},"text":text},"max_ms":2000}})).await;
    assert_eq!(result["outcome"]["type"], "condition_met", "{result}");
    assert_eq!(
        evaluate(m, id, "document.querySelector('#status').textContent").await,
        json!(text),
        "{result}"
    );
}
#[tokio::test]
async fn indexed_db_and_other_storage_precede_scripts_and_survive_reload_isolation_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://indexed-db.example.test";
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":app_rules(origin),"then":{"type":"none"},"profile":{
        "initial_indexed_db":[database(origin)],
        "initial_storage":{"origin":origin,"local":{"token":"local"}},
        "initial_caches":[{"origin":origin,"name":"flags","entries":[{"request_url":format!("{origin}/flag"),"status":200,"body":"cache"}]}]
    }})).await;
    let id = &launched["launch_id"];
    assert_eq!(
        evaluate(&mut m, id, "typeof boot").await,
        json!("undefined")
    );
    ready(&mut m, id, "ready").await;
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected_boot("seed", true)
    );
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "changed").await;
    // IndexedDB reads natively as the page left it, with no virtual time passing.
    let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].clone();
    let scope = m.call_tool("state", json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let read = |extra: Value| {
        let mut field = json!({"type":"indexed_db","scope":scope});
        for (k, v) in extra.as_object().unwrap() { field[k] = v.clone(); }
        json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":field})
    };
    assert_eq!(m.call_tool("state", read(json!({}))).await["value"], json!([{"name":"boot","version":1}]));
    let described = m.call_tool("state", read(json!({"database":"boot"}))).await;
    assert_eq!(described["value"]["stores"], json!([{"name":"items","key_path":"id","auto_increment":true,"indexes":["label"],"count":2}]), "{described}");
    let items = m.call_tool("state", read(json!({"database":"boot","store":"items"}))).await;
    assert_eq!(items["value"], json!({"records":[{"key":1,"value":{"id":1,"label":"changed"}},{"key":2,"value":{"id":2,"label":"second"}}],"total":2}), "{items}");
    assert_eq!(m.call_tool("state", read(json!({"database":"boot","store":"items","key":2}))).await["value"], json!({"id":2,"label":"second"}));
    assert_eq!(m.call_tool("state", read(json!({"database":"boot","store":"items","key":9}))).await["value"], Value::Null);
    let (missing, error) = m.try_tool("state", read(json!({"database":"absent"}))).await;
    assert!(missing && error["code"] == "not_found", "{error}");
    let (missing, error) = m.try_tool("state", read(json!({"database":"boot","store":"absent"}))).await;
    assert!(missing && error["code"] == "not_found", "{error}");
    assert_eq!(m.call_tool("state", read(json!({}))).await["value"], json!([{"name":"boot","version":1}]), "reading a missing database creates none");
    let (refused, error) = m.try_tool("state", json!({"launch_id":id,"operation":"apply","boundary":described["boundary"],"changes":[
        {"target":{"by":"document"},"field":{"type":"indexed_db","scope":scope,"database":"boot"},"value":null}]})).await;
    assert!(refused && error["code"] == "invalid_arguments", "read only: {error}");
    assert_eq!(m.call_tool("status", json!({"launch_id":id})).await["t_us"], before);
    m.call_tool(
        "act",
        json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}}),
    )
    .await;
    ready(&mut m, id, "ready").await;
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected_boot("changed", true)
    );
    let clean=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":app_rules(origin),"then":{"type":"none"}})).await;
    ready(&mut m, &clean["launch_id"], "ready").await;
    assert_eq!(
        evaluate(&mut m, &clean["launch_id"], "boot").await,
        expected_boot("", false)
    );
    m.call_tool("stop", json!({"launch_id":clean["launch_id"]}))
        .await;
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!(
        "{}/trace.jsonl",
        stopped["bundle"].as_str().unwrap()
    ))
    .unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let receipts: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "indexed_db_initialized")
        .collect();
    assert_eq!(receipts.len(), 1, "reload must not reseed");
    assert_eq!(receipts[0]["t_us"], 0);
    assert_eq!(receipts[0]["payload"]["database"]["origin"], origin);
    assert_eq!(
        receipts[0]["payload"]["database"]["stores"][0]["records"][0]["value"]["label"],
        "seed"
    );
    let replay = m
        .call_tool(
            "debug",
            json!({"launch_id":id,"operation":"replay","url":"https://moved.example.test/start"}),
        )
        .await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn stopped_reads_refuse_what_the_pages_unfinished_work_holds() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://unfinished.example.test";
    let body = r#"<!doctype html><button id="write">Write</button><button id="upgrade">Upgrade</button><output id="status">loading</output><script>
const status=t=>document.querySelector('#status').textContent=t;let db;
const open=v=>{const r=indexedDB.open('d',v);r.onupgradeneeded=()=>{for(const n of ['s','t','u'].slice(0,v+1))if(!r.result.objectStoreNames.contains(n))r.result.createObjectStore(n)};r.onsuccess=()=>{db=r.result;db.onversionchange=()=>db.close();status('open '+db.version)}};
open(1);
document.querySelector('#write').onclick=()=>{const tx=db.transaction('s','readwrite');tx.objectStore('s').put('v','k');tx.oncomplete=()=>status('written')};
document.querySelector('#upgrade').onclick=()=>open(2);
</script>"#;
    let rules = json!([{"id":"app","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]);
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"rules":rules})).await;
    let id = &launched["launch_id"];
    ready(&mut m, id, "open 1").await;
    let scope = m.call_tool("state", json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let read = |extra: Value| {
        let mut field = json!({"type":"indexed_db","scope":scope,"database":"d"});
        for (k, v) in extra.as_object().unwrap() { field[k] = v.clone(); }
        json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":field})
    };
    let click = |selector: &str| json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":selector}},"then":{"type":"none"}});
    // The page's write transaction has not run its request's callback, so it cannot commit yet.
    m.call_tool("act", click("#write")).await;
    let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].clone();
    let started = std::time::Instant::now();
    let (refused, error) = m.try_tool("state", read(json!({"store":"s"}))).await;
    assert!(refused && error["code"] == "page_work_pending", "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "refused at once: {:?}", started.elapsed());
    assert_eq!(m.call_tool("state", read(json!({"store":"t"}))).await["value"], json!({"records":[],"total":0}), "another store reads");
    assert_eq!(m.call_tool("status", json!({"launch_id":id})).await["t_us"], before);
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":10}})).await;
    ready(&mut m, id, "written").await;
    assert_eq!(m.call_tool("state", read(json!({"store":"s"}))).await["value"], json!({"records":[{"key":"k","value":"v"}],"total":1}));
    // An upgrade waits for the page's own connection to close and then for its upgrade handler.
    m.call_tool("act", click("#upgrade")).await;
    let (refused, error) = m.try_tool("state", read(json!({}))).await;
    assert!(refused && error["code"] == "page_work_pending", "{error}");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":10}})).await;
    ready(&mut m, id, "open 2").await;
    let names: Vec<Value> = m.call_tool("state", read(json!({}))).await["value"]["stores"].as_array().unwrap().iter().map(|s| s["name"].clone()).collect();
    assert_eq!(names, [json!("s"), json!("t"), json!("u")]);
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn indexed_db_rejects_invalid_inputs_and_failed_transactions_do_not_publish_a_generation() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    assert!(m.definition("state", "recipe", "InitialDatabase").await["properties"]["stores"].is_object());
    let origin = "https://indexed-db.example.test";
    for (pointer, value) in [
        ("/version", json!(0)),
        ("/version", json!(9_007_199_254_740_992u64)),
        ("/origin", json!("http://insecure.example.test")),
        ("/stores/0/key_path", json!(["a", "b"])),
        (
            "/stores/0/records/0/value/id",
            json!(9_007_199_254_740_992u64),
        ),
        (
            "/stores/0/indexes/0",
            json!({"name":"label","key_path":["a","b"],"multi_entry":true}),
        ),
    ] {
        let mut invalid = database(origin);
        *invalid.pointer_mut(pointer).unwrap() = value;
        let (failed, error) = m
            .try_tool(
                "launch",
                json!({"url":format!("{origin}/start"),"profile":{"initial_indexed_db":[invalid]}}),
            )
            .await;
        assert!(
            failed && error["code"] == "invalid_arguments",
            "{pointer}: {error}"
        );
    }
    let mut conflict = database(origin);
    conflict["stores"][0]["indexes"][0]["unique"] = json!(true);
    conflict["stores"][0]["records"][1]["value"]["label"] = json!("seed");
    let (failed,error)=m.try_tool("launch",json!({"url":format!("{origin}/start"),"rules":app_rules(origin),"then":{"type":"none"},"profile":{"initial_indexed_db":[conflict]}})).await;
    assert!(failed && error["launch_id"].is_null(), "{error}");
    let inventory = m.call_tool("session", json!({"operation":"list"})).await;
    assert!(
        inventory["generations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["state"] == "failed"),
        "{inventory}"
    );
    let fresh=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":app_rules(origin),"then":{"type":"none"},"profile":{"initial_indexed_db":[database(origin)]}})).await;
    ready(&mut m, &fresh["launch_id"], "ready").await;
    assert_eq!(
        evaluate(&mut m, &fresh["launch_id"], "boot.items.length").await,
        json!(2)
    );
    m.call_tool("stop", json!({"launch_id":fresh["launch_id"]}))
        .await;
}

#[tokio::test]
async fn named_buckets_isolate_same_named_stores_before_scripts_through_reload_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://buckets.example.test";
    let body = r#"<!doctype html><button id="change">Change</button><output id="status">loading</output><script>
const request=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
async function scope(name){return name?await navigator.storageBuckets.open(name):{indexedDB,caches}}
async function dbFor(s){const r=s.indexedDB.open('boot',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');return request(r)}
async function read(){const values=[];for(const name of [null,'alpha','beta']){const s=await scope(name);const db=await dbFor(s);const value=await request(db.transaction('items').objectStore('items').get('value'));db.close();const response=await s.caches.match('/flag');values.push({bucket:name,db:value??null,cache:response?await response.text():null})}globalThis.boot=values;document.querySelector('#status').textContent='ready'}
document.querySelector('#change').onclick=async()=>{try{const s=await scope('alpha');const db=await dbFor(s);const tx=db.transaction('items','readwrite');const done=new Promise((ok,no)=>{tx.oncomplete=ok;tx.onabort=()=>no(tx.error)});tx.objectStore('items').put('changed','value');await done;db.close();await(await s.caches.open('flags')).put('/flag',new Response('changed'));document.querySelector('#status').textContent='changed'}catch(e){document.querySelector('#status').textContent=String(e)}};
read().catch(e=>document.querySelector('#status').textContent=String(e));
</script>"#;
    let rules = json!([{"id":"app","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]);
    let mut databases = Vec::new();
    let mut caches = Vec::new();
    for bucket in [None, Some("alpha"), Some("beta")] {
        let value = bucket.unwrap_or("default");
        let mut db = json!({"origin":origin,"name":"boot","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"value"},"value":value}]}]});
        let mut cache = json!({"origin":origin,"name":"flags","entries":[{"request_url":format!("{origin}/flag"),"status":200,"body":value}]});
        if let Some(name) = bucket {
            db["bucket"] = json!(name);
            cache["bucket"] = json!(name);
        }
        databases.push(db);
        caches.push(cache);
    }
    // Invalid names must be refused before browser publication for either provider.
    for name in ["", "Upper", "_start", "has space", "a/b", &"a".repeat(64)] {
        for provider in ["initial_caches", "initial_indexed_db"] {
            let mut entry = if provider == "initial_caches" {
                caches[0].clone()
            } else {
                databases[0].clone()
            };
            entry["bucket"] = json!(name);
            let (failed, error) = m
                .try_tool(
                    "launch",
                    json!({"url":format!("{origin}/start"),"profile":{provider:[entry]}}),
                )
                .await;
            assert!(
                failed && error["code"] == "invalid_arguments",
                "{provider} {name}: {error}"
            );
        }
    }
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"rules":rules,"then":{"type":"none"},"profile":{"initial_caches":caches,"initial_indexed_db":databases}})).await;
    let id = &launched["launch_id"];
    assert_eq!(
        evaluate(&mut m, id, "typeof boot").await,
        json!("undefined")
    );
    ready(&mut m, id, "ready").await;
    let expected = |alpha: &str| {
        json!([
            {"bucket":null,"db":"default","cache":"default"},
            {"bucket":"alpha","db":alpha,"cache":alpha},
            {"bucket":"beta","db":"beta","cache":"beta"}
        ])
    };
    assert_eq!(evaluate(&mut m, id, "boot").await, expected("alpha"));
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "changed").await;
    m.call_tool(
        "act",
        json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}}),
    )
    .await;
    ready(&mut m, id, "ready").await;
    assert_eq!(evaluate(&mut m, id, "boot").await, expected("changed"));
    let clean = m
        .call_tool(
            "launch",
            json!({"url":format!("{origin}/start"),"rules":rules,"then":{"type":"none"}}),
        )
        .await;
    ready(&mut m, &clean["launch_id"], "ready").await;
    assert_eq!(
        evaluate(&mut m, &clean["launch_id"], "boot").await,
        json!([
            {"bucket":null,"db":null,"cache":null},{"bucket":"alpha","db":null,"cache":null},{"bucket":"beta","db":null,"cache":null}
        ])
    );
    m.call_tool("stop", json!({"launch_id":clean["launch_id"]}))
        .await;
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!(
        "{}/trace.jsonl",
        stopped["bundle"].as_str().unwrap()
    ))
    .unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (kind, payload) in [
        ("cache_initialized", "cache"),
        ("indexed_db_initialized", "database"),
    ] {
        let receipts: Vec<_> = events.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(receipts.len(), 3, "{kind}: reload must not reseed");
        for (receipt, bucket) in receipts
            .iter()
            .zip([Value::Null, json!("alpha"), json!("beta")])
        {
            assert_eq!(receipt["t_us"], 0);
            assert_eq!(receipt["payload"][payload]["bucket"], bucket);
        }
    }
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay","url":"https://moved-buckets.example.test/start"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn explicit_origins_and_partitions_seed_child_first_reads_without_cross_partition_leaks() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let parent = "https://scope-parent.test";
    let other = "https://scope-other.test";
    let child = "https://scope-child.test";
    let child_body = r#"<!doctype html><output id="status">loading</output><script>
const request=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
async function open(factory=indexedDB){const r=factory.open('boot',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');return request(r)}
async function read(){const db=await open();const value=await request(db.transaction('items').objectStore('items').get('value'));db.close();const response=await caches.match('/flag');const bucket=await navigator.storageBuckets.open('named');const namedDb=await open(bucket.indexedDB);const namedValue=await request(namedDb.transaction('items').objectStore('items').get('value'));namedDb.close();const namedResponse=await bucket.caches.match('/flag');globalThis.boot={db:value??null,cache:response?await response.text():null,namedDb:namedValue??null,namedCache:namedResponse?await namedResponse.text():null};document.querySelector('#status').textContent='ready';if(parent!==window)parent.postMessage({kind:'ready',value:boot},'*')}
addEventListener('message',async e=>{if(e.data!=='change')return;try{const db=await open();const tx=db.transaction('items','readwrite');const done=new Promise((ok,no)=>{tx.oncomplete=ok;tx.onabort=()=>no(tx.error)});tx.objectStore('items').put('changed','value');await done;db.close();await(await caches.open('flags')).put('/flag',new Response('changed'));parent.postMessage({kind:'changed'},'*')}catch(e){parent.postMessage({kind:'error',value:String(e)},'*')}});
read().catch(e=>{document.querySelector('#status').textContent=String(e);if(parent!==window)parent.postMessage({kind:'error',value:String(e)},'*')});
</script>"#;
    let parent_body = format!(
        r#"<!doctype html><button id="change">Change</button><output id="status">loading</output><script>addEventListener('message',e=>{{if(e.source!==document.querySelector('iframe').contentWindow)return;if(e.data.kind==='ready')globalThis.boot=e.data.value;document.querySelector('#status').textContent=e.data.kind}});document.querySelector('#change').onclick=()=>document.querySelector('iframe').contentWindow.postMessage('change','*');</script><iframe src="{child}/child"></iframe>"#
    );
    let rules = json!([
        {"id":"parent","match":{"url":format!("{parent}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":parent_body}},
        {"id":"other","match":{"url":format!("{other}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":parent_body}},
        {"id":"child","match":{"url":format!("{child}/child")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child_body}}
    ]);
    let mut databases = Vec::new();
    let mut caches = Vec::new();
    for (site, value) in [
        (None, "first-party"),
        (Some(parent), "parent-partition"),
        (Some(other), "other-partition"),
    ] {
        let mut db = json!({"origin":child,"name":"boot","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"value"},"value":value}]}]});
        let mut cache = json!({"origin":child,"name":"flags","entries":[{"request_url":format!("{child}/flag"),"status":200,"body":value}]});
        if let Some(site) = site {
            let partition = json!({"top_level_site":site,"has_cross_site_ancestor":true});
            db["partition"] = partition.clone();
            cache["partition"] = partition;
        }
        let mut named_db = db.clone();
        named_db["bucket"] = json!("named");
        named_db["stores"][0]["records"][0]["value"] = json!(format!("{value}-named"));
        let mut named_cache = cache.clone();
        named_cache["bucket"] = json!("named");
        named_cache["entries"][0]["body"] = json!(format!("{value}-named"));
        databases.extend([db, named_db]);
        caches.extend([cache, named_cache]);
    }
    let mut alias = caches[0].clone();
    alias["partition"] = json!({"top_level_site":child,"has_cross_site_ancestor":false});
    let invalid = json!({"request_id":"native-scope-alias","url":format!("{parent}/start"),"rules":rules,
        "then":{"type":"none"},"profile":{"initial_caches":[caches[0],alias]}});
    let (failed, error) = m.try_tool("launch", invalid.clone()).await;
    assert!(
        failed
            && error["code"] == "invalid_arguments"
            && error["launch_id"].is_null()
            && error["message"]
                .as_str()
                .is_some_and(|m| m.contains("same provider/key/bucket/name")),
        "native identity aliases must retain their typed refusal: {error}"
    );
    assert!(
        error["recovery"]
            .as_str()
            .unwrap()
            .contains("new request_id")
    );
    assert_eq!(
        m.try_tool("launch", invalid).await,
        (true, error),
        "retry must retain the original terminal outcome"
    );
    let inventory = m.call_tool("session", json!({"operation":"list"})).await;
    assert_eq!(
        inventory["generations"].as_array().unwrap().len(),
        1,
        "retry must not launch a second generation"
    );
    let mut contradictory = caches[2].clone();
    contradictory["partition"]["has_cross_site_ancestor"] = json!(false);
    let (failed, error) = m
        .try_tool(
            "launch",
            json!({"url":format!("{parent}/start"),"rules":rules,
        "then":{"type":"none"},"profile":{"initial_caches":[contradictory]}}),
        )
        .await;
    assert!(
        failed
            && error["code"] == "invalid_arguments"
            && error["message"]
                .as_str()
                .is_some_and(|m| m.contains("cross-site ancestor")),
        "{error}"
    );
    let launched=m.call_tool("launch",json!({"url":format!("{parent}/start"),"rules":rules,"then":{"type":"none"},"profile":{"initial_caches":caches,"initial_indexed_db":databases}})).await;
    let id = &launched["launch_id"];
    assert_eq!(
        evaluate(&mut m, id, "typeof boot").await,
        json!("undefined")
    );
    ready(&mut m, id, "ready").await;
    let expected = |value: &str| {
        let named = if value == "changed" {
            "parent-partition"
        } else {
            value
        };
        json!({"db":value,"cache":value,"namedDb":format!("{named}-named"),"namedCache":format!("{named}-named")})
    };
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected("parent-partition")
    );
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "changed").await;
    m.call_tool(
        "act",
        json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}}),
    )
    .await;
    ready(&mut m, id, "ready").await;
    assert_eq!(evaluate(&mut m, id, "boot").await, expected("changed"));
    for (url, value) in [
        (format!("{other}/start"), "other-partition"),
        (format!("{child}/child"), "first-party"),
        (format!("{parent}/start"), "changed"),
    ] {
        m.call_tool(
            "act",
            json!({"launch_id":id,"action":{"type":"navigate","url":url},"then":{"type":"none"}}),
        )
        .await;
        ready(&mut m, id, "ready").await;
        assert_eq!(evaluate(&mut m, id, "boot").await, expected(value));
    }
    let clean = m
        .call_tool(
            "launch",
            json!({"url":format!("{parent}/start"),"rules":rules,"then":{"type":"none"}}),
        )
        .await;
    ready(&mut m, &clean["launch_id"], "ready").await;
    assert_eq!(
        evaluate(&mut m, &clean["launch_id"], "boot").await,
        json!({"db":null,"cache":null,"namedDb":null,"namedCache":null})
    );
    m.call_tool("stop", json!({"launch_id":clean["launch_id"]}))
        .await;
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!(
        "{}/trace.jsonl",
        stopped["bundle"].as_str().unwrap()
    ))
    .unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for kind in ["cache_initialized", "indexed_db_initialized"] {
        let receipts: Vec<_> = events.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(receipts.len(), 6, "{kind}");
        let keys: std::collections::BTreeSet<_> = receipts
            .iter()
            .map(|e| e["payload"]["storage_key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys.len(),
            3,
            "partitions must retain distinct native identities"
        );
        assert!(receipts.iter().all(|e| e["t_us"] == 0));
    }
    let replay = m
        .call_tool(
            "debug",
            json!({"launch_id":id,"operation":"replay","url":"https://moved-parent.test/start"}),
        )
        .await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn same_site_origins_and_cross_site_ancestors_keep_distinct_storage_through_reload_and_replay()
 {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://app.ancestry.test";
    let sibling = "https://sub.ancestry.test";
    let cross = "https://cross-ancestor.test";
    let site = "https://ancestry.test";
    let functions = r#"
const request=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
async function open(){const r=indexedDB.open('boot',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');return request(r)}
async function read(){const db=await open();const value=await request(db.transaction('items').objectStore('items').get('value'));db.close();const response=await caches.match('/flag');return {db:value??null,cache:response?await response.text():null}}
"#;
    let leaf = |role: &str| {
        format!(
            "<!doctype html><script>{functions}read().then(value=>parent.postMessage({{role:'{role}',value}},'*')).catch(e=>parent.postMessage({{role:'error',value:String(e)}},'*'));</script>"
        )
    };
    let bridge = format!(
        r#"<!doctype html><script>addEventListener('message',e=>{{if(e.source===document.querySelector('iframe').contentWindow)parent.postMessage(e.data,'*')}});</script><iframe src="{origin}/nested"></iframe>"#
    );
    let parent = format!(
        r#"<!doctype html><button id="change">Change root</button><output id="status">loading</output><script>
{functions}
globalThis.boot={{}};function accept(role,value){{boot[role]=value;if(Object.keys(boot).length===4)document.querySelector('#status').textContent='ready'}}
addEventListener('message',e=>{{const expected={{direct:'direct',sibling:'sibling',nested:'bridge'}}[e.data.role];if(!expected||e.source!==document.querySelector('#'+expected).contentWindow)return;accept(e.data.role,e.data.value)}});
read().then(value=>accept('root',value)).catch(e=>document.querySelector('#status').textContent=String(e));
document.querySelector('#change').onclick=async()=>{{try{{const db=await open();const tx=db.transaction('items','readwrite');const done=new Promise((ok,no)=>{{tx.oncomplete=ok;tx.onabort=()=>no(tx.error)}});tx.objectStore('items').put('changed','value');await done;db.close();await(await caches.open('flags')).put('/flag',new Response('changed'));document.querySelector('#status').textContent='changed'}}catch(e){{document.querySelector('#status').textContent=String(e)}}}};
</script><iframe id="direct" src="{origin}/direct"></iframe><iframe id="sibling" src="{sibling}/leaf"></iframe><iframe id="bridge" src="{cross}/bridge"></iframe>"#
    );
    let rules:Vec<_>=[("parent",format!("{origin}/start"),parent),("direct",format!("{origin}/direct"),leaf("direct")),
        ("sibling",format!("{sibling}/leaf"),leaf("sibling")),("bridge",format!("{cross}/bridge"),bridge),
        ("nested",format!("{origin}/nested"),leaf("nested"))].into_iter().map(|(id,url,body)|json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}})).collect();
    let mut caches = Vec::new();
    let mut databases = Vec::new();
    for (scope_origin, ancestor, value) in [
        (origin, false, "root"),
        (sibling, false, "sibling"),
        (origin, true, "nested"),
    ] {
        let partition = json!({"top_level_site":site,"has_cross_site_ancestor":ancestor});
        caches.push(json!({"origin":scope_origin,"partition":partition,"name":"flags","entries":[{"request_url":format!("{scope_origin}/flag"),"status":200,"body":value}]}));
        databases.push(json!({"origin":scope_origin,"partition":partition,"name":"boot","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"value"},"value":value}]}]}));
    }
    let launch=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":rules,"then":{"type":"none"},"profile":{"initial_caches":caches,"initial_indexed_db":databases}})).await;
    let id = &launch["launch_id"];
    assert_eq!(
        evaluate(&mut m, id, "({boot:typeof boot,now:Date.now()})").await,
        json!({"boot":"undefined","now":946684800000i64}),
        "scope initialization must preserve the native stopped boundary"
    );
    ready(&mut m, id, "ready").await;
    let expected = |value: &str| json!({"root":{"db":value,"cache":value},"direct":{"db":value,"cache":value},"sibling":{"db":"sibling","cache":"sibling"},"nested":{"db":"nested","cache":"nested"}});
    assert_eq!(evaluate(&mut m, id, "boot").await, expected("root"));
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "changed").await;
    m.call_tool(
        "act",
        json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}}),
    )
    .await;
    ready(&mut m, id, "ready").await;
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected("changed"),
        "same-origin direct frame shares root storage; cross-site ancestry remains isolated"
    );
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!(
        "{}/trace.jsonl",
        stopped["bundle"].as_str().unwrap()
    ))
    .unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for kind in ["cache_initialized", "indexed_db_initialized"] {
        let receipts: Vec<_> = events.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(receipts.len(), 3);
        let keys: std::collections::BTreeSet<_> = receipts
            .iter()
            .map(|e| e["payload"]["storage_key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys.len(),
            3,
            "same site and origin do not erase the ancestor partition"
        );
    }
    let replay = m
        .call_tool(
            "debug",
            json!({"launch_id":id,"operation":"replay","url":"https://moved.ancestry.test/start"}),
        )
        .await;
    if replay["equal"] != true {
        let replay_trace = std::fs::read_to_string(
            ws.root().join("runs").join(replay["run"].as_str().unwrap()).join("trace.jsonl"),
        ).unwrap();
        eprintln!("recorded trace:\n{trace}\nreplayed trace:\n{replay_trace}");
    }
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn replay_relocates_registrable_partition_sites_without_rewriting_app_data_or_other_sites() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://app.source.co.uk:8443";
    let site = "https://source.co.uk";
    let body = r#"<!doctype html><output id="status">loading</output><script>
(async()=>{
 const db=await new Promise((ok,no)=>{const r=indexedDB.open('boot',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
 const value=await new Promise((ok,no)=>{const r=db.transaction('items').objectStore('items').get('value');r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});db.close();
 const response=await caches.match('/flag');
 document.querySelector('#status').textContent=String(value)+'|'+(response?await response.text():'missing');
})().catch(e=>document.querySelector('#status').textContent=String(e));
</script>"#;
    let mut caches = Vec::new();
    let mut databases = Vec::new();
    let mut buckets = Vec::new();
    for (scope_origin, top_site, ancestor, value) in [
        (origin, site, false, site),
        ("https://sibling.source.co.uk", site, false, "sibling"),
        (
            "https://fixed.test",
            "https://external.test",
            true,
            "unrelated",
        ),
    ] {
        let partition = json!({"top_level_site":top_site,"has_cross_site_ancestor":ancestor});
        buckets.push(json!({"origin":scope_origin,"partition":partition,"quota":"0","persistent":true,"durability":"relaxed"}));
        buckets.push(json!({"origin":scope_origin,"partition":partition,"name":"empty-policy","expiration":"15746918400000000","quota":"2097152","persistent":true,"durability":"strict"}));
        caches.push(json!({"origin":scope_origin,"partition":partition,"name":"flags","entries":[{"request_url":format!("{scope_origin}/flag"),"status":200,"body":value}]}));
        databases.push(json!({"origin":scope_origin,"partition":partition,"name":"boot","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"value"},"value":value}]}]}));
    }
    let launch = m.call_tool("launch", json!({
        "url":format!("{origin}/start"),"then":{"type":"none"},
        "rules":[{"id":"app","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}],
        "profile":{"initial_buckets":buckets,"initial_caches":caches,"initial_indexed_db":databases}
    })).await;
    let id = &launch["launch_id"];
    ready(&mut m, id, &format!("{site}|{site}")).await;
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace_path = std::path::Path::new(stopped["bundle"].as_str().unwrap()).join("trace.jsonl");
    let before = std::fs::read(&trace_path).unwrap();
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay","url":"https://app.destination.co.uk:9443/start"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
    assert_eq!(
        std::fs::read(trace_path).unwrap(),
        before,
        "relocation must not edit source evidence"
    );
    let replay_trace = std::fs::read_to_string(
        ws.root()
            .join("runs")
            .join(replay["run"].as_str().unwrap())
            .join("trace.jsonl"),
    )
    .unwrap();
    let events: Vec<Value> = replay_trace
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for (kind, field) in [
        ("cache_initialized", "cache"),
        ("indexed_db_initialized", "database"),
    ] {
        let receipts: Vec<_> = events.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(receipts.len(), 3);
        assert_eq!(
            receipts[0]["payload"][field]["partition"]["top_level_site"],
            "https://destination.co.uk"
        );
        assert_eq!(
            receipts[1]["payload"][field]["partition"]["top_level_site"],
            "https://destination.co.uk"
        );
        assert_eq!(
            receipts[1]["payload"][field]["partition"]["has_cross_site_ancestor"],
            true
        );
        assert_eq!(
            receipts[1]["payload"][field]["origin"],
            "https://sibling.source.co.uk"
        );
        assert_eq!(
            receipts[2]["payload"][field]["partition"]["top_level_site"],
            "https://external.test"
        );
        assert_eq!(
            receipts[2]["payload"][field]["origin"],
            "https://fixed.test"
        );
    }
}
