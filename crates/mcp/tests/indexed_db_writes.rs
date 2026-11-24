//! Native record intervention keeps a stopped boundary and distinguishes null from deletion.
mod support;
use serde_json::{Value, json};

fn present(value: Value) -> Value { json!({"state":"present","value":value}) }
async fn read(m: &mut support::Mcp, id: &Value, field: &Value) -> Value {
    m.call_tool("state",json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":field})).await
}
async fn apply(m: &mut support::Mcp, id: &Value, field: &Value, value: Value, request: &str) -> Value {
    let before=read(m,id,field).await;
    m.call_tool("state",json!({"launch_id":id,"operation":"apply","request_id":request,"boundary":before["boundary"],
        "changes":[{"target":{"by":"document"},"field":field,"value":value,"expected":{"value":before["value"]}}]})).await
}
#[tokio::test]
async fn native_record_writes_commit_indexes_null_deletion_and_replay_without_page_progress() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://record-write.test";
    let page=r#"<!doctype html><button id="read">Read</button><output id="status">idle</output><script>
window.ticks=0;setInterval(()=>ticks++,10);
const result=r=>new Promise((ok,no)=>{r.onsuccess=()=>ok(r.result);r.onerror=()=>no(r.error)});
document.querySelector('#read').onclick=async()=>{
 const db=await result(indexedDB.open('records'));
 const tx=db.transaction(['items','raw']);
 const s=tx.objectStore('items');
 window.loaded=await Promise.all([result(s.index('label').get('new')),result(s.index('tags').getAll('x')),result(tx.objectStore('raw').getAllKeys())]);
 db.close();document.querySelector('#status').textContent='read';
};</script>"#;
    let db=json!({"origin":origin,"name":"records","version":1,"stores":[
        {"name":"items","key_path":"id","indexes":[{"name":"label","key_path":"label","unique":true},{"name":"tags","key_path":"tags","multi_entry":true}],
         "records":[{"value":{"id":1,"label":"old","tags":["a"]}},{"value":{"id":2,"label":"taken"}}]},
        {"name":"raw","auto_increment":true,"records":[]}
    ]});
    let mut bucket=db.clone();bucket["bucket"]=json!("alpha");
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"then":{"type":"none"},
        "profile":{"initial_indexed_db":[db,bucket]},
        "rules":[{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let field=|store:&str,key:Value|json!({"type":"indexed_db_record","scope":scope,"database":"records","store":store,"key":key});
    let item=field("items",json!(1));
    let at=m.call_tool("status",json!({"launch_id":id})).await["t_us"].clone();
    let changed=json!({"id":1,"label":"new","tags":["x","x","y"]});
    let observed=read(&mut m,id,&item).await;
    let (failed,error)=m.try_tool("state",json!({"launch_id":id,"operation":"apply","boundary":observed["boundary"],
        "changes":[{"target":{"by":"document"},"field":item,"value":present(changed.clone()),"expected":{"value":{"state":"absent"}}}]})).await;
    assert!(failed && error["code"]=="observation_changed","{error}");
    assert_eq!(read(&mut m,id,&item).await["value"],observed["value"]);
    let mut supplied=changed.clone();
    supplied["id"]=json!(1.0);
    let applied=apply(&mut m,id,&item,present(supplied),"put-item").await;
    assert_eq!(applied["completed"],true,"{applied}");
    assert_eq!(read(&mut m,id,&item).await["value"],present(changed.clone()));
    let null=field("raw",json!(5));
    assert_eq!(read(&mut m,id,&null).await["value"],json!({"state":"absent"}));
    assert_eq!(apply(&mut m,id,&null,present(Value::Null),"put-null").await["completed"],true);
    assert_eq!(read(&mut m,id,&null).await["value"],present(Value::Null));
    // A failed unique-index put must roll back, retaining the previous record and indexes.
    let conflict=apply(&mut m,id,&item,present(json!({"id":1,"label":"taken"})),"unique-conflict").await;
    assert_eq!(conflict["completed"],false,"{conflict}");
    assert_eq!(read(&mut m,id,&item).await["value"],present(changed.clone()));
    let wrong=apply(&mut m,id,&item,present(json!({"id":9,"label":"wrong"})),"inline-conflict").await;
    assert_eq!(wrong["completed"],false,"{wrong}");
    assert_eq!(read(&mut m,id,&item).await["value"],present(changed.clone()));
    let mut other=item.clone();other["bucket"]=json!("alpha");
    assert_eq!(read(&mut m,id,&other).await["value"]["value"]["label"],"old");
    assert_eq!(apply(&mut m,id,&other,present(json!({"id":1,"label":"bucket"})),"put-bucket").await["completed"],true);
    assert_eq!(read(&mut m,id,&item).await["value"],present(changed.clone()));
    assert_eq!(apply(&mut m,id,&null,json!({"state":"absent"}),"delete-null").await["completed"],true);
    assert_eq!(read(&mut m,id,&null).await["value"],json!({"state":"absent"}));
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],at);
    let globals=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"({ticks, loaded: typeof loaded})"})).await;
    assert_eq!(globals["value"],json!({"ticks":0,"loaded":"undefined"}));
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#read"}},"then":{"type":"none"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"read"},"max_ms":1000}})).await;
    let loaded=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"loaded"})).await;
    assert_eq!(loaded["value"],json!([changed,[changed],[]]),"{loaded}");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn native_write_refuses_an_unfinished_page_read_transaction_without_serving_it() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let page=include_str!("../../recorder/tests/fixtures/idb-records.html");
    let launched=m.call_tool("launch",json!({"url":"https://busy-record.test/start","then":{"type":"none"},
        "rules":[{"id":"page","match":{"url":"https://busy-record.test/start"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"ready"},"max_ms":1000}})).await;
    let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let field=json!({"type":"indexed_db_record","scope":scope,"database":"records","store":"items","key":1});
    m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"globalThis.delivered=false; var read=db.transaction('items').objectStore('items').get(1); read.onsuccess=()=>globalThis.delivered=true; true"})).await;
    let before=m.call_tool("status",json!({"launch_id":id})).await["t_us"].clone();
    let refused=apply(&mut m,id,&field,present(json!("blocked")),"blocked-write").await;
    assert_eq!(refused["completed"],false,"{refused}");
    assert!(refused["receipts"][0]["error"].as_str().unwrap().contains("unfinished transaction"),"{refused}");
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],before);
    assert_eq!(m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"delivered"})).await["value"],false);
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    assert_eq!(apply(&mut m,id,&field,present(json!("after")),"write-after-progress").await["completed"],true);
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn native_writes_keep_frame_partitions_and_named_buckets_separate_in_replay() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let parent="https://idb-parent.test";
    let child_origin="https://idb-child.test";
    let mut databases=Vec::new();
    for origin in [parent,child_origin] {
        for bucket in [None,Some("alpha")] {
            let mut database=json!({"origin":origin,"name":"records","version":1,"stores":[
                {"name":"items","records":[{"key":{"type":"number","value":1},"value":"seed"}]}
            ]});
            if origin==child_origin {database["partition"]=json!({"top_level_site":parent,"has_cross_site_ancestor":true});}
            if let Some(bucket)=bucket {database["bucket"]=json!(bucket);}
            databases.push(database);
        }
    }
    let launched=m.call_tool("launch",json!({"url":format!("{parent}/"),"profile":{"initial_indexed_db":databases},"rules":[
        {"id":"parent","match":{"url":format!("{parent}/")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":format!("<!doctype html><p>parent</p><iframe id=child src='{child_origin}/'></iframe>")}},
        {"id":"child","match":{"url":format!("{child_origin}/")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>child</p>"}}
    ]})).await;
    let id=&launched["launch_id"];
    let targets=[json!({"by":"document"}),json!({"by":"frame","frame":{"by":"document","selector":"#child"},"target":{"by":"document"}})];
    let mut fields=Vec::new();
    for (index,target) in targets.iter().enumerate() {
        let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":target})).await["scope"].clone();
        for bucket in [None,Some("alpha")] {
            let mut field=json!({"type":"indexed_db_record","scope":scope,"database":"records","store":"items","key":1});
            if let Some(bucket)=bucket {field["bucket"]=json!(bucket);}
            let before=m.call_tool("state",json!({"launch_id":id,"operation":"read","target":target,"field":field})).await;
            assert_eq!(before["value"],present(json!("seed")),"{before}");
            let value=present(json!(format!("frame-{index}-{}",bucket.unwrap_or("default"))));
            let changed=m.call_tool("state",json!({"launch_id":id,"operation":"apply","boundary":before["boundary"],"changes":[
                {"target":target,"field":field,"value":value,"expected":{"value":before["value"]}}
            ]})).await;
            assert_eq!(changed["completed"],true,"{changed}");
            assert_eq!(changed["boundary"]["t_us"],before["boundary"]["t_us"]);
            fields.push((index,field,value));
        }
    }
    assert_ne!(fields[0].1["scope"]["storage_key"],fields[2].1["scope"]["storage_key"]);
    for (index,field,value) in &fields {
        let actual=m.call_tool("state",json!({"launch_id":id,"operation":"read","target":targets[*index],"field":field})).await;
        assert_eq!(actual["value"],*value,"{actual}");
        let (refused,error)=m.try_tool("state",json!({"launch_id":id,"operation":"read","target":targets[1-index],"field":field})).await;
        assert!(refused && error["code"]=="invalid_arguments","{error}");
    }
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}
