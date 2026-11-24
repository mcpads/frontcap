//! Long quiet advances still reach the page's real deadline exactly.
mod support;
use serde_json::json;

#[tokio::test]
async fn a_quiet_month_reaches_the_expiry_boundary() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    // JavaScript timers clamp delays beyond signed 32-bit milliseconds. Re-arm
    // after 20 days so this exercises a real month rather than the clamp.
    let page = r#"<!doctype html><output id=status>active</output><script>
setTimeout(()=>setTimeout(()=>document.querySelector('#status').textContent='expired',10*86400000),20*86400000);
</script>"#;
    let launched = m.call_tool("launch", json!({"url":"https://idle.example.test/","passthrough_origins":[],
        "rules":[{"id":"page","match":{"url":"https://idle.example.test/"},"action":{"type":"mock",
            "status":200,"headers":{"content-type":"text/html"},"body":page}}]})).await;
    let id = launched["launch_id"].clone();
    assert_eq!(launched["settle"]["next_timer_t"], 20_u64 * 86_400_000_000, "{launched}");
    let due = 30_u64 * 86_400_000_000;
    let now = launched["settle"]["t"].as_u64().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":(due-now)/1000-1}})).await;
    let active = m.call_tool("observe", json!({"launch_id":id,
        "op":{"type":"query","target":{"by":"css","selector":"#status"},"props":["text"]}})).await;
    assert_eq!(active["values"]["text"], "active");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    let expired = m.call_tool("observe", json!({"launch_id":id,
        "op":{"type":"query","target":{"by":"css","selector":"#status"},"props":["text"]}})).await;
    assert_eq!(expired["values"]["text"], "expired");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
