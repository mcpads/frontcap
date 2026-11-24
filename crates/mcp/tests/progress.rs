mod support;
use serde_json::json;

#[tokio::test]
async fn cross_site_navigation_stops_before_application_execution_and_replays() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let first = "https://one.example.test/start";
    let second = "https://two.other.test/arrived";
    let body = format!("<script>window.late=false;setTimeout(()=>{{location.href={}}},250);setTimeout(()=>{{late=true}},251)</script>", json!(second));
    let arrived = "<script>window.appStarted=true;window.late=false;setTimeout(()=>{late=true},1)</script>";
    let rules: Vec<_> = [("start", first, body.as_str()), ("arrived", second, arrived)].into_iter().map(|(name, url, body)| json!({
        "id":name,"match":{"url":url},"action":{"type":"mock","status":200,
        "headers":{"content-type":"text/html"},"body":body,"latency_ms":0}
    })).collect();
    let launch = m.call_tool("launch", json!({"url":first,"rules":rules,"then":{"type":"none"}})).await;
    let id = launch["launch_id"].as_str().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let stopped = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"navigation","max_ms":1000}})).await;
    assert_eq!(stopped["outcome"]["type"], "event", "{stopped}");
    assert_eq!(stopped["t_us"], 250_000, "{stopped}");
    let expression = "({url:location.href,started:window.appStarted===true,late:window.late===true,now:Date.now()-946684800000})";
    let args = json!({"launch_id":id,"operation":"evaluate","expression":expression});
    let before = m.call_tool("debug", args.clone()).await;
    assert_eq!(before["value"], json!({"url":second,"started":false,"late":false,"now":250}));
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let after = m.call_tool("debug", args).await;
    assert_eq!(after["value"], json!({"url":second,"started":true,"late":true,"now":301}));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn navigation_stops_before_later_tasks_and_new_document_scripts() {
    let addr = support::serve();
    for suffix in ["", "?same"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/navigation-stop.html{suffix}"),"then":{"type":"none"}})).await;
        let id = launch["launch_id"].as_str().unwrap();
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let stopped = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"navigation","max_ms":1000}})).await;
        assert_eq!(stopped["outcome"]["type"], "event", "{suffix}: {stopped}");
        assert_eq!(stopped["t_us"], 250_000, "{suffix}: {stopped}");
        let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({hash:location.hash,late:window.late===true,parsed:window.parsed===true})"})).await;
        assert_eq!(observed["value"], json!({"hash":if suffix.is_empty() {"#arrived"} else {"#same"},"late":false,"parsed":false}));
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let resumed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({late:window.late===true,parsed:window.parsed===true})"})).await;
        assert_eq!(resumed["value"], json!({"late":true,"parsed":suffix.is_empty()}));
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{suffix}: {replay}");
    }
}

#[tokio::test]
async fn event_stops_are_armed_deduplicated_and_replayed_at_their_boundaries() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules: Vec<_> = ["first", "other", "second"].into_iter().map(|name| json!({
        "id": name, "match": {"url":format!("https://api.example.test/{name}")},
        "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":name,"latency_ms":500}
    })).collect();
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/event-stops.html"),"rules":rules,"then":{"type":"none"}})).await;
    let id = launch["launch_id"].as_str().unwrap();
    // Complete initial document loading before arming for the application's API responses.
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let args = json!({"launch_id":id,"request_id":"next-response","op":{"type":"next","event":"response","max_ms":549}});
    let stopped = m.call_tool("wait", args.clone()).await;
    assert_eq!(stopped["outcome"]["type"], "event", "a trigger at the budget wins: {stopped}");
    assert_eq!(stopped["t_us"], 600_000);
    assert_eq!(stopped["outcome"]["triggers"].as_array().unwrap().len(), 2, "retain coincident responses");
    for trigger in stopped["outcome"]["triggers"].as_array().unwrap() {
        assert_eq!(trigger["t_us"], 600_000);
        assert!(trigger["seq"].as_u64().unwrap() >= stopped["armed_seq"].as_u64().unwrap());
    }
    assert_eq!(m.call_tool("wait", args).await, stopped);
    let inspect = json!({"launch_id":id,"operation":"evaluate","expression":"({delivered,painted,later,now:performance.now()})"});
    let before = m.call_tool("debug", inspect.clone()).await;
    assert_eq!(before["value"], json!({"delivered":["first","other"],"painted":false,"later":false,"now":600}));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(m.call_tool("debug", inspect).await, before, "host delay and observations do not advance the world");
    let presented = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"presentation","max_ms":100}})).await;
    assert_eq!(presented["outcome"]["event"], "presentation");
    assert_eq!(presented["outcome"]["type"], "event");
    assert_eq!(presented["t_us"], 601_000);
    assert_eq!(presented["outcome"]["triggers"][0]["context"], "page", "the surface's context is named: {presented}");
    let state = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({painted,later})"})).await;
    assert_eq!(state["value"], json!({"painted":true,"later":false}));
    m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":1000}})).await;
    assert_eq!(next["t_us"], 1_200_000, "the previous response cannot satisfy a new arm");
    for ms in [0, 20] {
        let none = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":ms}})).await;
        assert_eq!(none["outcome"]["type"], "event_timed_out");
        assert_eq!(none["elapsed_us"], ms * 1000);
    }
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn uncaught_exception_stops_before_later_tasks_and_replays() {
    for (suffix, count) in [("", 1), ("?promise", 1), ("?twins", 2)] {
        let addr = support::serve();
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/exception-stop.html{suffix}"),"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let args = json!({"launch_id":id,"request_id":"next-error","op":{"type":"next","event":"exception","max_ms":199}});
        let stopped = m.call_tool("wait", args.clone()).await;
        assert_eq!(stopped["outcome"]["type"], "event", "{suffix}: {stopped}");
        assert_eq!(stopped["t_us"], 250_000, "{stopped}");
        assert_eq!(stopped["outcome"]["triggers"].as_array().unwrap().len(), count, "{stopped}");
        assert!(stopped["outcome"]["triggers"][0]["text"].as_str().unwrap().contains("scheduled failure"));
        assert_eq!(m.call_tool("wait", args).await, stopped);
        let inspect = json!({"launch_id":id,"operation":"evaluate","expression":"({events,now:Date.now()-946684800000})"});
        let before = m.call_tool("debug", inspect.clone()).await;
        assert_eq!(before["value"], json!({"events":["error"],"now":250}));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(m.call_tool("debug", inspect).await, before);
        let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"exception","max_ms":300}})).await;
        assert_eq!(next["t_us"], 400_000, "{next}");
        assert_eq!(next["outcome"]["triggers"].as_array().unwrap().len(), count, "{next}");
        assert!(next["outcome"]["triggers"][0]["text"].as_str().unwrap().contains("second failure"));
        let timeout = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"exception","max_ms":20}})).await;
        assert_eq!(timeout["outcome"]["type"], "event_timed_out");
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{replay}");
    }
}

#[tokio::test]
async fn interception_stops_before_response_and_resumes_or_accepts_an_answer() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules: Vec<_> = ["first", "other", "second"].into_iter().map(|name| json!({
        "id":name,"match":{"url":format!("https://api.example.test/{name}")},
        "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":name}
    })).collect();
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/interception-stop.html"),"rules":rules,"then":{"type":"none"}})).await;
    let id = &launch["launch_id"];
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let args = json!({"launch_id":id,"request_id":"inspect-save","op":{"type":"next","event":"interception","max_ms":199}});
    let stopped = m.call_tool("wait", args.clone()).await;
    assert_eq!(stopped["outcome"]["type"], "event", "{stopped}");
    assert_eq!(stopped["t_us"], 250_000, "{stopped}");
    assert_eq!(stopped["outcome"]["triggers"].as_array().unwrap().len(), 2, "{stopped}");
    assert_eq!(m.call_tool("wait", args).await, stopped);
    let inspect = json!({"launch_id":id,"operation":"evaluate","expression":"({delivered,later,now:Date.now()-946684800000})"});
    let before = m.call_tool("debug", inspect.clone()).await;
    assert_eq!(before["value"], json!({"delivered":[],"later":false,"now":250}));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(m.call_tool("debug", inspect.clone()).await, before);
    let held = m.call_tool("network", json!({"launch_id":id,"operation":"list_held"})).await;
    assert_eq!(held["held"].as_array().unwrap().len(), 2);
    let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":300}})).await;
    assert_eq!(next["t_us"], 400_000, "{next}");
    assert_eq!(next["outcome"]["triggers"].as_array().unwrap().len(), 1);
    assert_eq!(m.call_tool("debug", inspect.clone()).await["value"], json!({"delivered":["first","other"],"later":true,"now":400}));
    let ordinal = &next["outcome"]["triggers"][0]["ordinal"];
    m.call_tool("network", json!({"launch_id":id,"operation":"respond","ordinal":ordinal,"response":{"status":200,"body":"changed","headers":{"access-control-allow-origin":"*"}}})).await;
    let timed = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":20}})).await;
    assert_eq!(timed["outcome"]["type"], "event_timed_out", "{timed}");
    assert_eq!(m.call_tool("debug", inspect).await["value"]["delivered"], json!(["first","other","changed"]));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn interception_of_a_document_keeps_the_old_page_until_answered() {
    for manual in [false, true] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let first = "https://one.example.test/start";
        let second = "https://two.other.test/arrived";
        let body = format!("<script>window.late=false;setTimeout(()=>{{location.href={}}},250);setTimeout(()=>{{late=true}},251)</script>", json!(second));
        let rules: Vec<_> = [("start", first, body.as_str()), ("arrived", second, "<script>window.arrivedAt=Date.now()-946684800000</script>")].into_iter().map(|(name, url, body)| json!({
            "id":name,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}
        })).collect();
        let launch = m.call_tool("launch", json!({"url":first,"rules":rules,"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let stopped = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":199}})).await;
        assert_eq!(stopped["t_us"], 250_000, "{stopped}");
        assert_eq!(stopped["outcome"]["triggers"][0]["url"], second, "{stopped}");
        let before = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({url:location.href,late,now:Date.now()-946684800000})"})).await;
        assert_eq!(before["value"], json!({"url":first,"late":false,"now":250}));
        let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
        assert_eq!(scene["boundary"]["t_us"], 250_000, "{scene}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let repeated = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
        assert_eq!(repeated["boundary"], scene["boundary"]);
        if manual {
            let ordinal = &stopped["outcome"]["triggers"][0]["ordinal"];
            m.call_tool("network", json!({"launch_id":id,"operation":"respond","ordinal":ordinal,
                "response":{"status":200,"headers":{"content-type":"text/html"},
                "body":"<script>window.arrivedAt=Date.now()-946684800000;window.manual=true</script>"}})).await;
            let frozen = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate",
                "expression":"({url:location.href,started:typeof arrivedAt!=='undefined'})"})).await;
            assert_eq!(frozen["value"], json!({"url":second,"started":false}));
        } else {
            let committed = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"navigation","max_ms":1000}})).await;
            assert_eq!(committed["t_us"], 250_000, "{committed}");
        }
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        let after = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({url:location.href,arrivedAt,manual:window.manual===true})"})).await;
        assert_eq!(after["value"], json!({"url":second,"arrivedAt":250,"manual":manual}));
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{replay}");
    }
}

#[tokio::test]
async fn interception_resumption_preserves_the_original_response_deadline() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let url = "https://one.example.test/start";
    let api = "https://one.example.test/api";
    let body = format!("<script>window.deliveredAt=null;setTimeout(()=>fetch({}).then(r=>r.text()).then(()=>deliveredAt=Date.now()-946684800000),250)</script>", json!(api));
    let rules = json!([
        {"id":"start","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
        {"id":"api","match":{"url":api},"action":{"type":"mock","status":200,"body":"done","latency_ms":100}}
    ]);
    let launch = m.call_tool("launch", json!({"url":url,"rules":rules,"then":{"type":"none"}})).await;
    let id = &launch["launch_id"];
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let intercepted = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":1000}})).await;
    assert_eq!(intercepted["t_us"], 250_000, "{intercepted}");
    let ordinal = intercepted["outcome"]["triggers"][0]["ordinal"].clone();
    let history = m.call_tool("debug", json!({"launch_id":id,"operation":"requests","filter":{"url_contains":api}})).await;
    assert_eq!(history["entries"], json!([{"ordinal":ordinal,"t_us":250000,"request":format!("GET {api} → held(rule=api)")}]));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let waiting = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":99}})).await;
    assert_eq!(waiting["outcome"]["type"], "event_timed_out", "{waiting}");
    assert_eq!(waiting["t_us"], 349_000);
    let history = m.call_tool("debug", json!({"launch_id":id,"operation":"requests","filter":{"url_contains":api}})).await;
    assert_eq!(history["entries"], json!([{"ordinal":ordinal,"t_us":250000,"request":format!("GET {api} → pending(rule=api)")}]));
    let delivered = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":1}})).await;
    assert_eq!(delivered["outcome"]["type"], "event", "{delivered}");
    assert_eq!(delivered["t_us"], 350_000);
    let history = m.call_tool("debug", json!({"launch_id":id,"operation":"requests","filter":{"url_contains":api}})).await;
    assert_eq!(history["entries"], json!([{"ordinal":ordinal,"t_us":250000,"answered_us":350000,"request":format!("GET {api} → mock(rule=api) 200")}]));
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"deliveredAt"})).await;
    assert_eq!(observed["value"], 350);
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn failed_and_aborted_interceptions_do_not_resume_their_original_response() {
    for abort in [false, true] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let url = "https://one.example.test/start";
        let api = "https://one.example.test/api";
        let body = format!("<script>window.results=[];window.later=false;window.controller=new AbortController();window.request=()=>fetch({},{{signal:controller.signal}}).then(r=>r.text()).then(v=>results.push(v)).catch(e=>results.push(e.name));setTimeout(request,250);setTimeout(()=>later=true,251)</script>", json!(api));
        let rules = json!([
            {"id":"start","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
            {"id":"api","match":{"url":api},"action":{"type":"mock","status":200,"body":"original"}}
        ]);
        let launch = m.call_tool("launch", json!({"url":url,"rules":rules,"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let intercepted = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":1000}})).await;
        assert_eq!(intercepted["t_us"], 250_000, "{intercepted}");
        if abort {
            m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"controller.abort()"})).await;
        } else {
            m.call_tool("network", json!({"launch_id":id,"operation":"fail","ordinal":intercepted["outcome"]["triggers"][0]["ordinal"],"reason":"aborted"})).await;
        }
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({results,later})"})).await;
        assert_eq!(observed["value"], json!({"results":[if abort {"AbortError"} else {"TypeError"}],"later":true}), "{observed}");
        let held = m.call_tool("network", json!({"launch_id":id,"operation":"list_held"})).await;
        assert_eq!(held["held"], json!([]));
        m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"controller=new AbortController();setTimeout(request,10)"})).await;
        let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":10}})).await;
        assert_eq!(next["t_us"], 280_000, "{next}");
        assert_ne!(next["outcome"]["triggers"][0]["ordinal"], intercepted["outcome"]["triggers"][0]["ordinal"]);
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"results"})).await;
        assert_eq!(observed["value"], json!([if abort {"AbortError"} else {"TypeError"},"original"]));
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{replay}");
    }
}

#[tokio::test]
async fn cancelling_an_intercepted_document_preserves_the_old_page_and_later_navigation() {
    for page_abort in [false, true] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let first = "https://one.example.test/start";
        let second = "https://two.other.test/arrived";
        let body = format!("<script>window.later=false;setTimeout(()=>{{location.href={}}},250);setTimeout(()=>later=true,251)</script>", json!(second));
        let rules = json!([
            {"id":"start","match":{"url":first},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
            {"id":"arrived","match":{"url":second},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<script>window.arrived=true</script>"}}
        ]);
        let launch = m.call_tool("launch", json!({"url":first,"rules":rules,"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let intercepted = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":1000}})).await;
        assert_eq!(intercepted["t_us"], 250_000, "{intercepted}");
        if page_abort {
            m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"window.stop()"})).await;
        } else {
            m.call_tool("network", json!({"launch_id":id,"operation":"fail","ordinal":intercepted["outcome"]["triggers"][0]["ordinal"],"reason":"aborted"})).await;
        }
        let old = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({url:location.href,later,now:Date.now()-946684800000})"})).await;
        assert_eq!(old["value"], json!({"url":first,"later":false,"now":250}));
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":format!("setTimeout(()=>{{location.href={}}},10)",json!(second))})).await;
        let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"navigation","max_ms":1000}})).await;
        assert_eq!(next["t_us"], 280_000, "{next}");
        let new = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({url:location.href,started:window.arrived===true})"})).await;
        assert_eq!(new["value"], json!({"url":second,"started":false}));
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
        let new = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"window.arrived===true"})).await;
        assert_eq!(new["value"], true);
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{replay}");
    }
}

#[tokio::test]
async fn focus_stops_preserve_save_feedback_and_replay_across_reachable_contexts() {
    let addr = support::serve();
    for suffix in ["", "?shadow", "?closed", "?frame"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/focus-stop.html{suffix}"),"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
        let args = json!({"launch_id":id,"request_id":"save-focus","op":{"type":"next","event":"focus","max_ms":199}});
        let stopped = m.call_tool("wait", args.clone()).await;
        assert_eq!(stopped["outcome"]["type"], "event", "{suffix}: {stopped}");
        assert_eq!(stopped["t_us"], 250_000, "{suffix}: {stopped}");
        let triggers = stopped["outcome"]["triggers"].as_array().unwrap();
        assert!(triggers.iter().any(|t| t["focused"] == true && t["target"].as_str().unwrap().contains("id=\"saved\"")), "{suffix}: {stopped}");
        assert!(triggers.iter().all(|t| t["t_us"] == 250_000));
        assert_eq!(m.call_tool("wait", args).await, stopped);
        let inspect = json!({"launch_id":id,"operation":"evaluate","expression":"({feedback:out.textContent,later,focused:(()=>{let a=document.activeElement;for(;;){const next=a?.shadowRoot?.activeElement||(a===window.closedHost?window.closedRoot.activeElement:null)||a?.contentDocument?.activeElement;if(!next)return a?.id;a=next}})(),now:Date.now()-946684800000})"});
        let before = m.call_tool("debug", inspect.clone()).await;
        assert_eq!(before["value"], json!({"feedback":"saved","later":false,"focused":"saved","now":250}));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(m.call_tool("debug", inspect).await, before);
        for (name, at) in [("again", 400_000), ("blur", 500_000)] {
            let next = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"focus","max_ms":300}})).await;
            assert_eq!(next["t_us"], at, "{suffix} {name}: {next}");
            assert_eq!(next["outcome"]["type"], "event");
            if name == "blur" {
                assert!(next["outcome"]["triggers"].as_array().unwrap().iter().any(|t| t["focused"] == false));
            }
        }
        let timeout = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"focus","max_ms":20}})).await;
        assert_eq!(timeout["outcome"]["type"], "event_timed_out");
        m.call_tool("stop", json!({"launch_id":id})).await;
        let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"], true, "{suffix}: {replay}");
    }
}

/// A response handed over while the world is stopped (an agent's answer, or a mock answered during an
/// input without progress) reaches the page only with the next progress: a response wait armed after
/// it stops once it is delivered, at the same instant, instead of waiting for a later one.
#[tokio::test]
async fn a_response_answered_while_stopped_is_the_next_delivered_response() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([{"id":"held","match":{"url":format!("http://{addr}/api/value?n=1")},"action":{"type":"hold"}},
        {"id":"mocked","match":{"url":format!("http://{addr}/api/*")},"action":{"type":"mock","status":200,"body":"mocked"}}]);
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/network-rules.html"),"rules":rules})).await;
    let id = &launch["launch_id"];
    let click = json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#request"}},"then":{"type":"none"}});
    let query = json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#responses"},"props":["text"]}});
    let next = json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":1000}});

    m.call_tool("act", click.clone()).await;
    let held = m.call_tool("network", json!({"launch_id":id,"operation":"list_held"})).await;
    let ordinal = held["held"][0]["ordinal"].clone();
    // An already held request is not the next interception; a timed-out wait names it (scenario 5 rerun).
    let missed = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":20}})).await;
    assert_eq!(missed["outcome"]["type"], "event_timed_out", "{missed}");
    assert_eq!(missed["already_held"][0]["ordinal"], ordinal, "{missed}");
    assert_eq!(missed["already_held"][0]["rule"], "held", "{missed}");
    // A latency is a rule's: the agent answers at the current instant (scenario 2 consumer run: it was ignored).
    let (refused, error) = m.try_tool("network", json!({"launch_id":id,"operation":"respond","ordinal":ordinal,"response":{"status":200,"body":"answered","latency_ms":300}})).await;
    assert!(refused && error["code"] == "invalid_arguments", "{error}");
    m.call_tool("network", json!({"launch_id":id,"operation":"respond","ordinal":ordinal,"response":{"status":200,"body":"answered"}})).await;
    assert_eq!(m.call_tool("observe", query.clone()).await["values"]["text"], "[]", "not delivered while stopped");
    let delivered = m.call_tool("wait", next.clone()).await;
    assert_eq!(delivered["outcome"]["type"], "event", "{delivered}");
    assert_eq!(delivered["elapsed_us"], 0, "delivered at the instant it was answered: {delivered}");
    assert_eq!(delivered["outcome"]["triggers"][0]["ordinal"], ordinal);
    assert_eq!(m.call_tool("observe", query.clone()).await["values"]["text"], "[\"answered\"]");

    // A mock answered during the click itself, likewise.
    m.call_tool("act", click).await;
    let mocked = m.call_tool("wait", next.clone()).await;
    assert_eq!(mocked["outcome"]["type"], "event", "{mocked}");
    assert_eq!(mocked["elapsed_us"], 0, "{mocked}");
    assert_eq!(m.call_tool("observe", query).await["values"]["text"], "[\"answered\",\"mocked\"]");

    // Once delivered, those responses cannot satisfy another wait.
    let none = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":20}})).await;
    assert_eq!(none["outcome"]["type"], "event_timed_out", "{none}");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Scenario 1 consumer run: a preflighted save that changes the page at the click. The click's own DOM
/// changes are its effects without a settle (they were counted only at the next operation), and a
/// response wait stops at the save's response, not at the preflight the platform answered.
#[tokio::test]
async fn a_preflighted_save_stops_at_its_response_and_counts_the_clicks_changes() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><button id=save>Save</button><script>save.onclick = () => { save.textContent = 'Saving…'; save.disabled = true;
fetch('https://api.example.test/settings', {method: 'POST', headers: {'content-type': 'application/json'}, body: '{}'}).then(r => { save.textContent = r.ok ? 'Saved' : 'Failed'; }); };</script>"#;
    let rules = json!([
        {"id":"app","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"settings","match":{"url":"https://api.example.test/settings"},"action":{"type":"mock","status":200,"body":"{}","latency_ms":500}}]);
    let launch = m.call_tool("launch", json!({"url":"https://app.example.test/","rules":rules})).await;
    let id = launch["launch_id"].as_str().unwrap();
    let clicked = m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#save"}},"then":{"type":"none"}})).await;
    assert!(clicked["effects"]["dom_mutations"].as_u64().unwrap() >= 2, "{clicked}");
    assert!(clicked["effects"]["requests"].to_string().contains("OPTIONS https://api.example.test/settings → preflight 204"), "{clicked}");
    let at = clicked["t_after_us"].as_u64().unwrap();
    let stopped = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"response","max_ms":1000}})).await;
    assert_eq!(stopped["outcome"]["type"], "event", "{stopped}");
    assert_eq!(stopped["t_us"].as_u64().unwrap(), at + 500_000, "{stopped}");
    // The stop names the request the response answered.
    assert_eq!(stopped["outcome"]["triggers"][0]["request"], "POST https://api.example.test/settings → mock(rule=settings) 200", "{stopped}");
    let saved = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#save"},"props":["text"]}})).await;
    assert_eq!(saved["values"]["text"], "Saved", "{saved}");
    let mut history = m.call_tool("debug", json!({"launch_id":id,"operation":"requests","filter":{"url_contains":"api.example.test"}})).await;
    let ordinals: Vec<serde_json::Value> = history["entries"].as_array_mut().unwrap().iter_mut().map(|e| e.as_object_mut().unwrap().remove("ordinal").unwrap()).collect();
    assert_eq!(ordinals[1], stopped["outcome"]["triggers"][0]["ordinal"], "entries name requests as response stops do");
    assert_eq!(history["entries"], json!([
        {"t_us":at,"answered_us":at,"request":"OPTIONS https://api.example.test/settings → preflight 204"},
        {"t_us":at,"answered_us":at + 500_000,"request":"POST https://api.example.test/settings → mock(rule=settings) 200","body":"{}","body_bytes":2}]), "{history}");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// An interception stop holds a dedicated worker's request and a frame's document request as it holds the
/// page's own: the world stays at the request's instant until progress resumes, and replay agrees.
#[tokio::test]
async fn interception_stops_hold_a_workers_request_and_a_frames_document() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([
        {"id":"worker","match":{"url":"https://api.example.test/worker"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"from the worker's request"}},
        {"id":"frame","match":{"url":"https://frame.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p id=inside>framed</p>"}}
    ]);
    let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/interception-contexts.html"),"rules":rules,"then":{"type":"none"}})).await;
    let id = &launch["launch_id"];
    let delivered = json!({"launch_id":id,"operation":"evaluate","expression":"({delivered,now:Date.now()-946684800000})"});
    let list_held = json!({"launch_id":id,"operation":"list_held"});
    let stop = |max_ms: u64| json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":max_ms}});
    // The worker's own script, at launch.
    let script = m.call_tool("wait", stop(150)).await;
    assert_eq!(script["t_us"], 0, "{script}");
    assert!(script["outcome"]["triggers"][0]["url"].as_str().unwrap().ends_with("/interception-worker.js"), "{script}");
    // The worker's request, at its instant: nothing delivered yet.
    let fetched = m.call_tool("wait", stop(150)).await;
    assert_eq!((fetched["t_us"].clone(), fetched["outcome"]["triggers"][0]["url"].clone()), (json!(100_000), json!("https://api.example.test/worker")), "{fetched}");
    assert_eq!(m.call_tool("network", list_held.clone()).await["held"][0]["rule"], "worker");
    assert_eq!(m.call_tool("debug", delivered.clone()).await["value"], json!({"delivered":[],"now":100}));
    // The frame's document request, after the worker's answer was delivered.
    let framed = m.call_tool("wait", stop(200)).await;
    assert_eq!((framed["t_us"].clone(), framed["outcome"]["triggers"][0]["url"].clone()), (json!(200_000), json!("https://frame.example.test/")), "{framed}");
    assert_eq!(m.call_tool("network", list_held.clone()).await["held"][0]["rule"], "frame");
    assert_eq!(m.call_tool("debug", delivered.clone()).await["value"], json!({"delivered":["from the worker's request"],"now":200}));
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
    let inside = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#inside"},"props":["text"]}})).await;
    assert_eq!(inside["values"]["text"], "framed");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A worker's load held at an interception stop stays held across recorder replacement and is delivered
/// when progress resumes: its script at launch, and its own request.
#[tokio::test]
async fn a_workers_load_held_at_a_stop_survives_recorder_replacement() {
    for (stops, url) in [(1, "/interception-worker.js"), (2, "https://api.example.test/worker")] {
        let addr = support::serve();
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let rules = json!([
            {"id":"worker","match":{"url":"https://api.example.test/worker"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"from the worker's request"}},
            {"id":"frame","match":{"url":"https://frame.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<p id=inside>framed</p>"}}
        ]);
        let launch = m.call_tool("launch", json!({"url":format!("http://{addr}/interception-contexts.html"),"rules":rules,"then":{"type":"none"}})).await;
        let id = &launch["launch_id"];
        for _ in 0..stops {
            m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"interception","max_ms":150}})).await;
        }
        let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
        frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
        let reattached = m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
        let held = &reattached["status"]["held_requests"];
        assert!(held.as_array().unwrap().len() == 1 && held[0]["url"].as_str().unwrap().ends_with(url), "{reattached}");
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":300}})).await;
        let state = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"delivered"})).await;
        assert_eq!(state["value"], json!(["from the worker's request"]), "{url}");
        m.call_tool("stop", json!({"launch_id":id})).await;
    }
}

/// App-state consumer rerun: a wait reported no effects, so the consumer moved time with a hover to see
/// what a window posted. Each kind of wait reports what happened while it ran, as an act does.
#[tokio::test]
async fn waits_report_what_happened_while_time_ran() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><p id=late></p><script>
for (const at of [100, 600, 1200]) setTimeout(() => { console.warn('at ' + at); document.getElementById('late').textContent = at; }, at);
</script>"#;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "passthrough_origins": [], "then": {"type": "none"},
        "rules": [{"id": "app", "match": {"url": "https://app.example.test/"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": page}}]})).await;
    let id = launched["launch_id"].clone();
    let advanced = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 300}})).await;
    assert_eq!(advanced["effects"]["console"]["warn"], 1, "{advanced}");
    assert!(advanced["effects"]["dom_mutations"].as_u64().is_some_and(|n| n > 0), "{advanced}");
    let settled = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "settle", "idle_window_ms": 400, "max_ms": 800}})).await;
    assert_eq!(settled["effects"]["console"]["warn"], 1, "the 600 ms timer: {settled}");
    let until = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "until", "max_ms": 2000,
        "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#late"}, "text": "1200"}}})).await;
    assert_eq!(until["effects"]["console"]["warn"], 1, "the 1200 ms timer: {until}");
    let idle = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 100}})).await;
    assert_eq!(idle["effects"]["console"]["warn"], 0, "{idle}");
    for (answer, at) in [(&advanced, 100), (&settled, 600), (&until, 1200)] {
        assert_eq!(answer["effects"]["console_messages"], json!([{
            "t_us": at * 1000, "level": "warning", "text": format!("at {at}")
        }]), "{answer}");
    }
    assert!(idle["effects"].get("console_messages").is_none(), "{idle}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}
