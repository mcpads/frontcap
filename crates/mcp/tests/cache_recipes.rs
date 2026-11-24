//! Live recipes preserve native default-bucket cache order and response identity before scripts.
mod support;
use serde_json::json;

#[tokio::test]
async fn live_cache_recipe_replaces_stale_seeds_and_restores_order_empty_caches_and_response_urls() {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-recipe.test";
    let page=r#"<!doctype html><button id="seed">Seed</button><output id="summary">loading</output><script>
async function show(){const names=await caches.keys();let keys=[],types=[],urls=[],bodies=[];
 if(names.includes('ordered')){const c=await caches.open('ordered');for(const key of await c.keys()){const r=await c.match(key);keys.push(new URL(key.url).pathname);types.push(r.type);urls.push(r.url);bodies.push(await r.text());}}
 document.querySelector('#summary').textContent=JSON.stringify([names,keys,types,urls,bodies]);}
document.querySelector('#seed').onclick=async()=>{document.querySelector('#summary').textContent='working';await caches.delete('stale');const c=await caches.open('ordered');await c.put('/z',new Response('first'));await c.put('/a',await fetch('/payload'));await caches.open('empty');await show();};show();</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"profile":{"initial_caches":[
        {"origin":origin,"name":"stale","entries":[{"request_url":format!("{origin}/old"),"status":200,"body":"old"}]}
    ]},"rules":[
        {"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"payload","match":{"url":format!("{origin}/payload")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/plain"},"body":"network"}}
    ]})).await;
    let id=&launched["launch_id"];
    let expected=json!([["ordered","empty"],["/z","/a"],["default","basic"],["",format!("{origin}/payload")],["first","network"]]).to_string();
    let predicate=json!({"is":"text_equals","target":{"by":"css","selector":"#summary"},"text":expected});
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#seed"}}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":predicate,"max_ms":2000}})).await;
    let source_scope=m.call_tool("state",json!({"launch_id":id,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let source=m.call_tool("state",json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":{"type":"cache_entry","scope":source_scope,"cache":"ordered","request_url":format!("{origin}/a")}})).await;
    let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":id,"verify":[{"name":"cache-state","predicate":predicate}]})).await;
    assert_eq!(saved["origin"]["boundary"]["t_us"],source["boundary"]["t_us"]);
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],source["boundary"]["t_us"]);
    let described=m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let caches=described["initial_state"]["caches"].as_array().unwrap();
    assert_eq!(caches.iter().map(|c|(&c["name"],&c["entries"])).collect::<Vec<_>>(),vec![(&json!("ordered"),&json!(2)),(&json!("empty"),&json!(0))]);
    for n in 0..2 {
        let child=m.call_tool("state",json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"],"request_id":format!("cache-child-{n}")})).await;
        assert_eq!(child["phase"],"ready","{child}");
        assert_eq!(child["verification"],json!([{"name":"cache-state","value":"true"}]),"{child}");
        let c=&child["launch_id"];
        let scope=m.call_tool("state",json!({"launch_id":c,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
        let restored=m.call_tool("state",json!({"launch_id":c,"operation":"read","target":{"by":"document"},"field":{"type":"cache_entry","scope":scope,"cache":"ordered","request_url":format!("{origin}/a")}})).await;
        assert_eq!(restored["value"],source["value"]);
        m.call_tool("stop",json!({"launch_id":c})).await;
        let replay=m.call_tool("debug",json!({"launch_id":c,"operation":"replay"})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn live_cache_recipe_names_whole_caches_it_cannot_restore() {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://cache-excluded.test";
    let page=r#"<!doctype html><button id="seed">Seed</button><output id="status">idle</output><script>
document.querySelector('#seed').onclick=async()=>{
 await(await caches.open('odd|name')).put('/ok',new Response('bad-name'));
 await(await caches.open('fragmented')).put('/item#piece',new Response('fragment'));
 await(await caches.open('good')).put('/item',new Response('good'));
 document.querySelector('#status').textContent='ready';};</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]})).await;
    let id=&launched["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#seed"}}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"ready"},"max_ms":2000}})).await;
    let before=m.call_tool("status",json!({"launch_id":id})).await["t_us"].clone();
    let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":id})).await;
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],before);
    for name in ["odd|name","fragmented"] {
        assert!(saved["excluded"].as_array().unwrap().iter().any(|s|s.as_str().unwrap().contains(&format!("Cache Storage cache {name}:"))),"{saved}");
    }
    let described=m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let caches=described["initial_state"]["caches"].as_array().unwrap();assert_eq!(caches.len(),1);assert_eq!(caches[0]["name"],"good");assert_eq!(caches[0]["entries"],1);
    m.call_tool("stop",json!({"launch_id":id})).await;
}
