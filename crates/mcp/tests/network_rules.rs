mod support;
use serde_json::{Value, json};

#[tokio::test]
async fn effective_rules_are_readable_without_a_lease_and_survive_recorder_replacement() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut owner = support::Mcp::start_in(&ws.root()).await;
    let launched = owner.call_tool("launch", json!({"url":format!("http://{addr}/count.html"),
        "rules":[{"id":"held-api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]})).await;
    let id = &launched["launch_id"];
    let before = owner.call_tool("status", json!({"launch_id":id})).await;
    let mut reader = support::Mcp::start_in(&ws.root()).await;
    let rules = reader.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(rules["revision"], 0);
    let ids: Vec<_> = rules["rules"].as_array().unwrap().iter().map(|rule| rule["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["held-api", "default-passthrough-0", "default-block"]);
    assert_eq!(rules["rules"][1]["match"]["url"], format!("http://{addr}/*"));
    assert_eq!(rules["rules"][2]["action"]["type"], "fail");
    let after = reader.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(after["ownership"], "other");
    assert_eq!(after["t_us"], before["t_us"]);
    assert_eq!(after["observation_seq"], before["observation_seq"]);
    assert_eq!(reader.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await, rules);
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let runtime = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    runtime.recorder.kill().unwrap();
    owner.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let restored = owner.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(restored["revision"], rules["revision"]);
    assert_eq!(restored["rules"], rules["rules"]);
    assert_eq!(restored["boundary"]["t_us"], rules["boundary"]["t_us"]);
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser, runtime.browser);
    owner.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn large_effective_rules_remain_readable_from_immutable_pages_after_stop() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut owner = support::Mcp::start_in(&ws.root()).await;
    let body = "한글🙂".repeat(6000);
    let launched = owner.call_tool("launch", json!({"url":format!("http://{addr}/count.html"),
        "rules":[{"id":"large","match":{"url":"https://api.example.test/large"},"action":{"type":"mock","status":200,"body":body}}]})).await;
    let id = &launched["launch_id"];
    let summary = owner.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(summary["rules"][0]["action"]["body_bytes"], body.len(), "{summary}");
    assert!(summary["rules"][0]["action"].get("body").is_none());
    let mut page = owner.call_tool("network", json!({"launch_id":id,"operation":"list_rules","bodies":true})).await;
    assert!(page["result_id"].is_string(), "{page}");
    owner.call_tool("stop", json!({"launch_id":id})).await;
    let mut result = String::new();
    loop {
        assert!(page.to_string().len() <= 16 * 1024);
        assert_eq!(page["offset"].as_u64().unwrap() as usize, result.len());
        result.push_str(page["json_fragment"].as_str().unwrap());
        let Some(offset) = page["next_offset"].as_u64() else { break };
        page = owner.call_tool("debug", json!({"launch_id":id,"operation":"result","result_id":page["result_id"],"offset":offset})).await;
    }
    let snapshot: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(snapshot["revision"], 0);
    assert_eq!(snapshot["rules"][0]["action"]["body"], body);
    assert_eq!(snapshot["rules"][2]["id"], "default-block");
}

#[tokio::test]
async fn replacing_rules_preserves_holds_and_replays() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let old = json!({"id":"api","match":{"url":format!("http://{addr}/api/*")},"action":{"type":"hold"}});
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html"),"rules":[old]})).await;
    let id = &launched["launch_id"];
    let before = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let (failed, error) = m.try_tool("network", json!({"launch_id":id,"operation":"set_rules","expected_revision":0,"boundary":before["boundary"],"rules":[old,old]})).await;
    assert!(failed && error["code"]=="invalid_arguments", "{error}");
    assert_eq!(m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await,before);
    let click = json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}});
    m.call_tool("act", click.clone()).await;
    let held = m.call_tool("network", json!({"launch_id":id,"operation":"list_held"})).await;
    assert_eq!(held["held"].as_array().unwrap().len(),1);
    assert_eq!(held["held"][0]["rules_revision"],0);
    let current = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let args = json!({"launch_id":id,"request_id":"replace","operation":"set_rules","expected_revision":0,"boundary":current["boundary"],
        "rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},"action":{"type":"mock","status":200,"body":"new","latency_ms":20}}]});
    let receipt = m.call_tool("network", args.clone()).await;
    assert_eq!(receipt["revision"],1);
    assert_eq!(m.call_tool("network",args.clone()).await,receipt);
    let mut conflict = args.clone(); conflict["rules"][0]["action"]["body"] = json!("conflict");
    let (failed,error) = m.try_tool("network",conflict).await;
    assert!(failed && error["code"]=="request_id_conflict", "{error}");
    let after = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(after["boundary"]["t_us"],current["boundary"]["t_us"]);
    assert_eq!(m.call_tool("network",json!({"launch_id":id,"operation":"list_held"})).await,held);
    m.call_tool("act", click).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    assert_eq!(m.call_tool("observe",query.clone()).await["values"]["text"],"[\"new\"]");
    let answered = m.call_tool("network",json!({"launch_id":id,"operation":"respond","ordinal":held["held"][0]["ordinal"],"response":{"status":200,"body":"old"}})).await;
    // The answer names the boundary a following rule edit checks, with no observation in between.
    let added = m.call_tool("network",json!({"launch_id":id,"operation":"add_rule","expected_revision":1,"boundary":answered["boundary"],
        "rule":{"id":"later","match":{"url":format!("http://{addr}/later")},"action":{"type":"mock","status":204}}})).await;
    assert_eq!(added["revision"],2,"{answered} {added}");
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    assert_eq!(m.call_tool("observe",query).await["values"]["text"],"[\"new\",\"old\"]");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn installed_cassette_replays_from_bundle_after_original_is_removed() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    std::fs::create_dir_all(ws.root()).unwrap();
    let cassette = ws.root().join("original.jsonl");
    let line = json!({"key":{"method":"GET","url_normalized":format!("http://{addr}/api/value?n=1")},
        "response":{"status":200,"headers":{},"body_base64":"Y2Fzc2V0dGU="},
        "latency":{"virtual_ms":20,"wall_ms":0}});
    std::fs::write(&cassette, format!("{line}\n")).unwrap();
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html")})).await;
    let id = &launched["launch_id"];
    let before = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let receipt = m.call_tool("network", json!({"launch_id":id,"operation":"set_rules",
        "expected_revision":0,"boundary":before["boundary"],"rules":[{"id":"api",
        "match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":cassette,"after_last":"fail"}}]})).await;
    assert_eq!(receipt["revision"], 1);
    std::fs::remove_file(&cassette).unwrap();
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let observed = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}})).await;
    assert_eq!(observed["values"]["text"], "[\"cassette\"]");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
    let relocated = support::serve();
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay", "url":format!("http://{relocated}/network-rules.html")})).await;
    assert_eq!(replay["equal"], true, "moved-origin replay: {replay}");
}

#[tokio::test]
async fn a_later_record_installation_replays_without_normalizing_other_versions() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mock = json!({"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"mock","status":200,"body":"mock"}});
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html"),"rules":[mock]})).await;
    let id = &launched["launch_id"];
    let click = json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}});
    m.call_tool("act", click.clone()).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let before = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let receipt = m.call_tool("network", json!({"launch_id":id,"operation":"set_rules",
        "expected_revision":0,"boundary":before["boundary"],"rules":[{"id":"api",
        "match":{"url":format!("http://{addr}/api/*")},"action":{"type":"record","cassette":"later"}}]})).await;
    assert_eq!(receipt["revision"],1);
    m.call_tool("act", click).await;
    // Evaluate outcomes include their value digest in replay: both executions must preserve
    // the stopped page, regardless of when the source server finishes its response.
    let frozen = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate",
        "expression":"document.querySelector('#responses').textContent"})).await;
    assert_eq!(frozen["value"], "[\"mock\"]");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let observed = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}})).await;
    assert_eq!(observed["values"]["text"], "[\"mock\",\"not found\"]");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn refused_rule_inputs_do_not_depend_on_later_host_file_contents() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/network-rules.html")})).await;
    let id = &launched["launch_id"];
    let snapshot = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    let path = ws.root().join("absent.jsonl");
    let args = json!({"launch_id":id,"operation":"set_rules","expected_revision":0,
        "boundary":snapshot["boundary"],"rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":path,"after_last":"fail"}}]});
    let (failed,error) = m.try_tool("network",args.clone()).await;
    assert!(failed && error["code"]=="invalid_arguments","{error}");
    // That original path now names a valid empty cassette. Replaying the refused command must not
    // install it just because the recording host's filesystem changed.
    std::fs::write(&path,"").unwrap();
    let mut duplicate = args;
    let repeated = duplicate["rules"][0].clone();
    duplicate["rules"].as_array_mut().unwrap().push(repeated);
    let (failed,error) = m.try_tool("network",duplicate).await;
    assert!(failed && error["code"]=="invalid_arguments","{error}");
    assert_eq!(m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await,snapshot);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

// External completion may arrive during deliberation, but its page callback requires progress.
#[tokio::test]
async fn recorded_http_response_waits_for_explicit_progress() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (issued_tx, issued_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (answered_tx, answered_rx) = tokio::sync::oneshot::channel();
    let server_thread = std::thread::spawn(move || {
        let page = server.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
        page.respond(tiny_http::Response::from_string(include_str!("fixtures/network-rules.html"))
            .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap())).unwrap();
        let request = server.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
        issued_tx.send(request.url().to_string()).unwrap();
        release_rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        request.respond(tiny_http::Response::from_string("released")).unwrap();
        answered_tx.send(()).unwrap();
    });
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/network-rules.html"),
        "rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"record","cassette":"frozen"}}]})).await;
    let id = &launched["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    let issued = tokio::time::timeout(std::time::Duration::from_secs(5), issued_rx).await.unwrap().unwrap();
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    let before = m.call_tool("observe",query.clone()).await;
    let stopped = m.call_tool("status",json!({"launch_id":id})).await;
    release_tx.send(()).unwrap();
    answered_rx.await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after = m.call_tool("observe",query.clone()).await;
    let still_stopped = m.call_tool("status",json!({"launch_id":id})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let progressed = m.call_tool("observe",query).await;
    m.call_tool("stop",json!({"launch_id":id})).await;
    server_thread.join().unwrap();
    assert_eq!(issued, "/api/value?n=1");
    assert!(stopped["t_us"].is_u64(), "{stopped}");
    assert_eq!(still_stopped["t_us"], stopped["t_us"]);
    assert_eq!(progressed["values"]["text"], "[\"released\"]");
    assert_eq!(before["values"]["text"], "[]");
    assert_eq!(after["values"]["text"], before["values"]["text"], "host response executed while stopped");
}


#[tokio::test]
async fn zero_latency_recorded_response_stays_pending_across_recorder_replacement() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    std::fs::create_dir_all(ws.root()).unwrap();
    let cassette = ws.root().join("progress.jsonl");
    let line = json!({"key":{"method":"GET","url_normalized":format!("http://{addr}/api/value?n=1")},
        "response":{"status":200,"headers":{},"body_base64":"cmVjb3JkZWQ="},
        "latency":{"virtual_ms":0,"wall_ms":0},"requires_progress":true});
    std::fs::write(&cassette,format!("{line}\n")).unwrap();
    let launch = m.call_tool("launch",json!({"url":format!("http://{addr}/network-rules.html")})).await;
    let id = &launch["launch_id"];
    let snapshot = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network",json!({"launch_id":id,"operation":"set_rules","expected_revision":0,
        "boundary":snapshot["boundary"],"rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":cassette,"after_last":"fail"}}]})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    assert_eq!(m.call_tool("observe",query.clone()).await["values"]["text"], "[]");
    let history = json!({"launch_id":id,"operation":"requests","filter":{"url_contains":"/api/"}});
    let pending = m.call_tool("debug",history.clone()).await;
    assert!(pending["entries"][0]["request"].as_str().unwrap().contains("pending(rule=api)"), "{pending}");
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let runtime = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    runtime.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(m.call_tool("observe",query.clone()).await["values"]["text"], "[]");
    assert_eq!(m.call_tool("debug",history).await,pending);
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    assert_eq!(m.call_tool("observe",query.clone()).await["values"]["text"], "[\"recorded\"]");
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    assert_eq!(m.call_tool("observe",query).await["values"]["text"], "[\"recorded\"]");
    m.call_tool("stop",json!({"launch_id":id})).await;
}

// A recorded response pending at the stopped boundary must forbid the same installation in replay.
#[tokio::test]
async fn prepared_rule_refusal_replays_after_the_source_cassette_is_removed() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (issued_tx, issued_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server_thread = std::thread::spawn(move || {
        let request = server.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
        issued_tx.send(()).unwrap();
        release_rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        request.respond(tiny_http::Response::from_string("released")).unwrap();
    });
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launch = m.call_tool("launch",json!({"url":format!("http://{addr}/network-rules.html"),
        "rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"record","cassette":"source"}},
        {"id":"page","match":{"url":format!("http://{addr}/network-rules.html")},
        "action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
        "body":include_str!("fixtures/network-rules.html")}}]})).await;
    let id = &launch["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5),issued_rx).await.unwrap().unwrap();
    let before = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    let path = ws.root().join("prepared.jsonl");
    std::fs::write(&path,"").unwrap();
    let (failed,error) = m.try_tool("network",json!({"launch_id":id,"request_id":"late-horizon",
        "operation":"set_rules","expected_revision":0,"boundary":before["boundary"],
        "rules":[{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":path}}]})).await;
    assert!(failed && error["code"]=="not_actionable","{error}");
    assert_eq!(m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await,before);
    std::fs::remove_file(path).unwrap();
    release_tx.send(()).unwrap();
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    server_thread.join().unwrap();
    let page = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query",
        "target":{"by":"css","selector":"#responses"},"props":["text"]}})).await;
    assert_eq!(page["values"]["text"],"[\"released\"]");
    let after = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    let installed = m.call_tool("network",json!({"launch_id":id,"operation":"set_rules",
        "expected_revision":0,"boundary":after["boundary"],"rules":[]})).await;
    assert_eq!(installed["revision"],1,"progress resolves the admission barrier");
    let stopped = m.call_tool("stop",json!({"launch_id":id})).await;
    let bundle = std::path::Path::new(stopped["bundle"].as_str().unwrap());
    let attempt = frontcap_core::canonical::sha256_hex("late-horizon");
    assert!(bundle.join(format!("inputs/rule-attempts/{attempt}.json")).is_file(),
        "the refusal must happen after input preparation");
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn ordered_rule_edits_preserve_held_requests_and_replay() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let old = json!({"id":"old","match":{"url":format!("http://{addr}/api/*")},"action":{"type":"hold"}});
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html"),"rules":[old]})).await;
    let id = &launched["launch_id"];
    let click = json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}});
    m.call_tool("act", click.clone()).await;
    let held = m.call_tool("network",json!({"launch_id":id,"operation":"list_held"})).await;
    assert_eq!(held["held"].as_array().unwrap().len(),1);
    let current = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    let add = json!({"launch_id":id,"request_id":"add","operation":"add_rule","expected_revision":0,"boundary":current["boundary"],"before":"old",
        "rule":{"id":"new","match":{"url":format!("http://{addr}/api/*")},"action":{"type":"mock","status":200,"body":"new"}}});
    let receipt = m.call_tool("network",add.clone()).await;
    assert_eq!(receipt["revision"],1);
    // A rule installation changes how later requests are answered, not the page: the boundary observed
    // before it stays current (scenario 2 consumer run: a locate after add_rule was observation_changed).
    let located = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"locate","boundary":current["boundary"],"point":[10,10]}})).await;
    assert_eq!(located["observation"],"located","{located}");
    assert_eq!(m.call_tool("network",add.clone()).await,receipt);
    let current = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(current["rules"][0]["id"],"new");
    assert_eq!(current["rules"][1]["id"],"old");
    let mut duplicate = add.clone(); duplicate["request_id"]=json!("duplicate");
    duplicate["expected_revision"]=json!(1); duplicate["boundary"]=current["boundary"].clone();
    assert!(m.try_tool("network",duplicate).await.0);
    let protected = json!({"launch_id":id,"operation":"remove_rule","expected_revision":1,"boundary":current["boundary"],"id":"default-block"});
    assert!(m.try_tool("network",protected).await.0);
    let remove = json!({"launch_id":id,"request_id":"remove","operation":"remove_rule","expected_revision":1,"boundary":current["boundary"],"id":"old"});
    let receipt = m.call_tool("network",remove.clone()).await;
    assert_eq!(receipt["revision"],2);
    assert_eq!(m.call_tool("network",remove).await,receipt);
    assert_eq!(m.call_tool("network",json!({"launch_id":id,"operation":"list_held"})).await,held);
    m.call_tool("act", click).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    m.call_tool("network",json!({"launch_id":id,"operation":"respond","ordinal":held["held"][0]["ordinal"],"response":{"status":200,"body":"old"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    assert_eq!(m.call_tool("observe",query).await["values"]["text"],"[\"new\",\"old\"]");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn rule_edits_preserve_existing_cassette_assets_after_original_removal() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    std::fs::create_dir_all(ws.root()).unwrap();
    let cassette = ws.root().join("original.jsonl");
    let line = json!({"key":{"method":"GET","url_normalized":format!("http://{addr}/api/value?n=1")},
        "response":{"status":200,"headers":{},"body_base64":"Y2Fzc2V0dGU="},
        "latency":{"virtual_ms":20,"wall_ms":0}});
    std::fs::write(&cassette, format!("{line}\n")).unwrap();
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html")})).await;
    let id = &launched["launch_id"];
    let before = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let receipt = m.call_tool("network", json!({"launch_id":id,"operation":"add_rule",
        "expected_revision":0,"boundary":before["boundary"],"rule":{"id":"api",
        "match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":cassette,"after_last":"fail"}}})).await;
    assert_eq!(receipt["revision"], 1);
    std::fs::remove_file(&cassette).unwrap();
    let current = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network", json!({"launch_id":id,"operation":"add_rule","expected_revision":1,
        "boundary":current["boundary"],"rule":{"id":"unused","match":{"url":"https://unused.example.test/*"},"action":{"type":"hold"}}})).await;
    let current = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network", json!({"launch_id":id,"operation":"remove_rule","expected_revision":2,
        "boundary":current["boundary"],"id":"unused"})).await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let observed = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}})).await;
    assert_eq!(observed["values"]["text"], "[\"cassette\"]");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
    let relocated = support::serve();
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay", "url":format!("http://{relocated}/network-rules.html")})).await;
    assert_eq!(replay["equal"], true, "moved-origin replay: {replay}");
}

/// Launches a three-step sequence, takes one step, then adds and removes an unrelated rule.
async fn sequence_after_unrelated_edits(m: &mut support::Mcp, addr: &std::net::SocketAddr) -> (Value, Value, Value) {
    let api = format!("http://{addr}/api/*");
    let steps = json!({"id":"steps","match":{"url":api},"action":{"type":"sequence","after_last":"repeat_last",
        "steps":[{"status":200,"body":"one"},{"status":200,"body":"two"},{"status":200,"body":"three"}]}});
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html"),"rules":[steps]})).await;
    let id = launched["launch_id"].clone();
    let click = json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"settle","idle_window_ms":50,"max_ms":1000}});
    m.call_tool("act", click.clone()).await;
    let current = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network",json!({"launch_id":id,"operation":"add_rule","expected_revision":0,"boundary":current["boundary"],"before":"steps",
        "rule":{"id":"other","match":{"url":"https://elsewhere.test/*"},"action":{"type":"hold"}}})).await;
    let current = m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network",json!({"launch_id":id,"operation":"remove_rule","expected_revision":1,"boundary":current["boundary"],"id":"other"})).await;
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    (id, click, query)
}

/// Editing other rules is not a reason to restart a sequence: an unchanged rule continues from its
/// consumed step, and replay reproduces that.
#[tokio::test]
async fn unrelated_rule_edits_keep_an_unchanged_sequence_position() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, click, query) = sequence_after_unrelated_edits(&mut m, &addr).await;
    m.call_tool("act", click.clone()).await;
    m.call_tool("act", click).await;
    assert_eq!(m.call_tool("observe",query).await["values"]["text"],"[\"one\",\"two\",\"three\"]");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

/// A recorder relaunched after the edits derives the same position from the rule history.
#[tokio::test]
async fn a_replaced_recorder_keeps_an_unchanged_sequence_position_after_edits() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, click, query) = sequence_after_unrelated_edits(&mut m, &addr).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    m.call_tool("act", click.clone()).await;
    m.call_tool("act", click).await;
    assert_eq!(m.call_tool("observe",query).await["values"]["text"],"[\"one\",\"two\",\"three\"]");
    m.call_tool("stop",json!({"launch_id":id})).await;
}

/// What the page sent is part of a held request: `list_held` shows its body, `status` only lists it.
#[tokio::test]
async fn held_requests_show_the_body_the_page_sent() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = format!("http://{addr}");
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/checkout.html"),
        "profile":{"initial_storage":{"origin":origin,"local":{"cart":"[{\"name\":\"a\",\"price\":2}]"}}},
        "rules":[{"id":"pay","match":{"method":"POST","url":format!("{origin}/api/pay")},"action":{"type":"hold"}}]})).await;
    let id = &launched["launch_id"];
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"fill","target":{"by":"css","selector":"#note"},"value":"ring twice"},"then":{"type":"none"}})).await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#pay"}},"then":{"type":"none"}})).await;
    let held = m.call_tool("network", json!({"launch_id":id,"operation":"list_held"})).await;
    let body = held["held"][0]["body"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(body).unwrap(), json!({"items":1,"note":"ring twice"}));
    assert_eq!(held["held"][0]["body_bytes"].as_u64().unwrap() as usize, body.len());
    let status = m.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(status["held_requests"][0]["ordinal"], held["held"][0]["ordinal"]);
    assert!(status["held_requests"][0].get("body").is_none(), "{status}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn a_body_condition_that_is_not_a_json_pointer_is_refused_before_installation() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rule = |key: &str| json!([{"id":"ok","match":{"method":"POST","url":"https://api.example.test/verify","body":{key:"314159"}},"action":{"type":"mock","status":200}}]);
    let (failed, error) = m.try_tool("launch", json!({"url":format!("http://{addr}/count.html"),"rules":rule("code")})).await;
    assert!(failed, "{error}");
    assert!(error["message"].as_str().unwrap().contains(r#"write "/code""#), "{error}");
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/count.html"),"rules":rule("/code")})).await;
    let id = &launched["launch_id"];
    let listed = m.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let (failed, error) = m.try_tool("network", json!({"launch_id":id,"operation":"set_rules","expected_revision":listed["revision"],"boundary":scene["boundary"],"rules":rule("code")})).await;
    assert!(failed, "{error}");
    assert!(error["message"].as_str().unwrap().contains(r#"write "/code""#), "{error}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

/// A relative cassette path names the calling server's file, wherever the recorder was started.
#[tokio::test]
async fn a_rule_edit_reads_a_relative_cassette_from_the_calling_server() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut first = support::Mcp::start_in(&ws.root()).await;
    let launched = first.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html")})).await;
    let id = launched["launch_id"].clone();
    first.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    let dir = tempfile::tempdir().unwrap();
    let line = json!({"key":{"method":"GET","url_normalized":format!("http://{addr}/api/value?n=1")},
        "response":{"status":200,"headers":{},"body_base64":"Y2Fzc2V0dGU="},
        "latency":{"virtual_ms":20,"wall_ms":0}});
    std::fs::write(dir.path().join("relative.jsonl"), format!("{line}\n")).unwrap();
    let mut second = support::Mcp::start_from(&ws.root(), dir.path()).await;
    second.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let before = second.call_tool("network", json!({"launch_id":id,"operation":"list_rules"})).await;
    second.call_tool("network", json!({"launch_id":id,"operation":"add_rule","expected_revision":before["revision"],
        "boundary":before["boundary"],"rule":{"id":"api","match":{"url":format!("http://{addr}/api/*")},
        "action":{"type":"replay","cassette":"relative.jsonl","after_last":"fail"}}})).await;
    second.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}})).await;
    second.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let observed = second.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}})).await;
    assert_eq!(observed["values"]["text"], "[\"cassette\"]");
    second.call_tool("stop", json!({"launch_id":id})).await;
}
