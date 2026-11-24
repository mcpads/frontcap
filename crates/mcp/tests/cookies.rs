mod support;
use serde_json::{Value, json};

fn serve_cookies() -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let cookie = request.headers().iter().find(|h| h.field.equiv("Cookie")).map(|h| h.value.as_str()).unwrap_or("");
            let cookies: std::collections::BTreeMap<_,_> = cookie.split("; ").filter_map(|s| s.split_once('=')).collect();
            let value = serde_json::to_string(&cookies).unwrap();
            let body = if request.url() == "/start" {
                format!("<!doctype html><script>window.boot={{request:{value},visible:document.cookie}};window.privateCookies=null;fetch('/private/echo').then(r=>r.json()).then(v=>privateCookies=v)</script><p>Account</p>")
            } else { value };
            let response = tiny_http::Response::from_string(body).with_header(tiny_http::Header::from_bytes("Content-Type", if request.url() == "/start" { "text/html; charset=utf-8" } else { "application/json" }).unwrap());
            request.respond(response).unwrap();
        }
    });
    addr
}

fn cookie(origin: &str, name: &str, value: &str) -> Value {
    json!({"origin":origin,"name":name,"value":value,"path":"/","same_site":"lax"})
}

async fn evaluate(m: &mut support::Mcp, id: &Value, expression: &str) -> Value {
    m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":expression})).await["value"].clone()
}

#[tokio::test]
async fn cookies_precede_first_request_respect_scope_and_replay_without_reseeding() {
    let addr = serve_cookies();
    let origin = format!("http://{addr}");
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut auth = cookie(&origin, "login", "signed-in"); auth["http_only"] = json!(true);
    let mut scoped = cookie(&origin, "private", "only-here"); scoped["path"] = json!("/private");
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"then":{"type":"none"},"profile":{"initial_cookies":[auth,cookie(&origin,"locale","ko"),scoped]}})).await;
    let id = &launched["launch_id"];
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"boot").await, json!({"request":{"login":"signed-in","locale":"ko"},"visible":"locale=ko"}));
    assert_eq!(evaluate(&mut m,id,"privateCookies").await, json!({"login":"signed-in","locale":"ko","private":"only-here"}));
    evaluate(&mut m,id,"document.cookie='locale=en; Path=/'; true").await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"boot").await, json!({"request":{"login":"signed-in","locale":"en"},"visible":"locale=en"}));
    // A different browser profile at the same origin must not inherit this session.
    let clean = m.call_tool("launch", json!({"url":format!("{origin}/start")})).await;
    assert_eq!(evaluate(&mut m,&clean["launch_id"],"boot").await, json!({"request":{},"visible":""}));
    m.call_tool("stop", json!({"launch_id":clean["launch_id"]})).await;
    m.call_tool("stop", json!({"launch_id":id})).await;
    let moved = serve_cookies();
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay","url":format!("http://{moved}/start")})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn recorder_loss_adopts_mutated_cookies_without_reseeding() {
    let origin = format!("http://{}", serve_cookies());
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut auth = cookie(&origin, "login", "signed-in");
    auth["http_only"] = json!(true);
    auth["expires_at_ms"] = json!(946_684_800_201u64);
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"then":{"type":"none"},"profile":{"initial_cookies":[auth,cookie(&origin,"locale","ko"),cookie(&origin,"removed","seed")]}})).await;
    let id = &launched["launch_id"];
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    // Exercise adoption both before first script execution and after page mutations.
    for t_us in [0, 100_000] {
        let before = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
        before.recorder.kill().unwrap();
        assert!(before.browser.as_ref().unwrap().is_alive());
        let (failed, attached) = m.try_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
        assert!(!failed, "cookie adoption at {t_us} us: {attached}");
        assert_eq!((attached["status"]["clock"]["state"].clone(), attached["status"]["clock"]["t_us"].clone()), (json!("frozen"), json!(t_us)));
        let after = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
        assert_eq!(after.browser, before.browser, "adoption must retain the living browser");
        if t_us == 0 {
            m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
            assert_eq!(evaluate(&mut m,id,"boot.request").await, json!({"login":"signed-in","locale":"ko","removed":"seed"}));
            evaluate(&mut m,id,"document.cookie='locale=en; Path=/'; document.cookie='removed=; Max-Age=0; Path=/'; document.cookie='fresh=runtime; Path=/'; true").await;
        }
    }
    let visible = "Object.fromEntries(document.cookie.split('; ').map(s=>s.split('=')))";
    assert_eq!(evaluate(&mut m,id,visible).await, json!({"locale":"en","fresh":"runtime"}));
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"boot.request").await, json!({"login":"signed-in","locale":"en","fresh":"runtime"}));
    assert_eq!(evaluate(&mut m,id,visible).await, json!({"locale":"en","fresh":"runtime"}));
    // Both recorder replacements must preserve the initial absolute expiry.
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"boot.request").await, json!({"locale":"en","fresh":"runtime"}));
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let events: Vec<Value> = trace.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.iter().filter(|e| e["kind"] == "cookies_initialized").count(), 1);
    let (failed, replay) = m.try_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert!(failed && replay["code"] == "not_replayable", "recorder loss must retain its fault boundary: {replay}");
}

#[tokio::test]
async fn secure_domain_and_partitioned_cookies_are_scoped_before_scripts() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let top = "https://app.example.test";
    let child = "https://editor.other.test";
    let mut domain = cookie(top,"shared","yes"); domain["domain"] = json!(".example.test"); domain["secure"] = json!(true);
    let mut partitioned = cookie(child,"session","current");
    partitioned["secure"] = json!(true); partitioned["same_site"] = json!("none");
    partitioned["partition"] = json!({"top_level_site":"https://example.test","has_cross_site_ancestor":true});
    domain["expires_at_ms"] = json!(946_684_801_001u64);
    partitioned["expires_at_ms"] = json!(946_684_801_001u64);
    let mut foreign = partitioned.clone(); foreign["value"] = json!("wrong-partition");
    foreign["partition"]["top_level_site"] = json!("https://elsewhere.test");
    let main = format!("<script>window.boot=document.cookie;window.childrenSeen={{}};addEventListener('message',e=>childrenSeen[e.origin]=e.data)</script><iframe src='https://sub.example.test/frame'></iframe><iframe src='{child}/frame'></iframe>");
    let rules: Vec<_> = [("main",format!("{top}/start"),main), ("sub","https://sub.example.test/frame".into(),"<script>parent.postMessage(document.cookie,'*')</script>".into()),("child",format!("{child}/frame"),"<script>parent.postMessage(document.cookie,'*')</script>".into())].into_iter().map(|(id,url,body)|json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}})).collect();
    let launched = m.call_tool("launch",json!({"url":format!("{top}/start"),"then":{"type":"none"},"rules":rules,"profile":{"initial_cookies":[domain,cookie(top,"host","only"),partitioned,foreign]}})).await;
    let id = &launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"Object.fromEntries(boot.split('; ').map(s=>s.split('=')))").await,json!({"shared":"yes","host":"only"}));
    assert_eq!(evaluate(&mut m,id,"childrenSeen").await,json!({"https://sub.example.test":"shared=yes","https://editor.other.test":"session=current"}));
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":901}})).await;
    assert_eq!(evaluate(&mut m,id,"document.cookie").await,json!("host=only"));
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"childrenSeen").await,json!({"https://sub.example.test":"","https://editor.other.test":""}));
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn invalid_cookie_scopes_are_rejected_before_launch() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let cookie_schema = m.definition("state", "recipe", "InitialCookie").await;
    assert!(cookie_schema["properties"]["partition"].is_object());
    assert!(cookie_schema["properties"]["expires_at_ms"].is_object());
    assert!(m.definition("state", "recipe", "InitialStorage").await["properties"]["origin"].is_object());
    let valid = cookie("https://app.example.test","session","value");
    for (field,value) in [("origin",json!("https://user@app.example.test")),("domain",json!(".other.test")),("path",json!("relative")),("name",json!("invalid;name")),("value",json!("line\nbreak")),("same_site",json!("none")),("expires",json!(12345)),("expires_at_ms",json!(946_684_800_000u64)),("expires_at_ms",json!(981_244_800_001u64)),("expires_at_ms",json!(-1)),("expires_at_ms",json!(946_684_800_001.5))] {
        let mut invalid = valid.clone(); invalid[field] = value;
        let (failed,error) = m.try_tool("launch",json!({"url":"https://app.example.test/start","profile":{"initial_cookies":[invalid]}})).await;
        assert!(failed && error["code"] == "invalid_arguments","{field}: {error}");
    }
    let (failed,error) = m.try_tool("launch",json!({"url":"https://app.example.test/start","profile":{"initial_cookies":[valid.clone(),valid]}})).await;
    assert!(failed && error["code"] == "invalid_arguments","{error}");
}

#[tokio::test]
async fn persistent_cookies_expire_on_the_controlled_clock_and_replay() {
    let origin = format!("http://{}", serve_cookies());
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let epoch = 1_893_456_000_000u64;
    let mut visible = cookie(&origin, "locale", "ko");
    visible["expires_at_ms"] = json!(epoch + 1001);
    let mut auth = cookie(&origin, "login", "signed-in");
    auth["http_only"] = json!(true);
    auth["expires_at_ms"] = json!(epoch + 1001);
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"then":{"type":"none"},"profile":{"initial_date_ms":epoch,"initial_cookies":[visible,auth]}})).await;
    let id = &launched["launch_id"];
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":1000}})).await;
    assert_eq!(evaluate(&mut m,id,"boot").await, json!({"request":{"login":"signed-in","locale":"ko"},"visible":"locale=ko"}));
    assert_eq!(evaluate(&mut m,id,"document.cookie").await, json!("locale=ko"));
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":1}})).await;
    assert_eq!(evaluate(&mut m,id,"document.cookie").await, json!(""));
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m,id,"boot").await, json!({"request":{},"visible":""}));
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let events: Vec<Value> = trace.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.iter().filter(|e| e["kind"] == "cookies_initialized").count(), 1);
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Cookies and storage keys are readable natively, without script evaluation: HttpOnly cookies the
/// page cannot see are included, path scope follows the document URL, and both fields are read only.
#[tokio::test]
async fn cookies_and_storage_keys_are_read_natively_and_only_read() {
    let origin = format!("http://{}", serve_cookies());
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut auth = cookie(&origin, "login", "signed-in"); auth["http_only"] = json!(true);
    let mut scoped = cookie(&origin, "private", "only-here"); scoped["path"] = json!("/private");
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"profile":{
        "initial_cookies":[auth,cookie(&origin,"locale","ko"),scoped],
        "initial_storage":{"origin":origin,"local":{"b":"2","a":"1"},"session":{"tab":"x"}}}})).await;
    let id = &launched["launch_id"];
    let target = json!({"by":"css","selector":"p"});
    let read = |field: Value| json!({"launch_id":id,"operation":"read","target":target,"field":field});
    let cookies = m.call_tool("state", read(json!({"type":"cookies"}))).await["value"].clone();
    let names: Vec<_> = cookies.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["locale", "login"], "the /private cookie is out of the document's path: {cookies}");
    assert_eq!(cookies[1]["http_only"], true);
    assert_eq!(cookies[1]["value"], "signed-in");
    assert_eq!(cookies[0]["same_site"], "lax");
    assert_eq!(cookies[0]["expires_at_ms"], Value::Null, "a session cookie");
    assert_eq!(evaluate(&mut m, id, "document.cookie").await, "locale=ko", "the page itself sees no HttpOnly cookie");

    let scope = m.call_tool("state", json!({"launch_id":id,"operation":"storage_scope","target":target})).await["scope"].clone();
    let keys = m.call_tool("state", read(json!({"type":"storage_keys","area":"local","scope":scope}))).await["value"].clone();
    let mut keys: Vec<_> = keys.as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_string()).collect();
    keys.sort();
    assert_eq!(keys, ["a", "b"]);
    let session = m.call_tool("state", read(json!({"type":"storage_keys","area":"session","scope":scope}))).await["value"].clone();
    assert_eq!(session, json!(["tab"]));

    let boundary = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene","max_nodes":1}})).await["boundary"].clone();
    for field in [json!({"type":"cookies"}), json!({"type":"storage_keys","area":"local","scope":scope})] {
        let (refused, error) = m.try_tool("state", json!({"launch_id":id,"operation":"apply","boundary":boundary,
            "changes":[{"target":target,"field":field,"value":"x"}]})).await;
        assert!(refused);
        assert_eq!(error["code"], "invalid_arguments", "{error}");
    }
    m.call_tool("stop", json!({"launch_id":id})).await;
}

/// A cookie is set or deleted at a stopped boundary in the browser's store, read back the same way and
/// replayed: the page no longer sees a deleted one, the next request carries an HttpOnly one, and one
/// given an expiry lapses on the controlled clock.
#[tokio::test]
async fn cookies_are_set_and_deleted_while_stopped_and_replayed() {
    let origin = format!("http://{}", serve_cookies());
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"then":{"type":"none"},"profile":{"initial_cookies":[cookie(&origin,"session","ada")]}})).await;
    let id = &launched["launch_id"];
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    let target = json!({"by":"css","selector":"p"});
    let field = |name: &str| json!({"type":"cookie","name":name});
    let read = |name: &str| json!({"launch_id":id,"operation":"read","target":target,"field":field(name)});
    assert_eq!(m.call_tool("state", read("session")).await["value"], json!({"value":"ada","secure":false,"http_only":false,"same_site":"lax"}));
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let (refused, error) = m.try_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":target,"field":field("session"),"value":null,"expected":{"value":{"value":"bob"}}}]})).await;
    let message = error["message"].as_str().unwrap_or_default();
    assert!(refused && error["code"] == "observation_changed" && message.contains("\"ada\"") && message.contains("\"bob\""), "both values named: {error}");
    // The cookie list gives a session cookie's expiry as null.
    let applied = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":target,"field":field("session"),"value":null,"expected":{"value":{"value":"ada","expires_at_ms":null,"secure":false,"http_only":false,"same_site":"lax"}}},
        {"target":target,"field":field("token"),"value":{"value":"t1","http_only":true}}]})).await;
    assert_eq!(applied["completed"], true, "{applied}");
    assert_eq!(applied["receipts"][0]["after"], Value::Null);
    assert_eq!(applied["receipts"][1]["after"], json!({"value":"t1","secure":false,"http_only":true,"same_site":"lax"}));
    assert_eq!(evaluate(&mut m, id, "document.cookie").await, json!(""), "deleted, and HttpOnly unseen");
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"reload"},"then":{"type":"none"}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
    assert_eq!(evaluate(&mut m, id, "boot.request").await, json!({"token":"t1"}), "the next request carries it");
    // An expiry on the controlled clock: absolute milliseconds after now.
    let now = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().unwrap() / 1000;
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let expires = 946_684_800_000u64 + now + 1000;
    let expiring = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":target,"field":field("locale"),"value":{"value":"ko","expires_at_ms":expires}}]})).await;
    assert_eq!(expiring["receipts"][0]["after"]["expires_at_ms"], expires, "{expiring}");
    let (refused, error) = m.try_tool("state", json!({"launch_id":id,"operation":"apply","boundary":expiring["boundary"],"changes":[
        {"target":target,"field":field("locale"),"value":{"value":"ko","expires_at_ms":expires - 2000}}]})).await;
    assert!(refused && error["code"] == "invalid_arguments", "an expiry before now: {error}");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":1001}})).await;
    assert_eq!(m.call_tool("state", read("locale")).await["value"], Value::Null, "lapsed");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}
