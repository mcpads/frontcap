mod support;
use serde_json::{Value,json};
fn css(s:&str)->Value{json!({"by":"css","selector":s})}
fn target(s:&str,child:bool)->Value{if child{json!({"by":"frame","frame":css("#child"),"target":css(s)})}else{css(s)}}
async fn text(m:&mut support::Mcp,id:&Value,s:&str,child:bool)->Value{
    m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":target(s,child),"props":["text"]}})).await["values"]["text"].clone()
}
async fn launch(m:&mut support::Mcp,child:bool)->Value{
    let addr=support::serve();m.call_tool("launch",json!({"url":format!("http://{addr}/multitouch.html{}",if child{"?parent"}else{""}),"profile":{"touch":true},"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}
async fn begin(m:&mut support::Mcp,id:&Value,child:bool)->Value{
    let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;
    m.call_tool("input",json!({"launch_id":id,"operation":"begin","pointer":"touch","target":target("#pad",child),"point":[40,40],"boundary":state["boundary"]})).await
}
#[tokio::test]
async fn pinch_contacts_move_end_and_replay_without_advancing_time_or_reusing_ids(){
    let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
    let launch=launch(&mut m,false).await;let id=&launch["launch_id"];
    let mut state=begin(&mut m,id,false).await;let handle=state["held"]["handle"].clone();let at=state["boundary"]["t_us"].clone();
    assert_eq!(state["held"]["contacts"],json!([{"id":0,"point":[40,40]}]));
    let args=json!({"launch_id":id,"request_id":"second-finger","operation":"touch_add","handle":handle,"target":css("#pad"),"point":[120,40],"boundary":state["boundary"]});
    state=m.call_tool("input",args.clone()).await;assert_eq!(m.call_tool("input",args).await,state);
    assert_eq!(state["held"]["contacts"],json!([{"id":0,"point":[40,40]},{"id":1,"point":[120,40]}]));
    assert_eq!(text(&mut m,id,"#distance",false).await,"80");
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"move","handle":handle,"point":[60,40],"boundary":state["boundary"]})).await;
    assert!(bad&&e["code"]=="invalid_arguments","{e}");
    state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_move","handle":handle,"contact":1,"point":[160,40],"boundary":state["boundary"]})).await;
    assert_eq!(text(&mut m,id,"#distance",false).await,"120");
    assert_eq!(state["held"]["contacts"][0]["point"],json!([40,40]));
    state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_end","handle":handle,"contact":0})).await;
    assert_eq!(state["held"]["contacts"],json!([{"id":1,"point":[160,40]}]));
    assert_eq!(state["held"]["handle"],handle);
    state=m.call_tool("input",json!({"launch_id":id,"operation":"move","handle":handle,"point":[180,40],"boundary":state["boundary"]})).await;
    for x in [60,80,100,140]{
        state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_add","handle":handle,"target":css("#pad"),"point":[x,80],"boundary":state["boundary"]})).await;
    }
    assert_eq!(state["held"]["contacts"].as_array().unwrap().iter().map(|c|c["id"].as_u64().unwrap()).collect::<Vec<_>>(),vec![1,2,3,4,5]);
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"touch_add","handle":handle,"target":css("#pad"),"point":[180,80],"boundary":state["boundary"]})).await;
    assert!(bad&&e["code"]=="invalid_arguments","{e}");
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"touch_end","handle":handle,"contact":0})).await;
    assert!(bad&&e["code"]=="invalid_arguments","{e}");
    let ended=m.call_tool("input",json!({"launch_id":id,"operation":"end","handle":handle})).await;
    assert!(ended["held"].is_null());assert_eq!(ended["boundary"]["t_us"],at);
    assert_eq!(text(&mut m,id,"#timer",false).await,"pending");assert_eq!(text(&mut m,id,"#clicks",false).await,"0");
    let seen:Value=serde_json::from_str(text(&mut m,id,"#events",false).await.as_str().unwrap()).unwrap();let seen=seen.as_array().unwrap();
    assert!(seen.iter().all(|e|e[3]==true&&e[4].as_u64().unwrap()*1000==at.as_u64().unwrap()),"{seen:?}");
    assert_eq!(seen[0][1],json!([[0,40,40]]));assert_eq!(seen[1][1],json!([[0,40,40],[1,120,40]]));
    assert_eq!(seen[2][1],json!([[0,40,40],[1,160,40]]));assert_eq!(seen[3][0],"touchend");assert_eq!(seen[3][1],json!([[1,160,40]]));assert_eq!(seen[3][2],json!([[0,40,40]]));
    assert_eq!(seen.last().unwrap()[1],json!([]));
    let fresh=begin(&mut m,id,false).await;
    let (bad,e)=m.try_tool("input",json!({"launch_id":id,"operation":"touch_end","handle":handle,"contact":0})).await;
    assert!(bad&&e["code"]=="invalid_arguments","the prior handle cannot affect a new contact zero: {e}");
    let ended=m.call_tool("input",json!({"launch_id":id,"operation":"touch_end","handle":fresh["held"]["handle"],"contact":0})).await;
    assert!(ended["held"].is_null());assert_eq!(ended["boundary"]["t_us"],at);
    m.call_tool("stop",json!({"launch_id":id})).await;let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{replay}");
}
#[tokio::test]
async fn child_multitouch_cancels_all_contacts_on_cancel_lease_loss_and_recorder_replacement(){
    for cleanup in ["cancel","release","crash"]{
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let launch=launch(&mut m,true).await;let id=&launch["launch_id"];
        let mut state=begin(&mut m,id,true).await;let handle=state["held"]["handle"].clone();let at=state["boundary"]["t_us"].clone();
        state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_add","handle":handle,"target":target("#pad",true),"point":[140,40],"boundary":state["boundary"]})).await;
        assert_eq!(state["held"]["contacts"].as_array().unwrap().len(),2);
        if cleanup=="cancel"{
            let cancelled=m.call_tool("input",json!({"launch_id":id,"operation":"cancel","handle":handle})).await;
            let events=cancelled["effects"]["input"][0]["events"].as_array().unwrap();
            assert_eq!(events.len(),1,"{cancelled}");
            assert_eq!(events[0]["event"],"touchcancel","{cancelled}");
            assert_eq!(events[0]["t_us"],at,"{cancelled}");
            assert_eq!(events[0]["trusted"],true,"{cancelled}");
        }
        else if cleanup=="release"{
            m.call_tool("session",json!({"launch_id":id,"operation":"release"})).await;m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
        }else{
            let dir=ws.root().join("runtimes").join(id.as_str().unwrap());let old=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();old.recorder.kill().unwrap();
            m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,old.browser);
        }
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;assert!(state["held"].is_null());assert_eq!(state["boundary"]["t_us"],at);
        let seen:Value=serde_json::from_str(text(&mut m,id,"#events",true).await.as_str().unwrap()).unwrap();let terminal:Vec<_>=seen.as_array().unwrap().iter().filter(|e|e[0]=="touchend"||e[0]=="touchcancel").collect();
        assert_eq!(terminal.len(),1,"{cleanup}: {seen}");assert_eq!(terminal[0][0],"touchcancel");assert_eq!(terminal[0][1],json!([]));assert_eq!(terminal[0][2],json!([[0,40,40],[1,140,40]]));assert_eq!(terminal[0][3],true);assert_eq!(terminal[0][4].as_u64().unwrap()*1000,at.as_u64().unwrap());
        assert_eq!(text(&mut m,id,"#clicks",true).await,"0");
        m.call_tool("stop",json!({"launch_id":id})).await;
        if cleanup!="crash"{let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{cleanup}: {replay}");}
    }
}
/// A second finger over another document than the first's: Chrome sends its pointer events to the element
/// under it, and its touch events to the first finger's document — to the frame element there when the
/// finger is inside that document's frame, nowhere when it is outside.
#[tokio::test]
async fn a_contact_over_another_document_reaches_each_document_as_chrome_sends_it(){
    let events=|seen:Value|->Value{serde_json::from_str(seen.as_str().unwrap()).unwrap()};
    for first_in_child in [false,true]{
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let addr=support::serve();
        let launch=m.call_tool("launch",json!({"url":format!("http://{addr}/touch-span.html?parent"),"profile":{"touch":true},"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
        let id=&launch["launch_id"];
        let top=css("#top");let pad=json!({"by":"frame","frame":css("#child"),"target":css("#pad")});
        let (first,at,second,to)=if first_in_child{(pad.clone(),[360,60],top.clone(),[60,60])}else{(top.clone(),[60,60],pad.clone(),[360,60])};
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;let t=state["boundary"]["t_us"].clone();
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"begin","pointer":"touch","target":first,"point":at,"boundary":state["boundary"]})).await;
        let handle=state["held"]["handle"].clone();
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_add","handle":handle,"target":second,"point":to,"boundary":state["boundary"]})).await;
        assert_eq!(state["held"]["contacts"],json!([{"id":0,"point":at},{"id":1,"point":to,"other_document":true}]));
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"touch_move","handle":handle,"contact":1,"point":[to[0]+20,to[1]+10],"boundary":state["boundary"]})).await;
        m.call_tool("input",json!({"launch_id":id,"operation":"touch_end","handle":handle,"contact":1})).await;
        let ended=m.call_tool("input",json!({"launch_id":id,"operation":"end","handle":handle})).await;
        assert!(ended["held"].is_null());assert_eq!(ended["boundary"]["t_us"],t,"{state}");
        let top_seen=events(text(&mut m,id,"#top-events",false).await);
        let child_seen=events(m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":{"by":"frame","frame":css("#child"),"target":css("#events")},"props":["text"]}})).await["values"]["text"].clone());
        if first_in_child{
            // The second finger's pointer events in the top document; no touch event for it anywhere.
            assert_eq!(top_seen,json!([["pointerdown","top",0,60,60],["pointermove","top",0,80,70],["pointerup","top",0,80,70]]));
            assert_eq!(child_seen,json!([["pointerdown","pad",0,60,60],["touchstart","pad",[[0,60,60]],[[0,60,60]]],["pointerup","pad",0,60,60],["touchend","pad",[],[[0,60,60]]]]));
        }else{
            // The second finger's touch events at the frame element in the top document, its pointer events in the frame.
            assert_eq!(top_seen,json!([["pointerdown","top",0,60,60],["touchstart","top",[[0,60,60]],[[0,60,60]]],
                ["touchstart","child",[[0,60,60],[1,360,60]],[[1,360,60]]],["touchmove","child",[[0,60,60],[1,380,70]],[[1,380,70]]],
                ["touchend","child",[[0,60,60]],[[1,380,70]]],["pointerup","top",0,60,60],["touchend","top",[],[[0,60,60]]]]));
            assert_eq!(child_seen,json!([["pointerdown","pad",0,60,60],["pointermove","pad",0,80,70],["pointerup","pad",0,80,70]]));
        }
        m.call_tool("stop",json!({"launch_id":id})).await;
        let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{replay}");
    }
}
#[tokio::test]
async fn contacts_over_two_documents_are_cancelled_on_cancel_lease_loss_and_recorder_replacement(){
    let events=|seen:Value|->Value{serde_json::from_str(seen.as_str().unwrap()).unwrap()};
    for cleanup in ["cancel","release","crash"]{
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let addr=support::serve();
        let launch=m.call_tool("launch",json!({"url":format!("http://{addr}/touch-span.html?parent"),"profile":{"touch":true},"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
        let id=&launch["launch_id"];
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;let at=state["boundary"]["t_us"].clone();
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"begin","pointer":"touch","target":{"by":"frame","frame":css("#child"),"target":css("#pad")},"point":[360,60],"boundary":state["boundary"]})).await;
        let handle=state["held"]["handle"].clone();
        m.call_tool("input",json!({"launch_id":id,"operation":"touch_add","handle":handle,"target":css("#top"),"point":[60,60],"boundary":state["boundary"]})).await;
        if cleanup=="cancel"{m.call_tool("input",json!({"launch_id":id,"operation":"cancel","handle":handle})).await;}
        else if cleanup=="release"{
            m.call_tool("session",json!({"launch_id":id,"operation":"release"})).await;m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
        }else{
            let dir=ws.root().join("runtimes").join(id.as_str().unwrap());let old=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();old.recorder.kill().unwrap();
            m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
        }
        let state=m.call_tool("input",json!({"launch_id":id,"operation":"status"})).await;assert!(state["held"].is_null(),"{cleanup}");assert_eq!(state["boundary"]["t_us"],at,"{cleanup}");
        // Each finger is cancelled where its events went: the first's touches and pointer in the frame, the
        // second's pointer in the top document.
        let top_seen=events(text(&mut m,id,"#top-events",false).await);
        let child_seen=events(m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":{"by":"frame","frame":css("#child"),"target":css("#events")},"props":["text"]}})).await["values"]["text"].clone());
        assert_eq!(top_seen,json!([["pointerdown","top",0,60,60],["pointercancel","top",0,60,60]]),"{cleanup}");
        assert_eq!(child_seen,json!([["pointerdown","pad",0,60,60],["touchstart","pad",[[0,60,60]],[[0,60,60]]],["pointercancel","pad",0,60,60],["touchcancel","pad",[],[[0,60,60]]]]),"{cleanup}");
        m.call_tool("stop",json!({"launch_id":id})).await;
        if cleanup!="crash"{let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;assert_eq!(replay["equal"],true,"{cleanup}: {replay}");}
    }
}
