mod support;
use serde_json::{Value, json};

fn app_rule(origin: &str) -> Value {
    let body = r#"<!doctype html><button id="change">Change</button><output id="status">loading</output><script>
const key=()=>new Request('/seed',{headers:{'accept-language':'ko'}});
async function readBoot(){const start=Date.now();const c=await caches.open('boot');const r=await c.match(key());const b=await c.match('/binary');const all=await c.matchAll('/binary');const keys=await c.keys();const present=await caches.has('boot');await caches.open('scratch');const deletedCache=await caches.delete('scratch');const names=await caches.keys();const deletedMissing=await c.delete('/missing');let rejected=false;try{await new Response('{').json()}catch(e){rejected=e instanceof SyntaxError}globalThis.boot={cloneText:r?await r.clone().text():'missing',text:r?await r.text():'missing',status:r?.status??null,bytes:b?[...new Uint8Array(await b.arrayBuffer())]:[],matches:all.length,keys:keys.length,present,deletedCache,names,deletedMissing,rejected,serviceElapsedMs:Date.now()-start};document.querySelector('#status').textContent='ready';}
document.querySelector('#change').onclick=async()=>{try{const c=await caches.open('boot');let rejected=false;try{await c.put('/broken',new Response(new ReadableStream({start(controller){setTimeout(()=>controller.error(new Error('producer failed')),5)}})))}catch(e){rejected=true}globalThis.failedPut=rejected&&!(await c.match('/broken'));const start=Date.now();const stream=new ReadableStream({start(controller){setTimeout(()=>{globalThis.timerElapsedMs=Date.now()-start;controller.enqueue(new TextEncoder().encode('changed'));controller.close()},25)}});await c.put(key(),new Response(stream,{status:202}));globalThis.changed=Date.now()-start;document.querySelector('#status').textContent='changed'}catch(e){document.querySelector('#status').textContent=String(e)}};
readBoot().catch(e=>document.querySelector('#status').textContent=String(e));
</script>"#;
    json!({"id":"app","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}})
}
fn expected_boot(text: &str, status: Value, seeded: bool) -> Value {
    json!({"cloneText":text,"text":text,"status":status,"bytes":if seeded{json!([0,1,2,255])}else{json!([])},
        "matches":if seeded{1}else{0},"keys":if seeded{2}else{0},"present":true,
        "deletedCache":true,"names":["boot"],"deletedMissing":false,"rejected":true,"serviceElapsedMs":0})
}
fn cache(origin: &str) -> Value {
    json!({"origin":origin,"name":"boot","entries":[
        {"request_url":format!("{origin}/seed"),"request_headers":{"accept-language":"ko"},"status":201,"status_text":"Created","headers":{"content-type":"text/plain","vary":"accept-language"},"body":"seed"},
        {"request_url":format!("{origin}/binary"),"status":200,"body_base64":"AAEC/w=="}
    ]})
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
    let result = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":text},"max_ms":2000}})).await;
    let status = evaluate(m, id, "document.querySelector('#status').textContent").await;
    assert_eq!(status, json!(text), "{result}");
}

#[tokio::test]
async fn cache_initialization_precedes_scripts_and_survives_reload_isolation_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://cache.example.test";
    let launched = m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[app_rule(origin)],"then":{"type":"none"},"profile":{"initial_caches":[cache(origin)]}})).await;
    let id = &launched["launch_id"];
    assert_eq!(
        evaluate(&mut m, id, "typeof boot").await,
        json!("undefined")
    );
    ready(&mut m, id, "ready").await;
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected_boot("seed", json!(201), true)
    );
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "changed").await;
    assert_eq!(
        evaluate(&mut m, id, "({timerElapsedMs,changed})").await,
        json!({"timerElapsedMs":25,"changed":25})
    );
    assert_eq!(evaluate(&mut m, id, "failedPut").await, json!(true));
    m.call_tool(
        "act",
        json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}}),
    )
    .await;
    ready(&mut m, id, "ready").await;
    assert_eq!(
        evaluate(&mut m, id, "boot").await,
        expected_boot("changed", json!(202), true)
    );
    let clean = m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[app_rule(origin)],"then":{"type":"none"}})).await;
    ready(&mut m, &clean["launch_id"], "ready").await;
    assert_eq!(
        evaluate(&mut m, &clean["launch_id"], "boot").await,
        expected_boot("missing", Value::Null, false)
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
    assert_eq!(
        events
            .iter()
            .filter(|e| e["kind"] == "cache_initialized")
            .count(),
        1
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
async fn cache_invalid_declarations_are_rejected_before_launch() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    assert!(m.definition("state", "recipe", "InitialCache").await["properties"]["entries"].is_object());
    let origin = "https://cache.example.test";
    for (pointer, value) in [
        ("/name", json!("bad|name")),
        ("/origin", json!("http://insecure.example.test")),
        ("/entries/0/status", json!(206)),
        ("/entries/0/headers/vary", json!("*")),
        (
            "/entries/0/request_url",
            json!("https://user@cache.example.test/seed"),
        ),
        (
            "/entries/0/request_url",
            json!("https://cache.example.test/a/../seed"),
        ),
        ("/entries/1/body_base64", json!("not base64")),
    ] {
        let mut invalid = cache(origin);
        *invalid.pointer_mut(pointer).unwrap() = value;
        let (failed, error) = m
            .try_tool(
                "launch",
                json!({"url":format!("{origin}/start"),"profile":{"initial_caches":[invalid]}}),
            )
            .await;
        assert!(
            failed && error["code"] == "invalid_arguments",
            "{pointer}: {error}"
        );
    }
}

#[tokio::test]
async fn removing_storage_owners_cancels_pending_puts_and_preserves_parent_progress() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://cache-owners.example.test";
    let parent = r#"<!doctype html><button id="discard">Discard owners</button><output id="status">loading</output><iframe src="/child"></iframe><script>
const owners=new Set();const worker=new Worker('/worker');
function started(owner){owners.add(owner);if(owners.size===2)document.querySelector('#status').textContent='pending'}
addEventListener('message',e=>{if(e.data==='child-started')started('child')});
worker.onmessage=e=>{if(e.data==='worker-started')started('worker')};
document.querySelector('#discard').onclick=async()=>{try{document.querySelector('iframe').remove();worker.terminate();const c=await caches.open('survivor');await c.put('/value',new Response('parent'));globalThis.value=await(await c.match('/value')).text();setTimeout(()=>document.querySelector('#status').textContent='recovered',10)}catch(e){document.querySelector('#status').textContent=String(e)}};
setTimeout(async()=>{const c=await caches.open('interrupted');globalThis.remaining=(await c.keys()).map(r=>new URL(r.url).pathname);document.querySelector('#status').textContent='checked'},1500);
</script>"#;
    let owner = r#"async function begin(owner,report){const c=await caches.open('interrupted');const stream=new ReadableStream({start(controller){setTimeout(()=>{controller.enqueue(new TextEncoder().encode(owner));controller.close()},1000)}});const pending=c.put('/'+owner,new Response(stream));report(owner+'-started');await pending;report(owner+'-completed')};"#;
    let child = format!(
        "<!doctype html><script>{owner}begin('child',value=>parent.postMessage(value,'*'));</script>"
    );
    let worker = format!("{owner}begin('worker',value=>postMessage(value));");
    let rules = json!([
        {"id":"parent","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":parent}},
        {"id":"child","match":{"url":format!("{origin}/child")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child}},
        {"id":"worker","match":{"url":format!("{origin}/worker")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":worker}}
    ]);
    let launched = m
        .call_tool(
            "launch",
            json!({"url":format!("{origin}/start"),"rules":rules,"then":{"type":"none"}}),
        )
        .await;
    let id = &launched["launch_id"];
    ready(&mut m, id, "pending").await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#discard"}},"then":{"type":"none"}})).await;
    ready(&mut m, id, "recovered").await;
    assert_eq!(evaluate(&mut m, id, "value").await, json!("parent"));
    ready(&mut m, id, "checked").await;
    assert_eq!(evaluate(&mut m, id, "remaining").await, json!([]));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m
        .call_tool(
            "debug",
            json!({"launch_id":id,"operation":"replay","url":"https://moved.example.test/start"}),
        )
        .await;
    assert_eq!(replay["equal"], true, "{replay}");
}
