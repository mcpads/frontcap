mod support;
use serde_json::{Value, json};

async fn collect(m: &mut support::Mcp, id: &str, mut page: Value) -> Value {
    let mut text = String::new();
    loop {
        assert!(page.to_string().len() <= 16 * 1024, "bounded structured response");
        assert_eq!(page["offset"].as_u64().unwrap() as usize, text.len());
        text.push_str(page["json_fragment"].as_str().unwrap());
        let Some(offset) = page["next_offset"].as_u64() else { break };
        page = m.call_tool("debug", json!({"launch_id":id,"operation":"result","result_id":page["result_id"],"pointer":page["pointer"],"offset":offset})).await;
    }
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn large_results_survive_changes_stop_and_controller_restart_without_rerunning() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/large-results.html")})).await;
    let id = launch["launch_id"].as_str().unwrap();
    let query = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#large"},"props":["text"]}})).await;
    assert!(query["omitted_bytes"].as_u64().unwrap() > 0);
    let console = m.call_tool("debug", json!({"launch_id":id,"operation":"console","filter":{"level":"log"}})).await;
    let network = m.call_tool("debug", json!({"launch_id":id,"operation":"requests","filter":{"url_contains":"large.example.test"}})).await;
    let args = json!({"launch_id":id,"operation":"evaluate","request_id":"large-eval","expression":"window.calls=(window.calls||0)+1; document.querySelector('#large').textContent='changed'; console.log('later'); ({calls:window.calls,items:Array.from({length:6000},(_,i)=>i)})"});
    let evaluated = m.call_tool("debug", args.clone()).await;
    assert_eq!(m.call_tool("debug", args).await, evaluated, "retry must retain the same result without execution");
    let selected = m.call_tool("debug", json!({"launch_id":id,"operation":"result","result_id":evaluated["result_id"],"pointer":"/value/calls"})).await;
    assert_eq!(selected["json_fragment"], "1");
    let recovered = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":"large-eval"})).await;
    assert!(recovered["result_id"].is_string(), "journal readback must also remain bounded");
    assert!(collect(&mut m,id,recovered).await["result"].is_object());
    let (failed, error) = m.try_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"throw new Error('error-detail-'.repeat(3000))"})).await;
    assert!(failed, "paging an error must preserve MCP isError");
    assert_eq!(error["code"],"script_failed");
    assert!(error.to_string().len() <= 16 * 1024);
    let detail = collect(&mut m,id,error).await;
    assert_eq!(detail["code"],"script_failed");
    assert!(detail["message"].as_str().unwrap().contains(&"error-detail-".repeat(3000)));
    let before = m.call_tool("status", json!({"launch_id":id})).await;
    let original = collect(&mut m, id, query.clone()).await;
    assert_eq!(original["values"]["text"], "한글🙂".repeat(6000));
    let logs = collect(&mut m, id, console).await;
    assert_eq!(logs["entries"].as_array().unwrap().len(),24);
    for (i, entry) in logs["entries"].as_array().unwrap().iter().enumerate() {
        assert_eq!(entry["text"],format!("entry-{i}:{}","x".repeat(1000)));
    }
    let requests = collect(&mut m, id, network).await;
    assert_eq!(requests["entries"].as_array().unwrap().len(),24);
    let unique:std::collections::BTreeSet<_> = requests["entries"].as_array().unwrap().iter().map(|e|e["request"].as_str().unwrap()).collect();
    assert_eq!(unique.len(),24);
    let after = m.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(before["t_us"],after["t_us"]);
    assert_eq!(before["observation_seq"],after["observation_seq"]);
    m.call_tool("stop", json!({"launch_id":id})).await;
    m.kill().await;
    let mut reader = support::Mcp::start_in(&ws.root()).await;
    let value = collect(&mut reader,id,evaluated).await;
    assert_eq!(value,json!({"value":{"calls":1,"items":(0..6000).collect::<Vec<_>>()}}));
    assert_eq!(collect(&mut reader,id,query).await, original);
}
