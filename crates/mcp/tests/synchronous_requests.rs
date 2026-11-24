//! Requests the page's main thread waits for (a synchronous XHR), through the public MCP surface: answered
//! at their rule's instant while the page cannot run, a held one suspending the operation until the agent
//! answers it, and both replayed from the recording.
mod support;
use serde_json::{Value, json};
use std::time::Duration;

fn mock(latency_ms: u64) -> Value {
    json!([{"id":"one","match":{"url":"https://api.example.test/one"},"action":{"type":"mock","status":200,"body":"mocked","latency_ms":latency_ms}}])
}

fn held() -> Value {
    json!([{"id":"one","match":{"url":"https://api.example.test/one"},"action":{"type":"hold"}}])
}

fn click(id: &Value, request: &str) -> Value {
    json!({"launch_id":id,"request_id":request,"action":{"type":"click","target":{"by":"css","selector":"#one"}},"then":{"type":"none"}})
}

async fn page_log(m: &mut support::Mcp, id: &Value) -> Vec<Value> {
    let v = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"JSON.stringify(window.__log)"})).await;
    serde_json::from_str(v["value"].as_str().unwrap()).unwrap()
}

/// The parent's final answer once it no longer runs.
async fn terminal(m: &mut support::Mcp, id: &Value, parent: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":parent})).await;
            if result["result"]["state"] == "completed" {
                let status = m.call_tool("status", json!({"launch_id":id})).await;
                if status["running"].is_null() { return result["result"]["answer"].clone(); }
            }
            tokio::task::yield_now().await;
        }
    }).await.expect("the parent never finished")
}

async fn launch(m: &mut support::Mcp, addr: std::net::SocketAddr, rules: Value) -> Value {
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/sync-xhr.html"),"rules":rules})).await;
    launched["launch_id"].clone()
}

async fn replays_equal(m: &mut support::Mcp, id: &Value) {
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn a_click_waits_for_the_rule_latency_and_replays() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m, addr, mock(300)).await;
    let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().unwrap();
    let acted = m.call_tool("act", click(&id, "c-1")).await;
    assert!(acted["effects"]["requests"].to_string().contains("api.example.test/one"), "{acted}");
    let after = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().unwrap();
    assert_eq!(after - before, 300_000, "the click took the request's latency");
    let log = page_log(&mut m, &id).await;
    assert_eq!((log[0][3].as_str(), log[0][5].as_f64().unwrap() - log[0][4].as_f64().unwrap()), (Some("mocked"), 300.0), "{log:?}");
    replays_equal(&mut m, &id).await;
}

#[tokio::test]
async fn a_held_request_suspends_the_click_until_the_agent_answers() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m, addr, held()).await;
    let suspended = m.call_tool("act", click(&id, "c-1")).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    assert_eq!(suspended["operation_id"], "c-1");
    assert_eq!(suspended["clock"]["state"], "blocked");
    let request = suspended["request"].clone();
    assert!(request["url"].as_str().unwrap().ends_with("/one"), "{suspended}");
    let status = m.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(status["suspension"]["request"]["ordinal"], request["ordinal"], "{status}");
    // Another request is refused while the page waits; the answer is the operation's dependent.
    let (busy, error) = m.try_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":10}})).await;
    assert!(busy);
    assert_eq!(error["code"], "busy", "{error}");
    let answered = m.call_tool("network", json!({"launch_id":id,"operation":"respond","request_id":"answer-1","ordinal":request["ordinal"],
        "response":{"status":200,"body":"answered by the agent"}})).await;
    assert_eq!(answered["ok"], true, "{answered}");
    assert_eq!(answered["parent"]["state"], "completed", "the answer reports the click finishing: {answered}");
    let result = terminal(&mut m, &id, &json!("c-1")).await;
    assert!(result.to_string().contains("acted") || result.to_string().contains("ok"), "{result}");
    let log = page_log(&mut m, &id).await;
    assert_eq!(log[0][3], "answered by the agent", "{log:?}");
    assert_eq!(log[0][4], log[0][5], "no time passed while the agent decided");
    replays_equal(&mut m, &id).await;
}

#[tokio::test]
async fn a_held_request_can_be_failed() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m, addr, held()).await;
    let suspended = m.call_tool("act", click(&id, "c-1")).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    m.call_tool("network", json!({"launch_id":id,"operation":"fail","request_id":"answer-1","ordinal":suspended["request"]["ordinal"],"reason":"connection_refused"})).await;
    terminal(&mut m, &id, &json!("c-1")).await;
    let log = page_log(&mut m, &id).await;
    assert_eq!(log[0][0], "error", "{log:?}");
    replays_equal(&mut m, &id).await;
}

#[tokio::test]
async fn recorder_replacement_records_cleanup_of_a_held_synchronous_request() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m, addr, held()).await;
    let suspended = m.call_tool("act", click(&id, "interrupted-click")).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let old = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    old.recorder.kill().unwrap();
    let attached = m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert!(attached["status"]["suspension"].is_null(), "lease-loss cleanup finishes the blocker: {attached}");
    let recovered = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    assert_eq!(recovered.browser, old.browser, "the living browser must be preserved");
    terminal(&mut m, &id, &json!("interrupted-click")).await;
    let log = page_log(&mut m, &id).await;
    assert_eq!(log.len(), 1, "the click must not run twice: {log:?}");
    assert_eq!(log[0][0], "error", "lease loss aborts the held request: {log:?}");
    assert_eq!(log[0][3], log[0][4], "recovery must not move the virtual clock");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn releasing_the_lease_fails_the_held_request_and_stop_finalizes() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = launch(&mut m, addr, held()).await;
    let suspended = m.call_tool("act", click(&id, "c-1")).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    // Without a controller the recorder fails the held request itself, so the click can finish.
    m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = m.call_tool("status", json!({"launch_id":id})).await;
            if status["running"].is_null() && status["input_cleanup_pending"] == false { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("cleanup never finished the click");
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let log = page_log(&mut m, &id).await;
    assert_eq!(log[0][0], "error", "{log:?}");
    replays_equal(&mut m, &id).await;

    // A held request still waiting when the generation stops does not keep it alive.
    let id = launch(&mut m, addr, held()).await;
    let suspended = m.call_tool("act", click(&id, "c-2")).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    assert_eq!(stopped["finalized"], true, "{stopped}");
}

#[tokio::test]
async fn mixed_request_sequence_and_deadlines_replay_with_and_without_preflight() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    for selector in ["#plain", "#preflight"] {
        let rules = json!([{"id":"sequence","match":{"url":"https://api.example.test/sequence"},"action":{
            "type":"sequence","after_last":"fail","steps":[
                {"status":200,"body":"first","latency_ms":500},
                {"status":200,"body":"second","latency_ms":200},
                {"status":200,"body":"third"}]}}]);
        let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/sync-sequence.html"),"rules":rules})).await;
        let id = launched["launch_id"].clone();
        let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().unwrap();
        m.call_tool("act", json!({"launch_id":id,"request_id":"mixed","action":{"type":"click","target":{"by":"css","selector":selector}},"then":{"type":"none"}})).await;
        let after = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().unwrap();
        assert_eq!(after - before, 200_000, "{selector}: only the synchronous response moves the clock");
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":300}})).await;
        let log = page_log(&mut m, &id).await;
        assert_eq!(log.len(), 3, "{selector}: {log:?}");
        assert_eq!(log.iter().map(|e| (e[0].as_str().unwrap(), e[1].as_str().unwrap())).collect::<Vec<_>>(),
            vec![("sync", "second"), ("after", "third"), ("async", "first")], "{selector}");
        assert_eq!(log[1][2], log[0][2], "the third response has no latency");
        assert_eq!(log[2][2].as_u64().unwrap() - log[0][2].as_u64().unwrap(), 300);
        replays_equal(&mut m, &id).await;
    }
}
