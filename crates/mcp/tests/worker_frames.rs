//! Worker messages request page frames at the same instants in replay branches.
mod support;

use serde_json::{Value, json};

async fn events(m: &mut support::Mcp, id: &Value) -> Value {
    let seen = m.call_tool("observe", json!({"launch_id":id,
        "op":{"type":"query","target":{"by":"css","selector":"#events"},"props":["text"]}})).await;
    serde_json::from_str(seen["values"]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn worker_frame_timing_survives_replay_and_continued_progress() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><output id=events>[]</output><script>
const events=[];
const child=new Worker('/worker.js');
child.onmessage=e=>requestAnimationFrame(()=>{
  events.push([e.data,Date.now()-1700000000000]);
  document.querySelector('#events').textContent=JSON.stringify(events);
});
</script>"#;
    let worker = "setTimeout(()=>postMessage('a'),100);setTimeout(()=>postMessage('b'),437);";
    let rule = |id: &str, url: &str, mime: &str, body: &str| json!({"id":id,"match":{"url":url},
        "action":{"type":"mock","status":200,"headers":{"content-type":mime},"body":body}});
    let launched = m.call_tool("launch", json!({"url":"https://app.example.test/","then":{"type":"none"},
        "profile":{"initial_date_ms":1700000000000u64},"passthrough_origins":[],
        "rules":[rule("page","https://app.example.test/","text/html",page),
                 rule("worker","https://app.example.test/worker.js","text/javascript",worker)]})).await;
    let id=launched["launch_id"].clone();
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":200}})).await;
    assert_eq!(events(&mut m,&id).await,json!([["a",101]]));
    let saved=m.call_tool("state",json!({"operation":"save","kind":"replay","from":id})).await;
    let branch=m.call_tool("state",json!({"operation":"branch","kind":"replay","saved_id":saved["saved_id"]})).await;
    for current in [&id,&branch["launch_id"]] {
        assert_eq!(events(&mut m,current).await,json!([["a",101]]));
        m.call_tool("wait",json!({"launch_id":current,"op":{"type":"advance","by_ms":800}})).await;
        assert_eq!(events(&mut m,current).await,json!([["a",101],["b",451]]));
        assert_eq!(m.call_tool("status",json!({"launch_id":current})).await["t_us"],1_000_000);
        m.call_tool("stop",json!({"launch_id":current})).await;
        let replay=m.call_tool("debug",json!({"operation":"replay","launch_id":current})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
}
