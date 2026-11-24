//! Cache intervention uses native matching while the application remains stopped.
mod support;
use serde_json::{Value, json};

async fn read(m: &mut support::Mcp, id: &Value, field: &Value) -> Value {
    m.call_tool("state",json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":field})).await
}
async fn apply(m: &mut support::Mcp, id: &Value, field: &Value, value: Value, request: &str) -> Value {
    let before=read(m,id,field).await;
    m.call_tool("state",json!({"launch_id":id,"operation":"apply","request_id":request,"boundary":before["boundary"],
        "changes":[{"target":{"by":"document"},"field":field,"value":value,"expected":{"value":before["value"]}}]})).await
}
fn response(body: &str) -> Value {
    json!({"state":"present","response":{"status":200,"headers":{"Vary":"Accept-Language","X-Source":"native"},"body":body}})
}

#[tokio::test]
async fn vary_matching_binary_buckets_and_deletion_preserve_the_stopped_application() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-write.test";
    let page=r#"<!doctype html><button id="read">Read</button><output id="status">idle</output><script>
window.ticks=0;setInterval(()=>ticks++,10);
document.querySelector('#read').onclick=async()=>{
 const cache=await caches.open('variants');
 const en=await cache.match(new Request('/item',{headers:{'Accept-Language':'en'}}));
 const ko=await cache.match(new Request('/item',{headers:{'Accept-Language':'ko'}}));
 const binary=await cache.match('/bytes');
 const bucket=await navigator.storageBuckets.open('alpha');
 const other=await (await bucket.caches.open('variants')).match('/item');
 window.loaded={en:en?await en.text():null,ko:ko?await ko.text():null,
   bytes:Array.from(new Uint8Array(await binary.arrayBuffer())),other:await other.text(),names:await caches.keys()};
 document.querySelector('#status').textContent='read';
};</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"then":{"type":"none"},
        "profile":{"initial_caches":[{"origin":origin,"bucket":"alpha","name":"variants","entries":[{"request_url":format!("{origin}/item"),"status":200,"body":"bucket"}]}]},
        "rules":[{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let field=|path:&str,lang:&str|json!({"type":"cache_entry","scope":scope,"cache":"variants","request_url":format!("{origin}/{path}"),"request_headers":{"Accept-Language":lang}});
    let en=field("item","en");let ko=field("item","ko");
    let at=m.call_tool("status",json!({"launch_id":id})).await["t_us"].clone();
    assert_eq!(read(&mut m,id,&en).await["value"],json!({"state":"absent"}));
    assert_eq!(apply(&mut m,id,&en,response("hello"),"english").await["completed"],true);
    let english=read(&mut m,id,&en).await;
    assert_eq!(english["value"]["response"]["body_base64"],"aGVsbG8=");
    assert_eq!(english["value"]["response"]["headers"]["vary"],"Accept-Language");
    assert_eq!(read(&mut m,id,&ko).await["value"],json!({"state":"absent"}));
    assert_eq!(apply(&mut m,id,&ko,json!({"state":"absent"}),"delete-unmatched-korean").await["completed"],true);
    assert_eq!(read(&mut m,id,&en).await["value"],english["value"],"a mismatched Vary delete must preserve the existing response");
    // Chromium's put replaces by URL, regardless of the old response's Vary.
    assert_eq!(apply(&mut m,id,&ko,response("다시"),"replace-korean").await["completed"],true);
    assert_eq!(read(&mut m,id,&en).await["value"],json!({"state":"absent"}));
    assert_eq!(apply(&mut m,id,&en,json!({"state":"absent"}),"delete-english").await["completed"],true);
    assert_eq!(read(&mut m,id,&en).await["value"],json!({"state":"absent"}));
    assert_eq!(read(&mut m,id,&ko).await["value"]["response"]["body_base64"],frontcap_core::encode::base64("다시".as_bytes()));
    let bytes=field("bytes","");
    assert_eq!(apply(&mut m,id,&bytes,json!({"state":"present","response":{"status":200,"body_base64":"AP+A"}}),"binary").await["completed"],true);
    let mut missing=field("item","");missing["cache"]=json!("absent-cache");
    assert_eq!(apply(&mut m,id,&missing,json!({"state":"absent"}),"delete-missing").await["completed"],true);
    let mut named=field("item","");named["bucket"]=json!("alpha");
    assert_eq!(read(&mut m,id,&named).await["value"]["response"]["body_base64"],"YnVja2V0");
    assert_eq!(apply(&mut m,id,&named,json!({"state":"present","response":{"status":200,"body":"bucket-updated"}}),"named-bucket-write").await["completed"],true);
    let mut wrong=ko.clone();wrong["scope"]["origin"]=json!("https://other.test");
    let (refused,error)=m.try_tool("state",json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":wrong})).await;
    assert!(refused && error["code"]=="invalid_arguments","{error}");
    let before=read(&mut m,id,&missing).await;
    let (refused,error)=m.try_tool("state",json!({"launch_id":id,"operation":"apply","boundary":before["boundary"],"changes":[
        {"target":{"by":"document"},"field":missing,"value":response("must-not-write")},
        {"target":{"by":"document"},"field":ko,"value":{"state":"present","response":{"status":206,"body":"invalid"}}}
    ]})).await;
    assert!(refused,"{error}");
    assert_eq!(read(&mut m,id,&missing).await["value"],json!({"state":"absent"}));
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],at);
    assert_eq!(m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"({ticks,loaded:typeof loaded})"})).await["value"],json!({"ticks":0,"loaded":"undefined"}));
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#read"}},"then":{"type":"none"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"read"},"max_ms":1000}})).await;
    let loaded=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"loaded"})).await["value"].clone();
    assert_eq!(loaded,json!({"en":null,"ko":"다시","bytes":[0,255,128],"other":"bucket-updated","names":["variants"]}));
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn live_cache_fields_follow_the_selected_frame_storage_partition() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let launched=m.call_tool("launch",json!({"url":"https://cache-top.test/","rules":[
        {"id":"top","match":{"url":"https://cache-top.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>top</p><iframe id=child src='https://cache-child.test/'></iframe>"}},
        {"id":"child","match":{"url":"https://cache-child.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>child</p>"}}
    ]})).await;
    let id=&launched["launch_id"];
    let top=json!({"by":"document"});
    let child=json!({"by":"frame","frame":{"by":"document","selector":"#child"},"target":{"by":"document"}});
    let mut fields=Vec::new();
    for (target,text) in [(&top,"top"),(&child,"child")] {
        let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":target})).await["scope"].clone();
        // Identical cache name and request URL; only the selected document's storage scope differs.
        let field=json!({"type":"cache_entry","scope":scope,"cache":"same-name","request_url":"https://resource.test/shared"});
        let before=m.call_tool("state",json!({"launch_id":id,"operation":"read","target":target,"field":field})).await;
        assert_eq!(before["value"],json!({"state":"absent"}));
        let applied=m.call_tool("state",json!({"launch_id":id,"operation":"apply","boundary":before["boundary"],"changes":[
            {"target":target,"field":field,"value":{"state":"present","response":{"status":200,"body":text}}}
        ]})).await;
        assert_eq!(applied["completed"],true,"{applied}");
        assert_eq!(applied["boundary"]["t_us"],before["boundary"]["t_us"]);
        fields.push(field);
    }
    assert_ne!(fields[0]["scope"]["storage_key"],fields[1]["scope"]["storage_key"]);
    for (index,(target,text)) in [(&top,"top"),(&child,"child")].into_iter().enumerate() {
        let value=m.call_tool("state",json!({"launch_id":id,"operation":"read","target":target,"field":fields[index]})).await;
        assert_eq!(value["value"]["response"]["body_base64"],frontcap_core::encode::base64(text.as_bytes()));
        let (refused,error)=m.try_tool("state",json!({"launch_id":id,"operation":"read","target":target,"field":fields[1-index]})).await;
        assert!(refused && error["code"]=="invalid_arguments","{error}");
    }
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn fetched_response_metadata_survives_reads_and_guards_native_deletion() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-fetch.test";
    let page=r#"<!doctype html><output id="status">loading</output><script>
(async()=>{
 const response=await fetch('/payload');
 await (await caches.open('fetched')).put('/payload',response);
 document.querySelector('#status').textContent='ready';
})().catch(e=>document.querySelector('#status').textContent=String(e));
</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[
        {"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"payload","match":{"url":format!("{origin}/payload")},"action":{"type":"mock","status":200,"headers":{"content-type":"application/octet-stream","x-source":"fetched"},"body_base64":"AP+A"}}
    ]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"ready"},"max_ms":2000}})).await;
    let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let field=json!({"type":"cache_entry","scope":scope,"cache":"fetched","request_url":format!("{origin}/payload")});
    let before=read(&mut m,id,&field).await;
    let response=&before["value"]["response"];
    assert_eq!(response["response_type"],"basic","{before}");
    assert_eq!(response["urls"],json!([format!("{origin}/payload")]));
    assert_eq!(response["body_base64"],"AP+A");
    assert_eq!(response["headers"]["x-source"],"fetched");
    // Missing native metadata must not silently turn a fetched response into a synthetic one.
    let mut lossy=before["value"].clone();lossy["response"].as_object_mut().unwrap().remove("metadata");
    let (refused,error)=m.try_tool("state",json!({"launch_id":id,"operation":"apply","boundary":before["boundary"],"changes":[
        {"target":{"by":"document"},"field":field,"value":lossy}
    ]})).await;
    assert!(refused,"{error}");
    assert_eq!(read(&mut m,id,&field).await["value"],before["value"]);
    let deleted=apply(&mut m,id,&field,json!({"state":"absent"}),"delete-fetched").await;
    assert_eq!(deleted["completed"],true,"{deleted}");
    assert_eq!(deleted["boundary"]["t_us"],before["boundary"]["t_us"]);
    assert_eq!(read(&mut m,id,&field).await["value"],json!({"state":"absent"}));
    let restored=apply(&mut m,id,&field,before["value"].clone(),"restore-fetched").await;
    assert_eq!(restored["completed"],true,"{restored}");
    assert_eq!(read(&mut m,id,&field).await["value"],before["value"]);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn restored_native_responses_keep_cors_exposure_and_opaque_filtering() {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-types.test";let remote="https://resource-types.test";
    let page=r#"<!doctype html><button id="read">Read</button><output id="status">loading</output><script>
(async()=>{const c=await caches.open('types');
 await c.put('/cors',await fetch('https://resource-types.test/cors',{credentials:'omit'}));
 await c.put('/opaque',await fetch('https://resource-types.test/opaque',{mode:'no-cors',credentials:'omit'}));
 document.querySelector('#status').textContent='ready';})();
 document.querySelector('#read').onclick=async()=>{const c=await caches.open('types');const values={};
 for(const key of ['cors','opaque']){const r=await c.match('/'+key);values[key]={type:r.type,url:r.url,status:r.status,visible:r.headers.get('x-visible'),hidden:r.headers.get('x-hidden'),body:await r.text()};}
 window.values=values;document.querySelector('#status').textContent='read';};</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[
        {"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"cors","match":{"url":format!("{remote}/cors")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/plain","access-control-allow-origin":origin,"access-control-expose-headers":"X-Visible","x-visible":"yes","x-hidden":"hidden"},"body":"cors-body"}},
        {"id":"opaque","match":{"url":format!("{remote}/opaque")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/plain","x-visible":"hidden"},"body":"opaque-body"}}
    ]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"ready"},"max_ms":2000}})).await;
    let scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    for key in ["cors","opaque"] {
        let field=json!({"type":"cache_entry","scope":scope,"cache":"types","request_url":format!("{origin}/{key}")});
        let captured=read(&mut m,id,&field).await;
        let metadata=&captured["value"]["response"]["metadata"];
        assert_eq!(metadata["response_type"],if key=="cors" {"cors"} else {"opaque_response"},"{captured}");
        assert_eq!(metadata["request_include_credentials"],false);
        if key=="cors" {assert!(metadata["cors_exposed_header_names"].as_array().unwrap().iter().any(|v|v.as_str().unwrap().eq_ignore_ascii_case("x-visible")));}
        assert_eq!(apply(&mut m,id,&field,json!({"state":"absent"}),&format!("delete-{key}")).await["completed"],true);
        let restored=apply(&mut m,id,&field,captured["value"].clone(),&format!("restore-{key}")).await;
        assert_eq!(restored["completed"],true,"{restored}");
        assert_eq!(restored["boundary"]["t_us"],captured["boundary"]["t_us"]);
        assert_eq!(read(&mut m,id,&field).await["value"],captured["value"]);
    }
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#read"}},"then":{"type":"none"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"read"},"max_ms":2000}})).await;
    let values=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"values"})).await["value"].clone();
    assert_eq!(values,json!({"cors":{"type":"cors","url":format!("{remote}/cors"),"status":200,"visible":"yes","hidden":null,"body":"cors-body"},"opaque":{"type":"opaque","url":"","status":0,"visible":null,"hidden":null,"body":""}}));
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn empty_initial_caches_exist_before_application_reads_and_replay() {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-empty.test";
    let page=r#"<!doctype html><output id="status">loading</output><script>
(async()=>{const exists=await caches.has('empty');const keys=exists?await(await caches.open('empty')).keys():[];document.querySelector('#status').textContent=exists&&keys.length===0?'empty-present':'wrong';})();</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"profile":{"initial_caches":[{"origin":origin,"name":"empty","entries":[]}]},"rules":[
        {"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}
    ]})).await;
    let id=&launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"empty-present"},"max_ms":2000}})).await;
    let text=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"document.querySelector('#status').textContent"})).await["value"].clone();
    assert_eq!(text,"empty-present");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}
