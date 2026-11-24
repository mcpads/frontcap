//! Public-input replay keeps a worker's synchronous atomic wait at the same instant.
mod support;

use serde_json::{Value, json};

async fn status(m: &mut support::Mcp, id: &Value) -> Value {
    m.call_tool("observe", json!({"launch_id":id,
        "op":{"type":"query","target":{"by":"css","selector":"#status"},"props":["text"]}})).await["values"]["text"].clone()
}

async fn waiting_page(m: &mut support::Mcp) -> Value {
    let page = r#"<!doctype html>
<button id=wait onclick="worker.postMessage(buffer)">Wait</button>
<button id=wake onclick="Atomics.notify(words,0)">Wake</button>
<output id=status>starting</output><script>
const buffer=new SharedArrayBuffer(4),words=new Int32Array(buffer);
const worker=new Worker('/worker.js');
worker.onmessage=e=>document.querySelector('#status').textContent=e.data;
</script>"#;
    let worker = "onmessage=e=>{postMessage('waiting');const result=Atomics.wait(new Int32Array(e.data),0,0);postMessage(result);};postMessage('ready');";
    let rule = |id: &str, url: &str, mime: &str, body: &str| json!({"id":id,"match":{"url":url},
        "action":{"type":"mock","status":200,"headers":{"content-type":mime,
        "cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":body}});
    let launched = m.call_tool("launch", json!({"url":"https://app.example.test/","passthrough_origins":[],
        "rules":[rule("page","https://app.example.test/","text/html",page),
                 rule("worker","https://app.example.test/worker.js","text/javascript",worker)]})).await;
    let id=launched["launch_id"].clone();
    assert_eq!(status(m,&id).await,"ready");
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#wait"}}})).await;
    assert_eq!(status(m,&id).await,"waiting");
    id
}

async fn release_after_an_hour(m: &mut support::Mcp, id: &Value) {
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":3_600_000}})).await;
    assert_eq!(status(m,id).await,"waiting");
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#wake"}}})).await;
    assert_eq!(status(m,id).await,"ok");
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn an_atomic_wait_survives_a_replay_branch_and_continued_progress() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = waiting_page(&mut m).await;
    let saved=m.call_tool("state",json!({"operation":"save","kind":"replay","from":id})).await;
    let branch=m.call_tool("state",json!({"operation":"branch","kind":"replay","saved_id":saved["saved_id"]})).await;
    for current in [&id,&branch["launch_id"]] {
        release_after_an_hour(&mut m,current).await;
        let replay=m.call_tool("debug",json!({"operation":"replay","launch_id":current})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
}

#[tokio::test]
async fn an_atomic_wait_survives_live_recorder_replacement() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = waiting_page(&mut m).await;
    let runtime=ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(status(&mut m,&id).await,"waiting");
    release_after_an_hour(&mut m,&id).await;
    // A forced recorder loss deliberately records recorder_fault. Live recovery
    // is supported, but that history cannot be certified for replay. The clean
    // replay test above exercises replay without erasing this coverage boundary.
}
