mod support;
use serde_json::{Value,json};
fn css(s:&str)->Value{json!({"by":"css","selector":s})}
fn child(s:&str)->Value{json!({"by":"frame","frame":css("#child"),"target":css(s)})}
async fn launch(m:&mut support::Mcp,suffix:&str,pointer:&str)->Value{
    let addr=support::serve();
    m.call_tool("launch",json!({"url":format!("http://{addr}/child-pointer.html{suffix}"),"profile":{"touch":pointer=="touch"},"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}
async fn begin(m:&mut support::Mcp,id:&Value,pointer:&str)->Value{
    let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;
    let args=json!({"launch_id":id,"operation":"begin","pointer":pointer,"request_id":"begin","target":child("#pad"),"point":[160,160],"boundary":state["boundary"]});
    let begun=m.call_tool("input",args.clone()).await;
    assert_eq!(m.call_tool("input",args).await,begun);
    begun
}
async fn events(m:&mut support::Mcp,id:&Value)->Value{
    let read=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":child("#events"),"props":["text"]}})).await;
    serde_json::from_str(read["values"]["text"].as_str().unwrap()).unwrap()
}
#[tokio::test]
async fn child_capture_moves_in_viewport_coordinates_and_replays() {
    for pointer in ["mouse","touch"] {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let launched=launch(&mut m,"",pointer).await;let id=&launched["launch_id"];
    let begun=begin(&mut m,id,pointer).await;let handle=&begun["held"]["handle"];
    let moved=m.call_tool("input",json!({"launch_id":id,"operation":"move","handle":handle,"point":[340,160],"boundary":begun["boundary"]})).await;
    let read=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":child("#pad"),"props":["style:left"]}})).await;
    assert_eq!(read["values"]["style:left"],"140px");
    let ended=m.call_tool("input",json!({"launch_id":id,"operation":"end","handle":handle})).await;
    assert!(ended["held"].is_null());assert_eq!(moved["boundary"]["t_us"],begun["boundary"]["t_us"]);
    let seen=events(&mut m,id).await;
    assert!(seen.as_array().unwrap().iter().all(|e|e[1]==true && e[3]==pointer && e[2].as_u64().unwrap()*1000==begun["boundary"]["t_us"].as_u64().unwrap()));
    assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[0]=="pointerdown").count(),1);
    assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[0]=="pointerup").count(),1);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{pointer}: {replay}");
    }
}
#[tokio::test]
async fn recorder_replacement_cancels_the_retained_child_pointer_once() {
    for pointer in ["mouse","touch"] {
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let launched=launch(&mut m,"",pointer).await;let id=&launched["launch_id"];
    let begun=begin(&mut m,id,pointer).await;
    let dir=ws.root().join("runtimes").join(id.as_str().unwrap());
    let runtime=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();runtime.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,runtime.browser);
    let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;
    assert!(state["held"].is_null());assert_eq!(state["boundary"]["t_us"],begun["boundary"]["t_us"]);
    let seen=events(&mut m,id).await;
    assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[0]=="pointercancel" && e[1]==true && e[3]==pointer).count(),1,"{seen}");
    assert!(!seen.as_array().unwrap().iter().any(|e|e[0]=="pointerup"||e[0]=="click"));
    let (failed,error)=m.try_tool("input",json!({"launch_id":id,"operation":"end","handle":begun["held"]["handle"]})).await;
    assert!(failed && error["code"]=="invalid_arguments","{error}");
    m.call_tool("stop",json!({"launch_id":id})).await;
    }
}
#[tokio::test]
async fn child_removal_or_navigation_fails_the_generation_before_handle_reuse() {
    for suffix in ["?remove","?navigate"] {
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let launched=launch(&mut m,suffix,"mouse").await;let id=&launched["launch_id"];
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#destroy")},"then":{"type":"none"}})).await;
        begin(&mut m,id,"mouse").await;
        let (failed,error)=m.try_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
        assert!(failed && error["code"]=="recorder_fault","{suffix}: {error}");
        let dir=ws.root().join("runtimes").join(id.as_str().unwrap());
        let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(5);
        let terminal=loop {
            let runtime=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
            if !runtime.recorder.is_alive(){break frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap()}
            assert!(tokio::time::Instant::now()<deadline,"failed recorder did not terminate");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(matches!(terminal.state,frontcap_recorder::runtime::RuntimeState::Failed{..}),"{terminal:?}");
        assert!(!terminal.browser.unwrap().is_alive());
    }
}

#[tokio::test]
async fn child_cancel_is_deduplicated_and_replays_without_release() {
    for pointer in ["mouse", "touch"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launched = launch(&mut m, "", pointer).await;
        let id = &launched["launch_id"];
        let begun = begin(&mut m, id, pointer).await;
        let args = json!({"launch_id":id,"operation":"cancel","request_id":"cancel","handle":begun["held"]["handle"]});
        let cancelled = m.call_tool("input", args.clone()).await;
        assert!(cancelled["held"].is_null());
        assert_eq!(cancelled["boundary"]["t_us"], begun["boundary"]["t_us"]);
        assert_eq!(m.call_tool("input", args).await, cancelled);
        let seen = events(&mut m, id).await;
        let cancellation: Vec<_> = seen.as_array().unwrap().iter().filter(|event| event[0] == "pointercancel").collect();
        assert_eq!(cancellation.len(), 1, "{pointer}: {seen}");
        assert_eq!(cancellation[0][1], true);
        assert_eq!(cancellation[0][3], pointer);
        assert!(!seen.as_array().unwrap().iter().any(|event| event[0] == "pointerup" || event[0] == "click"));
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{pointer}: {replay}");
    }
}
