//! The settle hint includes worker timers and survives recorder replacement.
mod support;

use serde_json::json;

#[tokio::test]
async fn worker_timer_hints_survive_recorder_replacement_and_cancellation() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><button id=cancel onclick="worker.postMessage('cancel')">Cancel</button>
<button id=schedule onclick="worker.postMessage('schedule')">Schedule</button>
<output id=status></output><script>
const worker=new Worker('/worker.js');
worker.onmessage=e=>document.querySelector('#status').textContent=e.data;
</script>"#;
    let worker = r#"const nested=new Worker('/nested.js');
let timer=setTimeout(()=>postMessage('saved'),60000);
onmessage=e=>{
  clearTimeout(timer);nested.terminate();
  if(e.data==='schedule')timer=setTimeout(()=>postMessage('saved'),30000);
  postMessage(e.data);
};"#;
    let rule = |id: &str, url: &str, mime: &str, body: &str| json!({"id":id,"match":{"url":url},
        "action":{"type":"mock","status":200,"headers":{"content-type":mime},"body":body}});
    let launched = m.call_tool("launch", json!({"url":"https://app.example.test/","passthrough_origins":[],
        "rules":[rule("page","https://app.example.test/","text/html",page),
                 rule("worker","https://app.example.test/worker.js","text/javascript",worker),
                 rule("nested","https://app.example.test/nested.js","text/javascript","setTimeout(()=>postMessage('nested'),45000)")]})).await;
    let id = launched["launch_id"].clone();
    assert_eq!(launched["settle"]["next_timer_t"], 45_000_000, "{launched}");
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let settled = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"settle","idle_window_ms":250,"max_ms":2000}})).await;
    assert_eq!(settled["outcome"]["next_timer_t"], 45_000_000, "{settled}");
    let cancelled = m.call_tool("act", json!({"launch_id":id,
        "action":{"type":"click","target":{"by":"css","selector":"#cancel"}}})).await;
    assert!(cancelled["settle"].get("next_timer_t").is_none(), "{cancelled}");
    let scheduled = m.call_tool("act", json!({"launch_id":id,
        "action":{"type":"click","target":{"by":"css","selector":"#schedule"}}})).await;
    let due = scheduled["t_before_us"].as_u64().unwrap() + 30_000_000;
    assert_eq!(scheduled["settle"]["next_timer_t"], due, "{scheduled}");
    let now = scheduled["t_after_us"].as_u64().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":(due-now).div_ceil(1000)}})).await;
    let settled = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"settle","idle_window_ms":250,"max_ms":2000}})).await;
    assert!(settled["outcome"].get("next_timer_t").is_none(), "{settled}");
    let status = m.call_tool("observe", json!({"launch_id":id,
        "op":{"type":"query","target":{"by":"css","selector":"#status"},"props":["text"]}})).await;
    assert_eq!(status["values"]["text"], "saved");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
