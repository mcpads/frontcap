mod support;
use serde_json::{Value, json};
use std::time::Duration;

async fn launch(m: &mut support::Mcp, addr: std::net::SocketAddr) -> Value {
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/dialog-flow.html"),"then":{"type":"none"}})).await;
    let id = &launched["launch_id"];
    m.call_tool("session", json!({"launch_id":id,"operation":"dialog_rules","rules":[{"answer":"hold"}]})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
    launched
}

fn click(id: &Value, selector: &str, request: &str) -> Value {
    json!({"launch_id":id,"request_id":request,"action":{"type":"click","target":{"by":"css","selector":selector}},"then":{"type":"none"}})
}

async fn terminal(m: &mut support::Mcp, id: &Value, parent: &Value) -> Value {
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":parent})).await;
            if result["result"]["state"] == "completed" {
                let status = m.call_tool("status", json!({"launch_id":id})).await;
                if status["running"].is_null() { return result["result"]["answer"].clone(); }
            }
            tokio::task::yield_now().await;
        }
    }).await;
    match completed {
        Ok(result) => result,
        Err(_) => {
            let status = m.call_tool("status", json!({"launch_id":id})).await;
            panic!("parent never reached a terminal idle boundary: {status}");
        }
    }
}

async fn dialog(m: &mut support::Mcp, id: &Value, message: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = m.call_tool("status", json!({"launch_id":id})).await;
            if status["suspension"]["dialog"]["message"] == message { return status["suspension"]["dialog"].clone(); }
            tokio::task::yield_now().await;
        }
    }).await.expect("expected dialog did not appear")
}

fn answer(id: &Value, parent: &Value, dialog: &Value, request: &str, accept: bool) -> Value {
    json!({"launch_id":id,"operation":"answer_dialog","operation_id":parent,"dialog_id":dialog["id"],"request_id":request,"accept":accept})
}

async fn arm_wait(m: &mut support::Mcp, id: &Value) -> Value {
    m.call_tool("act", click(id, "#schedule", "schedule")).await;
    let result = m.call_tool("wait", json!({"launch_id":id,"request_id":"predicate","op":{"type":"until",
        "predicate":{"is":"attached","target":{"by":"css","selector":"body[data-completed=yes]"}},"max_ms":100}})).await;
    assert_eq!(result["state"], "suspended");
    assert_eq!(result["operation_id"], "predicate");
    assert_eq!((result["clock"]["state"].clone(), result["clock"]["t_us"].clone()), (json!("blocked"), json!(60000)));
    let status = m.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(status["clock"], result["clock"], "status must use the native blocked instant");
    assert_eq!(status["suspension"]["request_id"], result["operation_id"]);
    assert_eq!(m.call_tool("wait", json!({"launch_id":id,"op":{"type":"now"}})).await, result["clock"]);
    result
}

#[tokio::test]
async fn held_action_answers_are_correlated_and_replayable() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, addr).await;
    let id = &g["launch_id"];
    let args = click(id, "#delete", "delete");
    let suspended = m.call_tool("act", args.clone()).await;
    assert_eq!(suspended["state"], "suspended");
    assert_eq!(suspended["launch_id"], *id);
    assert_eq!((suspended["clock"]["state"].clone(), suspended["clock"]["t_us"].clone()), (json!("blocked"), json!(50000)));
    assert!(suspended.get("effects").is_none(), "unfinished input must not claim terminal effects");
    let parent = &suspended["operation_id"];
    let first = &suspended["dialog"];
    assert_eq!(first["message"], "Delete draft?");
    assert_eq!(first["source"]["frame_path"], json!([]));
    let pending = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":parent})).await;
    assert_eq!(pending["result"]["state"], "pending");
    let (failed, busy) = m.try_tool("observe", json!({"launch_id":id,"op":{"type":"snapshot"}})).await;
    assert!(failed && busy["code"] == "busy");
    let (failed, busy) = m.try_tool("act", args.clone()).await;
    assert!(failed && busy["code"] == "busy");
    let first_answer = answer(id, parent, first, "z-first", true);
    for (field, value, code) in [("lease",json!("0".repeat(32)),"lease_required"),("dialog_id",json!("dialog-999"),"no_such_hold"),("prompt_text",json!("invalid"),"invalid_arguments")] {
        let mut bad = first_answer.clone(); bad[field] = value;
        let (failed, error) = m.try_tool("session", bad).await;
        assert!(failed && error["code"] == code, "{error}");
    }
    let now = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"now"}})).await;
    assert_eq!(now, suspended["clock"], "invalid authority must not drop the actual controller lease");
    let receipt = m.call_tool("session", first_answer.clone()).await;
    assert_eq!(receipt["receipt"]["dialog"], *first);
    // The answer reports what the parent did next: here it suspended on the next dialog.
    assert_eq!(receipt["parent"]["state"], "suspended", "{receipt}");
    assert_eq!(receipt["parent"]["suspension"]["dialog"]["message"], "Archive label", "{receipt}");
    let second = dialog(&mut m, id, "Archive label").await;
    assert_eq!(m.call_tool("session", first_answer.clone()).await, receipt, "a retry must not answer the next dialog");
    let mut conflict = first_answer; conflict["accept"] = json!(false);
    let (failed, error) = m.try_tool("session", conflict).await;
    assert!(failed && error["code"] == "request_id_conflict", "{error}");
    let mut reply = answer(id, parent, &second, "a-second", true);
    reply["prompt_text"] = json!("한글 🍎");
    let last = m.call_tool("session", reply).await;
    assert_eq!(last["parent"]["state"], "completed", "{last}");
    let result = terminal(&mut m, id, parent).await;
    assert_eq!(result["answer"], "ok", "{result}");
    let done = m.call_tool("act", args).await;
    assert_eq!(done["t_before_us"], 50000);
    assert_eq!(done["t_after_us"], 50000);
    assert_eq!(done["effects"]["dialogs"].as_array().unwrap().len(), 2);
    let output = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#answers"},"props":["text"]}})).await;
    assert_eq!(output["values"]["text"], "[1,true,\"한글 🍎\"]");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn predicate_wait_cleans_up_after_release_disconnect_and_stop() {
    for mode in ["release", "disconnect", "stop"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let g = launch(&mut m, support::serve()).await;
        let id = &g["launch_id"];
        let suspended = arm_wait(&mut m, id).await;
        if mode == "stop" {
            let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
            assert_eq!(stopped["finalized"], true, "{stopped}");
            let saved = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":suspended["operation_id"]})).await;
            assert_eq!(saved["result"]["state"], "completed");
            assert_eq!(saved["result"]["answer"]["body"]["outcome"]["t_us"], 60000);
            continue;
        }
        if mode == "release" { m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await; }
        else { m.kill().await; m = support::Mcp::start_in(&ws.root()).await; }
        let attached = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (failed, r) = m.try_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
                if !failed { break r; }
                assert!(r["code"] == "busy" || r["code"] == "lease_held", "{r}");
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!((attached["status"]["clock"]["state"].clone(), attached["status"]["clock"]["t_us"].clone()), (json!("frozen"), json!(60000)));
        assert_eq!(attached["status"]["watches"], json!([]));
        let done = terminal(&mut m, id, &suspended["operation_id"]).await;
        assert_eq!(done["body"]["outcome"]["t_us"], 60000);
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{mode}: {replay}");
    }
}

#[tokio::test]
async fn input_and_evaluation_return_suspension_without_losing_their_results() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, support::serve()).await;
    let id = &g["launch_id"];
    m.call_tool("act", click(id, "#key", "focus")).await;
    let suspended = m.call_tool("input", json!({"launch_id":id,"operation":"key_down","key":"x"})).await;
    assert_eq!(suspended["state"], "suspended");
    assert!(suspended["operation_id"].as_str().unwrap().starts_with("mcp-"));
    m.call_tool("session", answer(id, &suspended["operation_id"], &suspended["dialog"], "key-answer", false)).await;
    let result = terminal(&mut m, id, &suspended["operation_id"]).await;
    assert_eq!(result["answer"], "ok");
    let input = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    assert_eq!(input["keys"], json!(["x"]));
    m.call_tool("input", json!({"launch_id":id,"operation":"key_up","key":"x"})).await;
    let suspended = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"prompt('Evaluate prompt', 'default')"})).await;
    assert_eq!(suspended["state"], "suspended");
    m.call_tool("session", answer(id, &suspended["operation_id"], &suspended["dialog"], "eval-answer", true)).await;
    let result = terminal(&mut m, id, &suspended["operation_id"]).await;
    assert_eq!(result["body"]["outcome"]["value"], "default");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn dismissed_beforeunload_is_a_definite_navigation_result() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let addr = support::serve();
    let g = launch(&mut m, addr).await;
    let id = &g["launch_id"];
    m.call_tool("act", click(id, "#dirty", "dirty")).await;
    let navigation = json!({"launch_id":id,"request_id":"leave","action":{"type":"navigate","url":format!("http://{addr}/toast.html")},"then":{"type":"none"}});
    let suspended = m.call_tool("act", navigation.clone()).await;
    assert_eq!(suspended["state"], "suspended");
    assert_eq!(suspended["dialog"]["kind"], "beforeunload");
    m.call_tool("session", answer(id, &suspended["operation_id"], &suspended["dialog"], "stay", false)).await;
    let result = terminal(&mut m, id, &suspended["operation_id"]).await;
    assert_eq!(result["answer"], "err");
    assert_eq!(result["body"]["code"], "navigation_cancelled");
    let (failed, error) = m.try_tool("act", navigation).await;
    assert!(failed && error["code"] == "navigation_cancelled", "{error}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn reattach_after_recorder_loss_resolves_the_suspended_parent_in_place() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, support::serve()).await;
    let id = &g["launch_id"];
    let suspended = arm_wait(&mut m, id).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let before = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    before.recorder.kill().unwrap();
    assert!(before.browser.as_ref().unwrap().is_alive());
    let saved = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":suspended["operation_id"]})).await;
    assert_eq!(saved["result"]["state"], "pending", "dead-recorder lookup must not execute recovery");
    let attached = m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert_ne!(attached["lease"], g["lease"]);
    assert_eq!((attached["status"]["clock"]["state"].clone(), attached["status"]["clock"]["t_us"].clone()), (json!("frozen"), json!(60000)));
    assert_eq!(attached["status"]["watches"], json!([]));
    assert!(attached["status"]["suspension"].is_null());
    let after = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    assert_eq!(after.browser, before.browser, "recovery replaced the living browser");
    let done = terminal(&mut m, id, &suspended["operation_id"]).await;
    assert_eq!(done["body"]["outcome"]["t_us"], 60000);
    m.call_tool("stop", json!({"launch_id":id})).await;
    let (failed, replay) = m.try_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert!(failed && replay["code"] == "not_replayable", "recovery must retain its fault boundary: {replay}");
}

#[tokio::test]
async fn stop_terminates_a_failed_browser_without_recovery_or_finalization() {
    use frontcap_recorder::runtime;
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, addr).await;
    let id = &g["launch_id"];
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let before = runtime::load_runtime(&dir).unwrap().unwrap();
    before.recorder.kill().unwrap();
    runtime::mark_failed(&dir, "retained failure before termination").unwrap();
    let failed = std::fs::read(runtime::runtime_path(&dir)).unwrap();
    let control = std::fs::read(runtime::control_path(&dir)).unwrap();
    let args = json!({"launch_id":id,"request_id":"terminate-failed"});
    let stopped = m.call_tool("stop", args.clone()).await;
    assert_eq!(stopped["finalized"], false);
    assert!(!before.browser.unwrap().is_alive());
    assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), failed);
    assert_eq!(std::fs::read(runtime::control_path(&dir)).unwrap(), control);
    assert!(!std::path::Path::new(stopped["bundle"].as_str().unwrap()).join("manifest.json").exists());
    assert_eq!(m.call_tool("stop", args).await, stopped);
    let result = m.call_tool("session", json!({"launch_id":id,"operation":"result","operation_id":"terminate-failed"})).await;
    assert_eq!(result["result"]["state"], "completed");
    let (error, value) = m.try_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert!(error && value["code"] == "recorder_fault", "{value}");
    let finalized = m.call_tool("debug", json!({"launch_id":id,"operation":"finalize"})).await;
    assert_eq!(finalized["finalization"]["state"], "finalized");
    assert_eq!(finalized["replayable"], false);
    assert_eq!(finalized["determinism"]["level"], "none");
    assert_eq!(finalized["launch"], *id);
    assert!(finalized["engine"].as_str().unwrap().contains("HeadlessChrome"));
    let snapshot = finalized["recovery_snapshot"].as_str().unwrap();
    assert!(finalized["members"].get(format!("{snapshot}/journal.jsonl")).is_some());
    assert_eq!(m.call_tool("debug", json!({"launch_id":id,"operation":"finalize"})).await, finalized);
    assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), failed);
}

#[tokio::test]
async fn next_dialog_temporarily_overrides_automatic_answers_and_replays() {
    for suffix in ["", "?frame", "?alert", "?frame&alert"] { run_dialog_stop(suffix).await; }
}

async fn run_dialog_stop(suffix: &str) {
    let first_answer = if suffix.contains("alert") { json!("alert") } else { json!(false) };
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let addr = support::serve();
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/dialog-stop.html{suffix}"),"then":{"type":"none"}})).await;
    let id = &launched["launch_id"];
    m.call_tool("session", json!({"launch_id":id,"operation":"dialog_rules","rules":[{"answer":"accept"}]})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let args = json!({"launch_id":id,"request_id":"next-dialog","op":{"type":"next","event":"dialog","max_ms":199}});
    let suspended = m.call_tool("wait", args.clone()).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    assert_eq!((suspended["clock"]["state"].clone(), suspended["clock"]["t_us"].clone()), (json!("blocked"), json!(250000)));
    assert_eq!(suspended["dialog"]["message"], "Save draft?");
    assert_eq!(suspended["dialog"]["source"]["frame_path"], if suffix.contains("frame") { json!([0]) } else { json!([]) });
    m.call_tool("session", answer(id, &suspended["operation_id"], &suspended["dialog"], "dismiss-save", false)).await;
    terminal(&mut m, id, &suspended["operation_id"]).await;
    let done = m.call_tool("wait", args).await;
    assert_eq!(done["outcome"]["type"], "event", "{done}");
    assert_eq!(done["outcome"]["event"], "dialog");
    assert_eq!(done["t_us"], 250000, "{done}");
    assert_eq!(done["outcome"]["triggers"][0]["dialog_id"], suspended["dialog"]["id"]);
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({answers,later,now:Date.now()-946684800000})"})).await;
    assert_eq!(observed["value"], json!({"answers":[first_answer],"later":false,"now":250}));
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":200}})).await;
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"answers"})).await;
    assert_eq!(observed["value"], json!([first_answer,true]), "the configured automatic rule must be restored");
    let args = json!({"launch_id":id,"request_id":"next-prompt","op":{"type":"next","event":"dialog","max_ms":500}});
    let suspended = m.call_tool("wait", args.clone()).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    assert_eq!(suspended["clock"]["t_us"], 700000);
    let mut response = answer(id, &suspended["operation_id"], &suspended["dialog"], "name-draft", true);
    response["prompt_text"] = json!("한글 🍎");
    m.call_tool("session", response).await;
    terminal(&mut m, id, &suspended["operation_id"]).await;
    let done = m.call_tool("wait", args).await;
    assert_eq!(done["t_us"], 700000, "{done}");
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"answers"})).await;
    assert_eq!(observed["value"], json!([first_answer,true,"한글 🍎"]));
    let timeout = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"dialog","max_ms":20}})).await;
    assert_eq!(timeout["outcome"]["type"], "event_timed_out");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn next_beforeunload_keeps_cancelled_and_accepted_navigation_controlled() {
    for accept in [false, true] {
        eprintln!("beforeunload accept={accept}");
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let addr = support::serve();
        let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/dialog-navigation-stop.html"),"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        m.call_tool("act", click(id, "#edit", "edit")).await;
        let args = json!({"launch_id":id,"request_id":"beforeunload-stop","op":{"type":"next","event":"dialog","max_ms":1000}});
        let suspended = m.call_tool("wait", args.clone()).await;
        assert_eq!(suspended["state"], "suspended", "{accept}: {suspended}");
        assert_eq!(suspended["dialog"]["kind"], "beforeunload");
        assert_eq!(suspended["clock"]["t_us"], 250000);
        m.call_tool("session", answer(id, &suspended["operation_id"], &suspended["dialog"], "leave-draft", accept)).await;
        terminal(&mut m, id, &suspended["operation_id"]).await;
        let done = m.call_tool("wait", args).await;
        assert_eq!(done["outcome"]["type"], "event", "{accept}: {done}");
        assert_eq!(done["t_us"], 250000, "{accept}: {done}");
        let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({path:location.pathname,later:window.later===true,started:window.appStarted===true,now:Date.now()-946684800000})"})).await;
        assert_eq!(observed["value"], json!({"path":if accept {"/dialog-arrived.html"} else {"/dialog-navigation-stop.html"},"later":false,"started":false,"now":250}), "{accept}: {observed}");
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        let after = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({later:window.later===true,started:window.appStarted===true})"})).await;
        assert_eq!(after["value"], json!({"later":!accept,"started":accept}));
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{accept}: {replay}");
    }
}

#[tokio::test]
async fn next_dialog_in_a_cross_origin_frame_preserves_source_and_replays() {
    for prompt in [None, Some("한글 🍎")] {
        run_cross_origin_dialogs(prompt).await;
    }
}

async fn run_cross_origin_dialogs(prompt: Option<&str>) {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let parent = "https://draft.example.test/start";
    let child = "https://editor.other.test/frame";
    let main = format!("<script>window.answers=[];window.later=false;addEventListener('message',e=>{{if(e.origin!=='https://editor.other.test')return;answers.push(e.data);if(e.data?.save===false){{answers.push(confirm('Parent archive?'));e.source.postMessage('follow-up',e.origin)}}}});setTimeout(()=>{{later=true}},251)</script><iframe src='{child}'></iframe>");
    let frame = "<script>addEventListener('message',e=>{if(e.data==='follow-up')parent.postMessage({followup:prompt('Automatic child label','Automatic label')},'*')});setTimeout(()=>{alert('Draft ready');const save=confirm('Cross-origin save?');const label=prompt('Cross-origin label','Draft');parent.postMessage({save,label},'*')},250);setTimeout(()=>parent.postMessage(confirm('Automatic again'),'*'),400)</script>";
    let rules: Vec<_> = [("main",parent,main.as_str()),("child",child,frame)].into_iter().map(|(name,url,body)| json!({
        "id":name,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}
    })).collect();
    let launch = m.call_tool("launch", json!({"url":parent,"rules":rules,"then":{"type":"none"}})).await;
    let id = &launch["launch_id"];
    m.call_tool("session", json!({"launch_id":id,"operation":"dialog_rules","rules":[{"answer":"accept"}]})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let args = json!({"launch_id":id,"request_id":"child-dialog","op":{"type":"next","event":"dialog","max_ms":1000}});
    let suspended = m.call_tool("wait", args.clone()).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    assert_eq!(suspended["clock"]["t_us"], 250000);
    let mut previous = None;
    for (kind, message, accept) in [("alert", "Draft ready", true), ("confirm", "Cross-origin save?", false), ("prompt", "Cross-origin label", prompt.is_some())] {
        let opening = dialog(&mut m, id, message).await;
        assert_eq!(opening["kind"], kind);
        assert_eq!(opening["t_us"], 250000);
        assert_eq!(opening["source"], json!({"frame_path":[0],"frame_urls":[parent,child]}));
        if let Some((request, receipt)) = previous.take() {
            assert_eq!(m.call_tool("session", request).await, receipt, "a previous answer must not consume the next native opening");
        }
        let mut response = answer(id, &suspended["operation_id"], &opening, &format!("answer-{kind}"), accept);
        if kind == "prompt" {
            assert_eq!(opening["default_prompt"], "Draft");
            if let Some(text) = prompt { response["prompt_text"] = json!(text); }
        }
        let receipt = m.call_tool("session", response.clone()).await;
        previous = Some((response, receipt));
    }
    terminal(&mut m, id, &suspended["operation_id"]).await;
    let done = m.call_tool("wait", args).await;
    assert_eq!(done["outcome"]["type"], "event");
    assert_eq!(done["t_us"], 250000, "{done}");
    let triggers = done["outcome"]["triggers"].as_array().unwrap();
    assert_eq!(triggers.iter().map(|t| t["dialog_kind"].as_str().unwrap()).collect::<Vec<_>>(), ["alert", "confirm", "prompt"]);
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({answers,later,now:Date.now()-946684800000})"})).await;
    // The child's answer queues a message to its parent; that receiver is a later task.
    assert_eq!(observed["value"], json!({"answers":[],"later":false,"now":250}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let repeated = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({answers,later,now:Date.now()-946684800000})"})).await;
    assert_eq!(repeated, observed, "a host delay must not deliver the queued cross-origin message");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":200}})).await;
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"answers"})).await;
    assert_eq!(observed["value"], json!([{"save":false,"label":prompt},true,{"followup":"Automatic label"},true]));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}
