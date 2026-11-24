mod support;
use serde_json::{Value, json};
fn css(s: &str) -> Value {
    json!({"by":"css","selector":s})
}
fn target(s: &str, child: bool) -> Value {
    if child {
        json!({"by":"frame","frame":css("#child"),"target":css(s)})
    } else {
        css(s)
    }
}
async fn text(m: &mut support::Mcp, id: &Value, s: &str, child: bool) -> Value {
    m.call_tool(
        "observe",
        json!({"launch_id":id,"op":{"type":"query","target":target(s,child),"props":["text"]}}),
    )
    .await["values"]["text"]
        .clone()
}
async fn launch(m: &mut support::Mcp, suffix: &str) -> Value {
    let addr = support::serve();
    m.call_tool("launch",json!({"url":format!("http://{addr}/held-html-drag.html{suffix}"),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}
async fn begin(m: &mut support::Mcp, id: &Value, child: bool) -> Value {
    let state = m
        .call_tool("input", json!({"launch_id":id,"operation":"status"}))
        .await;
    m.call_tool("input",json!({"launch_id":id,"operation":"begin","target":target("#src",child),"point":[40,40],"boundary":state["boundary"]})).await
}
async fn move_to(m: &mut support::Mcp, id: &Value, state: &Value, x: i32) -> Value {
    m.call_tool("input",json!({"launch_id":id,"operation":"move","handle":state["held"]["handle"],"point":[x,40],"boundary":state["boundary"]})).await
}
#[tokio::test]
async fn html_drag_continues_between_calls_and_drops_native_data_with_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launch = launch(&mut m, "").await;
    let id = &launch["launch_id"];
    let mut state = begin(&mut m, id, false).await;
    let at = state["boundary"]["t_us"].clone();
    state = move_to(&mut m, id, &state, 90).await;
    assert_eq!(state["held"]["html_drag"], true);
    state = move_to(&mut m, id, &state, 240).await;
    assert_eq!(text(&mut m, id, "#hover", false).await, "true");
    let args = json!({"launch_id":id,"request_id":"drag-over","operation":"move","handle":state["held"]["handle"],"point":[250,40],"boundary":state["boundary"]});
    state = m.call_tool("input", args.clone()).await;
    assert_eq!(m.call_tool("input", args).await, state);
    assert_eq!(text(&mut m, id, "#drops", false).await, "0");
    assert_eq!(text(&mut m, id, "#timer", false).await, "pending");
    let ended = m
        .call_tool(
            "input",
            json!({"launch_id":id,"operation":"end","handle":state["held"]["handle"]}),
        )
        .await;
    assert!(ended["held"].is_null());
    assert_eq!(ended["boundary"]["t_us"], at);
    let seen: Value =
        serde_json::from_str(text(&mut m, id, "#events", false).await.as_str().unwrap()).unwrap();
    assert!(
        seen.as_array()
            .unwrap()
            .iter()
            .all(|e| e[2] == true && e[3].as_u64().unwrap() * 1000 == at.as_u64().unwrap()),
        "{seen}"
    );
    assert_eq!(
        seen.as_array()
            .unwrap()
            .iter()
            .filter(|e| e[0] == "drop" && e[4] == "owned drag")
            .count(),
        1
    );
    assert_eq!(
        seen.as_array()
            .unwrap()
            .iter()
            .filter(|e| e[0] == "dragend")
            .count(),
        1
    );
    assert_eq!(text(&mut m, id, "#hover", false).await, "false");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m
        .call_tool("debug", json!({"launch_id":id,"operation":"replay"}))
        .await;
    assert_eq!(replay["equal"], true, "{replay}");
}
#[tokio::test]
async fn child_html_drag_cancel_lease_release_and_recorder_recovery_clear_source_and_target() {
    for mode in ["cancel", "release", "crash"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launch = launch(&mut m, "?parent").await;
        let id = &launch["launch_id"];
        let mut state = begin(&mut m, id, true).await;
        let at = state["boundary"]["t_us"].clone();
        for x in [90, 240, 250] {
            state = move_to(&mut m, id, &state, x).await;
        }
        assert_eq!(state["held"]["html_drag"], true);
        assert_eq!(text(&mut m, id, "#hover", true).await, "true");
        if mode == "cancel" {
            m.call_tool(
                "input",
                json!({"launch_id":id,"operation":"cancel","handle":state["held"]["handle"]}),
            )
            .await;
        } else if mode == "release" {
            m.call_tool("session", json!({"launch_id":id,"operation":"release"}))
                .await;
            m.call_tool("session", json!({"launch_id":id,"operation":"reattach"}))
                .await;
        } else {
            let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
            let before = frontcap_recorder::runtime::load_runtime(&dir)
                .unwrap()
                .unwrap();
            before.recorder.kill().unwrap();
            m.call_tool("session", json!({"launch_id":id,"operation":"reattach"}))
                .await;
            assert_eq!(
                frontcap_recorder::runtime::load_runtime(&dir)
                    .unwrap()
                    .unwrap()
                    .browser,
                before.browser
            );
        }
        let state = m
            .call_tool("input", json!({"launch_id":id,"operation":"status"}))
            .await;
        assert!(state["held"].is_null());
        assert_eq!(state["boundary"]["t_us"], at);
        assert_eq!(text(&mut m, id, "#drops", true).await, "0");
        assert_eq!(text(&mut m, id, "#hover", true).await, "false");
        let seen: Value =
            serde_json::from_str(text(&mut m, id, "#events", true).await.as_str().unwrap())
                .unwrap();
        assert_eq!(
            seen.as_array()
                .unwrap()
                .iter()
                .filter(|e| e[0] == "dragend")
                .count(),
            1,
            "{mode}: {seen}"
        );
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":target("#button",true)},"then":{"type":"none"}})).await;
        assert_eq!(text(&mut m, id, "#clicks", true).await, "1");
        m.call_tool("stop", json!({"launch_id":id})).await;
        if mode != "crash" {
            let replay = m
                .call_tool("debug", json!({"launch_id":id,"operation":"replay"}))
                .await;
            assert_eq!(replay["equal"], true, "{mode}: {replay}");
        }
    }
}
#[tokio::test]
async fn prevented_dragstart_keeps_an_ordinary_held_pointer() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launch = launch(&mut m, "?deny").await;
    let id = &launch["launch_id"];
    let state = begin(&mut m, id, false).await;
    let state = move_to(&mut m, id, &state, 90).await;
    assert_ne!(state["held"]["html_drag"], true);
    m.call_tool(
        "input",
        json!({"launch_id":id,"operation":"cancel","handle":state["held"]["handle"]}),
    )
    .await;
    assert_eq!(text(&mut m, id, "#drops", false).await, "0");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn removed_drag_source_allows_cancel_or_drop_and_subsequent_input() {
    for operation in ["cancel", "end"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launch = launch(&mut m, "?remove").await;
        let id = &launch["launch_id"];
        let mut state = begin(&mut m, id, false).await;
        let at = state["boundary"]["t_us"].clone();
        for x in [90, 240, 250] {
            state = move_to(&mut m, id, &state, x).await;
        }
        assert_eq!(state["held"]["html_drag"], true);
        let ended = m
            .call_tool(
                "input",
                json!({"launch_id":id,"operation":operation,"handle":state["held"]["handle"]}),
            )
            .await;
        assert!(ended["held"].is_null());
        assert_eq!(ended["boundary"]["t_us"], at);
        assert_eq!(text(&mut m, id, "#hover", false).await, "false");
        assert_eq!(
            text(&mut m, id, "#drops", false).await,
            if operation == "end" { "1" } else { "0" }
        );
        let seen: Value =
            serde_json::from_str(text(&mut m, id, "#events", false).await.as_str().unwrap())
                .unwrap();
        assert_eq!(
            seen.as_array()
                .unwrap()
                .iter()
                .filter(|e| e[0] == "dragend")
                .count(),
            0,
            "removed source has no bubbling dragend"
        );
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#button")},"then":{"type":"none"}})).await;
        assert_eq!(text(&mut m, id, "#clicks", false).await, "1");
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m
            .call_tool("debug", json!({"launch_id":id,"operation":"replay"}))
            .await;
        assert_eq!(replay["equal"], true, "{operation}: {replay}");
    }
}

/// A held HTML drag that leaves its document: into a frame of its origin it is dragged over and dropped
/// there; into a frame of another origin Chrome lets it enter nothing (crbug.com/59081), and the held
/// moves and the release still complete, the source ending its drag with no drop.
#[tokio::test]
async fn a_held_html_drag_crosses_into_frames_by_origin() {
    const ZONE: &str = "<!doctype html><style>body{margin:0}#dst{width:150px;height:100px}</style><div id=dst>drop here</div><script>const d=document.getElementById('dst');const P=(m)=>parent.postMessage(m,'*');for(const t of ['dragenter','dragover'])d.addEventListener(t,(e)=>{e.preventDefault();P(t)});d.addEventListener('drop',(e)=>{e.preventDefault();P('drop '+e.dataTransfer.getData('text/plain'))});</script>";
    let page = format!(r#"<!doctype html><style>body{{margin:0}}#src{{position:absolute;left:20px;top:20px;width:60px;height:40px}}iframe{{position:absolute;top:120px;width:200px;height:150px;border:0}}#same{{left:20px}}#other{{left:300px}}</style><div id=src draggable=true>drag me</div><iframe id=same srcdoc="{}"></iframe><iframe id=other src="https://zone.example.test/"></iframe><output id=log></output>
<script>const seen=[];const out=(m)=>{{seen.push(m);document.getElementById('log').textContent=seen.join(',')}};addEventListener('message',(e)=>out(e.data));const s=document.getElementById('src');s.addEventListener('dragstart',(e)=>{{e.dataTransfer.setData('text/plain','payload');out('dragstart')}});s.addEventListener('dragend',(e)=>out('dragend '+e.dataTransfer.dropEffect));</script>"#, ZONE.replace('"', "&quot;"));
    let mock = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = json!([mock("app", "https://app.example.test/", &page), mock("zone", "https://zone.example.test/", ZONE)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    // The frame's reports are messages, which reach the page after its own `dragend`.
    for (frame, x, seen) in [("#same", 100, &["dragstart", "dragend copy", "dragenter", "dragover", "drop payload"][..]), ("#other", 380, &["dragstart", "dragend none"][..])] {
        let launch = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
        let id = &launch["launch_id"];
        let mut state = m.call_tool("input", json!({"launch_id": id, "operation": "status"})).await;
        state = m.call_tool("input", json!({"launch_id": id, "operation": "begin", "target": css("#src"), "point": [40, 30], "boundary": state["boundary"]})).await;
        state = move_to(&mut m, id, &state, 90).await;
        assert_eq!(state["held"]["html_drag"], true, "{state}");
        for y in [160, 170] {
            state = m.call_tool("input", json!({"launch_id": id, "operation": "move", "handle": state["held"]["handle"], "point": [x, y], "boundary": state["boundary"]})).await;
        }
        let ended = m.call_tool("input", json!({"launch_id": id, "operation": "end", "handle": state["held"]["handle"]})).await;
        assert!(ended["held"].is_null(), "{ended}");
        m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 20}})).await;
        let log = text(&mut m, id, "#log", false).await;
        let mut got: Vec<&str> = log.as_str().unwrap().split(',').collect();
        got.dedup();
        assert_eq!(got, seen, "{frame}");
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}
