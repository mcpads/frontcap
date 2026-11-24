mod support;
use serde_json::json;

#[tokio::test]
async fn evaluation_uses_the_frames_current_main_world_and_retains_completed_results() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([
        {"id":"top","match":{"url":"https://top.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<script>let identity='top';</script><iframe id=child src='https://child.test/'></iframe>"}},
        {"id":"child","match":{"url":"https://child.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","content-security-policy":"script-src 'nonce-frontcap'"},
         "body":"<script nonce=frontcap>let identity='child';let count=0;</script><p>Child</p>"}},
        {"id":"next","match":{"url":"https://child.test/next"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<script>let identity='next';let count=10;</script><p>Next</p>"}}
    ]);
    let launch = m.call_tool("launch", json!({"url":"https://top.test/","rules":rules})).await;
    let id = &launch["launch_id"];
    let frame = json!({"by":"document","selector":"#child"});
    let eval = |expression: &str| json!({"launch_id":id,"operation":"evaluate","frame":frame,"expression":expression});
    let read = m.call_tool("debug", eval("Promise.resolve([identity,location.origin,performance.now()])")).await;
    let t = read["value"][2].clone();
    assert_eq!(read["value"], json!(["child","https://child.test",t]), "{read}");
    let top = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"identity"})).await;
    assert_eq!(top["value"], "top", "{top}");
    m.call_tool("debug", eval("let evaluatedGlobal='kept'; evaluatedGlobal")).await;
    assert_eq!(m.call_tool("debug", eval("evaluatedGlobal")).await["value"], "kept");
    let (failed, error) = m.try_tool("debug", eval("new Promise(done => setTimeout(done, 100))")).await;
    assert!(failed && error["message"].as_str().unwrap().contains("did not settle at this instant"), "{error}");
    assert_eq!(m.call_tool("debug", eval("performance.now()")).await["value"], t);
    let mut change = eval("++count");
    change["request_id"] = json!("increment-child");
    let changed = m.call_tool("debug", change.clone()).await;
    assert_eq!(changed["value"], 1, "{changed}");
    let runtime = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(m.call_tool("debug", change.clone()).await, changed);
    assert_eq!(m.call_tool("debug", eval("count")).await["value"], 1);
    assert_eq!(m.call_tool("debug", eval("performance.now()")).await["value"], t);
    let (failed, error) = m.try_tool("debug", eval("throw new Error('child exception')")).await;
    assert!(failed && error["message"].as_str().unwrap().contains("child exception"), "{error}");
    m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"document.querySelector('#child').src='https://child.test/next';true"})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
    let next = m.call_tool("debug", eval("[identity,count]")).await;
    assert_eq!(next["value"], json!(["next",10]), "{next}");
    assert_eq!(m.call_tool("debug", change).await, changed, "retry never evaluates in a replacement document");
    let (failed, error) = m.try_tool("debug", json!({"launch_id":id,"operation":"evaluate","frame":{"by":"document"},"expression":"true"})).await;
    assert!(failed && error["code"]=="not_found", "{error}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
