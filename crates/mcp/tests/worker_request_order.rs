//! Same-instant requests of the page and its workers through the public MCP surface: a sequence rule
//! answers them in the declared issuer order whichever thread issued first, a request one answer causes
//! follows the whole round, and the order holds across recorder replacement and in replay.
mod support;

use serde_json::{Value, json};

/// Two workers from one script upload to one URL; `two` issues first, then `one`, then the page. The
/// first worker's answer makes the page confirm at once.
const PAGE: &str = r#"<!doctype html><button id=sync>Sync</button><output id=out></output><script>
const got={};const show=()=>document.querySelector('#out').textContent=['page','one','two','confirm'].map(n=>n+'='+(got[n]??'-')).join(' ');
const send=name=>fetch('/upload',{method:'POST',body:name}).then(r=>r.text()).then(b=>{got[name]=b;show();});
const workers={};
for(const name of ['one','two']){
  const w=new Worker('/upload-worker.js',{name});
  w.onmessage=e=>{
    if(e.data.issued==='two')workers.one.postMessage('go');
    else if(e.data.issued==='one')send('page');
    else{got[e.data.name]=e.data.body;show();if(e.data.name==='one')send('confirm');}
  };
  workers[name]=w;
}
document.querySelector('#sync').onclick=()=>{for(const n in got)delete got[n];workers.two.postMessage('go');};
</script>"#;

const WORKER: &str = "onmessage=()=>{fetch('/upload',{method:'POST',body:name}).then(r=>r.text()).then(body=>postMessage({name,body}));postMessage({issued:name});};";

async fn sync(m: &mut support::Mcp, id: &Value) -> Value {
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#sync"}}})).await;
    m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#out"},"props":["text"]}})).await["values"]["text"].clone()
}

async fn launch(m: &mut support::Mcp) -> Value {
    let steps: Vec<_> = (1..=8).map(|n| json!({"status":200,"body":n.to_string()})).collect();
    let rule = |id: &str, url: &str, mime: &str, body: &str| json!({"id":id,"match":{"url":url},
        "action":{"type":"mock","status":200,"headers":{"content-type":mime},"body":body}});
    let launched = m.call_tool("launch", json!({"url":"https://upload.example.test/","passthrough_origins":[],
        "rules":[rule("page","https://upload.example.test/","text/html",PAGE),
                 rule("worker","https://upload.example.test/upload-worker.js","text/javascript",WORKER),
                 {"id":"upload","match":{"url":"https://upload.example.test/upload"},"action":{"type":"sequence","after_last":"fail","steps":steps}}]})).await;
    launched["launch_id"].clone()
}

#[tokio::test]
async fn same_instant_uploads_take_the_issuer_order_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m).await;
    assert_eq!(sync(&mut m, &id).await, "page=1 one=2 two=3 confirm=4");
    assert_eq!(sync(&mut m, &id).await, "page=5 one=6 two=7 confirm=8");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn same_instant_uploads_keep_the_issuer_order_across_recorder_replacement() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m).await;
    assert_eq!(sync(&mut m, &id).await, "page=1 one=2 two=3 confirm=4");
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(sync(&mut m, &id).await, "page=5 one=6 two=7 confirm=8");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
