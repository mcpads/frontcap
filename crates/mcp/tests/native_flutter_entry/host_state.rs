use super::*;
use std::{path::Path, time::Duration};

async fn published(path:&Path) {
    tokio::time::timeout(Duration::from_secs(5),async {
        while !path.exists(){tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.unwrap();
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn flutter_host_state_preserves_foreground_admission_after_frontend_loss(){
    for android in [false,true] { lifecycle(false,android).await; }
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn flutter_host_state_recovers_only_original_evidence_after_recorder_loss(){
    for android in [false,true] { lifecycle(true,android).await; }
}
async fn lifecycle(recorder_dies:bool,android:bool){
    let workspace=support::workspace();let ws=workspace.root();std::fs::create_dir_all(&ws).unwrap();
    let worker=ws.join("worker.py");let http=ws.join("http.py");
    std::fs::write(&worker,include_str!("../../../recorder/tests/fixtures/native/flutter-worker.py")).unwrap();
    std::fs::write(&http,include_str!("../../../recorder/tests/fixtures/native/flutter-http.py")).unwrap();
    std::fs::write(ws.join("system-catalog.json"),include_str!("../../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let python=std::env::var("FRONTCAP_TEST_PYTHON").unwrap_or_else(|_|"/usr/bin/python3".into());
    let platform=if android{"android"}else{"ios"};
    let product=if android{
        let path=ws.join("client.apk");std::fs::write(&path,b"fixture APK").unwrap();
        json!({"product":path,"apk_sha256":frontcap_core::canonical::sha256_bytes_hex(b"fixture APK"),"package":"org.example.client","activity":"org.example.client.Main"})
    }else{standalone_product(&ws)};
    let mut settings=json!({"flutter":"/unused","project":ws,"platform":platform,"source_sha256":"b".repeat(64),"acquisition_timeout_s":5,"lifetime_s":60,"installed_app":product});
    if !android{settings["system_ui"]=system_products(&ws);}
    std::fs::write(ws.join(format!("flutter-{platform}.json")),json!({"session_executable":"/unused","acquisition_budget_ms":5000,
        "target":{"runtime":"flutter","platform":platform,"config":{"worker":{"python":python,"worker_script":worker,"config":settings},"http_script":http}}}).to_string()).unwrap();
    let mut mcp=Mcp::start_in(&ws).await;
    let launch=native(&mut mcp,json!({"type":"launch","runtime":"flutter","platform":platform,"request_id":"launch"})).await;
    let id=launch["launch_id"].as_str().unwrap();let owner=&launch["result"]["body"]["status"]["ready"]["owner"];
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("app_lifecycle")));
    let runtime=ws.join("runtimes").join(id);let target=runtime.join("native-session");let journal=runtime.join("journal.jsonl");
    let old=native(&mut mcp,json!({"type":"observe","launch_id":id,"request_id":"before-home"})).await;
    let observation=&old["result"]["body"]["outcome"]["completion"]["surface"]["observation"];
    let background=json!({"type":"set_app_state","launch_id":id,"request_id":"background","generation":owner["generation"],"state":"background","budget_ms":5000});
    std::fs::write(ws.join("hold-state"),b"").unwrap();
    let unread=mcp.send("tools/call",json!({"name":"native","arguments":{"op":background}})).await;
    published(&ws.join("state-published")).await;
    // The fixture retains native effect evidence before its worker reply. This is not the
    // recorder's feature-gated native_after_effect fault point; both boundaries stay distinct.
    if recorder_dies{frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap();}
    mcp.kill().await;drop(unread);
    let original_journal=std::fs::read(&journal).unwrap();let history=target.join("app-state/history.json");let original_history=std::fs::read(&history).unwrap();
    let binding:Value=serde_json::from_slice(&original_history).unwrap();let number=binding["entries"][0]["command_number"].as_u64().unwrap();
    let evidence=target.join(format!("session/worker/commands/{number}/app-state"));
    let lookup=json!({"type":"result","launch_id":id,"request_id":"background"});let mut mcp=Mcp::start_in(&ws).await;
    if recorder_dies{
        let path=evidence.join("after.json");let original=std::fs::read(&path).unwrap();std::fs::remove_file(&path).unwrap();
        assert_eq!(native(&mut mcp,lookup.clone()).await["result"]["result"],json!({"state":"pending","dispatched":true}));
        let mut changed:Value=serde_json::from_slice(&original).unwrap();
        changed["process"][if android{"start_ticks"}else{"birth"}]=if android{json!(999)}else{json!("replaced process")};
        std::fs::write(&path,changed.to_string()).unwrap();assert!(mcp.try_tool("native",json!({"op":lookup})).await.0);
        std::fs::write(&path,original).unwrap();
        let raw=if android{evidence.join("window-after.txt")}else{
            let native:Value=serde_json::from_slice(&std::fs::read(evidence.join("native.json")).unwrap()).unwrap();
            target.join("session/worker/system-ui/client/receipts").join(format!("{}.json",native["completion"]["receipt_sha256"].as_str().unwrap()))
        };
        let original=std::fs::read(&raw).unwrap();std::fs::write(&raw,b"changed source receipt").unwrap();
        assert!(mcp.try_tool("native",json!({"op":lookup})).await.0);std::fs::write(raw,original).unwrap();
    }
    std::fs::remove_file(ws.join("hold-state")).unwrap();
    let saved=tokio::time::timeout(Duration::from_secs(5),async{
        loop{let value=native(&mut mcp,lookup.clone()).await;if value["result"]["result"]["state"]=="completed"{break value;}
            tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.unwrap();
    let transition=&saved["result"]["result"]["answer"]["body"]["body"]["outcome"]["transition"];
    assert_eq!(&transition["owner"],owner);assert_eq!(transition["process_before"],transition["process_after"]);
    assert_eq!(transition["requested"],"background");
    if recorder_dies{
        published(&super::worker_result(&runtime)).await;
        assert_eq!(std::fs::read(&journal).unwrap(),original_journal);assert_eq!(std::fs::read(&history).unwrap(),original_history);
        assert!(mcp.try_tool("native",json!({"op":{"type":"reattach","launch_id":id,"request_id":"no-adoption"}})).await.0);
    }else{
        native(&mut mcp,json!({"type":"reattach","launch_id":id,"request_id":"attach"})).await;
        native(&mut mcp,background).await;
        let before=std::fs::read(&journal).unwrap();
        for mut operation in [json!({"type":"observe"}),json!({"type":"read_clock"}),json!({"type":"advance","by_ms":1}),
            json!({"type":"restart_flutter","generation":owner["generation"]}),
            json!({"type":"dispatch_url","generation":owner["generation"],"url":"client-test://item/1"}),
            json!({"type":"set_app_state","generation":owner["generation"],"state":"background"}),
            json!({"type":"set_app_state","generation":"c".repeat(48),"state":"foreground"})]{
            operation["launch_id"]=json!(id);operation["request_id"]=json!("rejected");assert!(mcp.try_tool("native",json!({"op":operation})).await.0);
        }
        assert_eq!(std::fs::read(&journal).unwrap(),before);
        if !android{native(&mut mcp,json!({"type":"observe_system_alerts","launch_id":id,"request_id":"background-system"})).await;}
        let returned=native(&mut mcp,json!({"type":"set_app_state","launch_id":id,"request_id":"foreground","generation":owner["generation"],"state":"foreground"})).await;
        assert_eq!(&returned["result"]["body"]["outcome"]["transition"]["owner"],owner);
        assert!(mcp.try_tool("native",json!({"op":{"type":"flutter","launch_id":id,"request_id":"stale-target","generation":owner["generation"],
            "operation":{"operation":"tap","observation":observation,"target":"s1"}}})).await.0);
        let fresh=native(&mut mcp,json!({"type":"observe","launch_id":id,"request_id":"returned-observe"})).await;
        native(&mut mcp,lookup.clone()).await;
        native(&mut mcp,json!({"type":"flutter","launch_id":id,"request_id":"new-input","generation":owner["generation"],"operation":{"operation":"tap",
            "observation":fresh["result"]["body"]["outcome"]["completion"]["surface"]["observation"],"target":"s1"}})).await;
        native(&mut mcp,json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    }
    assert_eq!(native(&mut mcp,lookup).await,saved);
    assert_eq!(std::fs::read_to_string(target.join("session/worker/state-effects.jsonl")).unwrap().lines().count(),if recorder_dies{1}else{2});
    mcp.kill().await;
}
