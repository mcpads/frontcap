mod support;
use serde_json::{Value,json};
fn css(selector:&str)->Value { json!({"by":"css","selector":selector}) }
fn child(selector:&str)->Value { json!({"by":"frame","frame":css("#child"),"target":css(selector)}) }
async fn query(m:&mut support::Mcp,id:&Value,target:Value,prop:&str)->Value {
    m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":target,"props":[prop]}})).await["values"][prop].clone()
}

#[tokio::test]
async fn child_composition_and_held_keys_remain_native_frozen_and_recoverable() {
    for shadow in [false,true] {
        let ws=support::workspace();
        let addr=support::serve();
        let mut m=support::Mcp::start_in(&ws.root()).await;
        let launch=m.call_tool("launch",json!({"url":format!("http://{addr}/child-input.html{}",if shadow {"?shadow"} else {""}),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
        let id=&launch["launch_id"];
        let clicked=m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("#editor")},"then":{"type":"none"}})).await;
        let at=clicked["t_after_us"].as_u64().unwrap();
        for text in ["ㅎ","하","한"] {
            m.call_tool("input",json!({"launch_id":id,"operation":"compose","text":text})).await;
        }
        assert_eq!(query(&mut m,id,child("#editor"),"value").await,"한");
        let committed=m.call_tool("input",json!({"launch_id":id,"operation":"commit","key":"Enter","request_id":"commit-hangul"})).await;
        assert!(committed["composition"].is_null());
        assert_eq!(m.call_tool("input",json!({"launch_id":id,"operation":"commit","key":"Enter","request_id":"commit-hangul"})).await,committed);
        m.call_tool("input",json!({"launch_id":id,"operation":"key_down","key":"Shift"})).await;
        m.call_tool("session",json!({"launch_id":id,"operation":"release"})).await;
        m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
        assert_eq!(query(&mut m,id,css("#parent"),"value").await,"");
        assert_eq!(query(&mut m,id,child("#timer"),"text").await,"pending");
        let events:Value=serde_json::from_str(query(&mut m,id,child("#events"),"text").await.as_str().unwrap()).unwrap();
        let events=events.as_array().unwrap();
        assert!(events.iter().all(|e|e["time"]==at/1000),"{events:?}");
        assert!(events.iter().filter(|e|e["type"]!="compositionend").all(|e|e["trusted"]==true),"{events:?}");
        assert_eq!(events.iter().filter(|e|e["type"]=="compositionend").count(),1);
        assert_eq!(events.iter().filter(|e|e["type"]=="keyup" && e["key"]=="Shift").count(),1,"release must deliver native modifier cleanup to the focused child");
        assert!(events.iter().any(|e|e["type"]=="keydown" && e["key"]=="Enter" && e["composing"]==true));
        assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],at);
        if shadow {
            m.call_tool("input",json!({"launch_id":id,"operation":"compose","text":"글"})).await;
            let dir=ws.root().join("runtimes").join(id.as_str().unwrap());
            let runtime=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
            runtime.recorder.kill().unwrap();
            m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
            assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,runtime.browser);
            assert!(m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await["composition"].is_null());
            assert_eq!(query(&mut m,id,child("#editor"),"value").await,"한","recorder lease loss cancels the unfinished child composition");
            let cleaned:Value=serde_json::from_str(query(&mut m,id,child("#events"),"text").await.as_str().unwrap()).unwrap();
            assert_eq!(cleaned.as_array().unwrap().iter().filter(|e|e["type"]=="compositionend").count(),2);
            assert_eq!(query(&mut m,id,child("#timer"),"text").await,"pending");
            m.call_tool("input",json!({"launch_id":id,"operation":"compose","text":"글"})).await;
            m.call_tool("input",json!({"launch_id":id,"operation":"commit"})).await;
            assert_eq!(query(&mut m,id,child("#editor"),"value").await,"한글");
        }
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
        assert_eq!(query(&mut m,id,child("#timer"),"text").await,"fired");
        m.call_tool("stop",json!({"launch_id":id})).await;
        if !shadow {
            let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
            assert_eq!(replay["equal"],true,"{replay}");
        }
    }
}

#[tokio::test]
async fn removing_a_focused_child_retires_its_reference_and_parent_input_still_works() {
    let ws=support::workspace();
    let addr=support::serve();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let launch=m.call_tool("launch",json!({"url":format!("http://{addr}/child-input.html?shadow"),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
    let id=&launch["launch_id"];
    let read=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":child("#editor"),"props":["value"]}})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#remove")},"then":{"type":"none"}})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("#editor")},"then":{"type":"none"}})).await;
    m.call_tool("input",json!({"launch_id":id,"operation":"compose","text":"한"})).await;
    let removed=m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
    assert!(m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await["composition"].is_null());
    let (failed,error)=m.try_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":{"by":"ref","id":read["ref"]},"props":["value"]}})).await;
    assert!(failed && error["code"]=="stale_ref","{error}");
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#parent")},"then":{"type":"none"}})).await;
    m.call_tool("input",json!({"launch_id":id,"operation":"compose","text":"부모"})).await;
    m.call_tool("input",json!({"launch_id":id,"operation":"commit"})).await;
    assert_eq!(query(&mut m,id,css("#parent"),"value").await,"부모");
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],removed["t_us"]);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}


#[tokio::test]
async fn physical_keys_repeat_once_keep_both_shifts_and_cleanup_through_replay() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/child-input.html"),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
    let id = &launch["launch_id"];
    let clicked = m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":child("#editor")},"then":{"type":"none"}})).await;
    let at = clicked["t_after_us"].clone();
    for key in ["ShiftLeft", "ShiftRight", "KeyA"] {
        m.call_tool("input", json!({"launch_id":id,"operation":"key_down","key":key})).await;
    }
    let (failed, error) = m.try_tool("input", json!({"launch_id":id,"operation":"key_down","key":"a"})).await;
    assert!(failed && error["code"] == "invalid_arguments", "{error}");
    m.call_tool("input", json!({"launch_id":id,"operation":"key_up","key":"ShiftLeft"})).await;
    let repeat = json!({"launch_id":id,"request_id":"repeat-a","operation":"key_repeat","key":"KeyA"});
    let repeated = m.call_tool("input", repeat.clone()).await;
    assert_eq!(m.call_tool("input", repeat).await, repeated, "retry must not insert another character");
    assert_eq!(repeated["keys"], json!(["ShiftRight", "KeyA"]));
    assert_eq!(query(&mut m,id,child("#editor"),"value").await, "AA");
    m.call_tool("input", json!({"launch_id":id,"operation":"key_up","key":"ShiftRight"})).await;
    m.call_tool("input", json!({"launch_id":id,"operation":"key_repeat","key":"KeyA"})).await;
    assert_eq!(query(&mut m,id,child("#editor"),"value").await, "AAa");
    assert_eq!(query(&mut m,id,child("#timer"),"text").await, "pending");
    m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let state = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    assert_eq!(state["keys"], json!([]));
    assert_eq!(state["boundary"]["t_us"], at);
    let (failed, error) = m.try_tool("input", json!({"launch_id":id,"operation":"key_repeat","key":"KeyA"})).await;
    assert!(failed && error["code"] == "invalid_arguments", "{error}");
    let events: Value = serde_json::from_str(query(&mut m,id,child("#events"),"text").await.as_str().unwrap()).unwrap();
    let keys: Vec<Value> = events.as_array().unwrap().iter().filter(|e| e["type"] == "keydown" || e["type"] == "keyup").map(|e| {
        assert_eq!(e["trusted"], true);
        assert_eq!(e["time"], at.as_u64().unwrap()/1000);
        json!([e["type"],e["key"],e["code"],e["repeat"],e["location"],e["shift"]])
    }).collect();
    assert_eq!(keys, vec![
        json!(["keydown","Shift","ShiftLeft",false,1,true]),
        json!(["keydown","Shift","ShiftRight",false,2,true]),
        json!(["keydown","A","KeyA",false,0,true]),
        json!(["keyup","Shift","ShiftLeft",false,1,true]),
        json!(["keydown","A","KeyA",true,0,true]),
        json!(["keyup","Shift","ShiftRight",false,2,false]),
        json!(["keydown","a","KeyA",true,0,false]),
        json!(["keyup","a","KeyA",false,0,false]),
    ]);
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}
