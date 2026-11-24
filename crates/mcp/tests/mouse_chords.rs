mod support;
use serde_json::{Value, json};
fn css(s: &str) -> Value { json!({"by":"css","selector":s}) }
fn target(s: &str, child: bool) -> Value {
    if child { json!({"by":"frame","frame":css("#child"),"target":css(s)}) } else { css(s) }
}
async fn text(m: &mut support::Mcp, id: &Value, s: &str, child: bool) -> Value {
    m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":target(s,child),"props":["text"]}})).await["values"]["text"].clone()
}
async fn launch(m: &mut support::Mcp, child: bool) -> Value {
    let addr=support::serve();
    m.call_tool("launch", json!({"url":format!("http://{addr}/mouse-chords.html{}",if child {"?parent"} else {""}),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}
async fn begin(m: &mut support::Mcp, id: &Value, child: bool) -> Value {
    let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;
    m.call_tool("input",json!({"launch_id":id,"operation":"begin","target":target("#pad",child),"point":[40,40],"boundary":state["boundary"]})).await
}
#[tokio::test]
async fn chord_press_move_partial_release_and_end_are_frozen_deduplicated_and_replayable() {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let launch=launch(&mut m,false).await;let id=&launch["launch_id"];
    let mut state=begin(&mut m,id,false).await;let handle=state["held"]["handle"].clone();let at=state["boundary"]["t_us"].clone();
    for button in ["right","middle"] {
        let args=json!({"launch_id":id,"operation":"press","request_id":format!("press-{button}"),"handle":handle,"button":button,"boundary":state["boundary"]});
        state=m.call_tool("input",args.clone()).await;
        assert_eq!(m.call_tool("input",args).await,state);
    }
    assert_eq!(state["held"]["buttons"],json!(["left","right","middle"]));
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"press","handle":handle,"button":"right","boundary":state["boundary"]})).await;
    assert!(bad&&e["code"]=="invalid_arguments","{e}");
    state=m.call_tool("input",json!({"launch_id":id,"operation":"move","handle":handle,"point":[100,40],"boundary":state["boundary"]})).await;
    assert_eq!(state["boundary"]["t_us"],at);
    state=m.call_tool("input",json!({"launch_id":id,"operation":"release","handle":handle,"button":"left"})).await;
    assert_eq!(state["held"]["buttons"],json!(["right","middle"]));
    assert_eq!(state["held"]["button"],"right");
    assert_eq!(state["held"]["handle"],handle);
    let ended=m.call_tool("input",json!({"launch_id":id,"operation":"end","handle":handle})).await;
    assert!(ended["held"].is_null());assert_eq!(ended["boundary"]["t_us"],at);
    assert_eq!(text(&mut m,id,"#timer",false).await,"pending");
    let seen:Value=serde_json::from_str(text(&mut m,id,"#events",false).await.as_str().unwrap()).unwrap();
    let pointer:Vec<_>=seen.as_array().unwrap().iter().filter(|e|matches!(e[0].as_str(),Some("pointerdown"|"pointermove"|"pointerup"))).map(|e|{
        assert_eq!(e[3],true);assert_eq!(e[4],at.as_u64().unwrap()/1000);
        json!([e[0],e[1],e[2]])
    }).collect();
    assert_eq!(pointer,vec![json!(["pointerdown",0,1]),json!(["pointermove",2,3]),json!(["pointermove",1,7]),json!(["pointermove",-1,7]),json!(["pointermove",0,6]),json!(["pointermove",1,2]),json!(["pointerup",2,0])]);
    let state=begin(&mut m,id,false).await;
    let second_handle=state["held"]["handle"].clone();
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"release","handle":handle,"button":"left"})).await;
    assert!(bad&&e["code"]=="invalid_arguments","a prior handle cannot release the new hold: {e}");
    let ended=m.call_tool("input",json!({"launch_id":id,"operation":"release","handle":second_handle,"button":"left"})).await;
    assert!(ended["held"].is_null());assert_eq!(ended["boundary"]["t_us"],at);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{replay}");
}
#[tokio::test]
async fn child_chord_cancel_and_recorder_replacement_release_all_buttons_without_clicking() {
    for cleanup in ["cancel","release","crash"] {
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let launch=launch(&mut m,true).await;let id=&launch["launch_id"];
        let mut state=begin(&mut m,id,true).await;let handle=state["held"]["handle"].clone();let at=state["boundary"]["t_us"].clone();
        for button in ["right","middle"] {
            state=m.call_tool("input",json!({"launch_id":id,"operation":"press","handle":handle,"button":button,"boundary":state["boundary"]})).await;
        }
        if cleanup=="cancel" { m.call_tool("input",json!({"launch_id":id,"operation":"cancel","handle":handle})).await; }
        else {
            if cleanup=="crash" {
                let runtime=frontcap_recorder::runtime::load_runtime(&ws.root().join("runtimes").join(id.as_str().unwrap())).unwrap().unwrap();
                runtime.recorder.kill().unwrap();
                m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
                assert_eq!(frontcap_recorder::runtime::load_runtime(&ws.root().join("runtimes").join(id.as_str().unwrap())).unwrap().unwrap().browser,runtime.browser);
            } else {
                m.call_tool("session",json!({"launch_id":id,"operation":"release"})).await;
                m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
            }
        }
        state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;
        assert!(state["held"].is_null());assert_eq!(state["boundary"]["t_us"],at);
        let seen:Value=serde_json::from_str(text(&mut m,id,"#events",true).await.as_str().unwrap()).unwrap();
        let terminal:Vec<_>=seen.as_array().unwrap().iter().filter(|e|matches!(e[0].as_str(),Some("pointercancel"|"pointerup"|"click"|"auxclick"))).map(|e|json!([e[0],e[2],e[3]])).collect();
        assert_eq!(terminal,vec![json!(["pointercancel",0,true])],"{cleanup}: {seen}");
        m.call_tool("stop",json!({"launch_id":id})).await;
        if cleanup!="crash" {
            let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{cleanup}: {replay}");
        }
    }
}
