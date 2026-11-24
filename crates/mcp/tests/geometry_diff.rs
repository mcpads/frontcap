mod support;
use serde_json::{Value, json};
fn css(selector: &str) -> Value { json!({"by":"css","selector":selector}) }
fn node<'a>(nodes: &'a Value, name: &str) -> &'a Value {
    nodes.as_array().unwrap().iter().find(|n|n["name"]==name).unwrap_or_else(||panic!("missing {name}: {nodes}"))
}
async fn launch(m: &mut support::Mcp, frames: bool) -> Value {
    let addr = support::serve();
    m.call_tool("launch",json!({"url":format!("http://{addr}/geometry-diff.html{}", if frames {""} else {"?main"}),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}
async fn click(m: &mut support::Mcp, id: &Value, target: Value) {
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":target},"then":{"type":"none"}})).await;
}

#[tokio::test]
async fn geometry_only_and_focus_only_diffs_preserve_scope_omissions_and_stopped_time() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m,false).await;
    let id = &launched["launch_id"];
    let base = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"snapshot","scope":css("#scope"),"geometry":true}})).await;
    assert_eq!(node(&base["nodes"],"No box")["geometry"]["state"],"no_box");
    let old = node(&base["nodes"],"Target").clone();
    assert_eq!(old["geometry"]["border_quad"],json!([20.0,40.0,100.0,40.0,100.0,70.0,20.0,70.0]));
    let limited = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"snapshot","scope":css("#scope"),"geometry":true,"max_nodes":1}})).await;
    assert!(limited["omitted"].as_u64().unwrap()>0);
    click(&mut m,id,css("#move")).await;
    let before = m.call_tool("status",json!({"launch_id":id})).await;
    let small = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":limited["observation_id"],"max_nodes":1}})).await;
    assert_eq!(small["omitted"],1,"two geometry-only changes are retained despite the initial node limit: {small}");
    let moved = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":limited["observation_id"]}})).await;
    assert_eq!(moved["added"],json!([]));
    assert_eq!(moved["removed"],json!([]));
    assert_eq!(moved["changed"].as_array().unwrap().len(),2);
    let target = node(&moved["changed"],"Target");
    assert_eq!(target["ref"],old["ref"]);
    assert_eq!(target["states"],old["states"]);
    assert_eq!(target["geometry"]["border_quad"],json!([45.0,55.0,165.0,55.0,165.0,85.0,45.0,85.0]));
    assert_eq!(small["boundary"],moved["boundary"]);
    let after = m.call_tool("status",json!({"launch_id":id})).await;
    assert_eq!(before["t_us"],after["t_us"]);
    assert_eq!(before["observation_seq"],after["observation_seq"]);
    click(&mut m,id,css("#target")).await;
    let focused = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":moved["observation_id"]}})).await;
    let focus = node(&focused["changed"],"Target");
    assert_eq!(focused["changed"].as_array().unwrap().len(),1);
    assert_eq!(focus["geometry"],target["geometry"]);
    assert!(focus["states"].as_array().unwrap().contains(&json!("focused")));
    assert_eq!(focus["name"],target["name"]);
    let stable = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":focused["observation_id"]}})).await;
    assert_eq!(stable["changed"],json!([]));
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn transformed_child_geometry_survives_recorder_replacement_and_defaults_remain_compact() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m,true).await;
    let id = &launched["launch_id"];
    let child = |target|json!({"by":"frame","frame":css("#child"),"target":target});
    let scope = child(css("#scope"));
    let plain = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"snapshot","scope":scope}})).await;
    assert!(plain.get("geometry").is_none());
    assert!(plain["nodes"].as_array().unwrap().iter().all(|n|n.get("geometry").is_none()));
    let base = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"snapshot","scope":scope,"geometry":true}})).await;
    assert_eq!(node(&base["nodes"],"Target")["geometry"]["border_quad"],json!([325.0,250.0,425.0,250.0,425.0,287.5,325.0,287.5]));
    click(&mut m,id,child(css("#move"))).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let runtime = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    runtime.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,runtime.browser);
    let diff = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":base["observation_id"]}})).await;
    assert_eq!(diff["changed"].as_array().unwrap().len(),2);
    let target = node(&diff["changed"],"Target");
    assert_eq!(target["geometry"]["border_quad"],json!([356.25,268.75,506.25,268.75,506.25,306.25,356.25,306.25]));
    let inspect = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"inspect","target":{"by":"ref","id":target["ref"]}}})).await;
    assert_eq!(target["geometry"]["border_quad"],inspect["border_quad"]);
    assert_eq!(diff["boundary"],inspect["boundary"]);
    m.call_tool("stop",json!({"launch_id":id})).await;
}
