//! App state providers: an app registers them (`frontcapState.register`) and the agent reads and writes
//! them as state fields, a write being an intervention like a document change, replayed with the run.
mod support;
use serde_json::{Value, json};

fn body() -> Value { json!({"by":"css","selector":"body"}) }

#[tokio::test]
async fn atomic_provider_requests_are_refused_before_any_effect_or_branch() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let described = m.call_tool("state",json!({"operation":"describe","of":"apply"})).await;
    assert_eq!(described["atomic_changes"]["supported"],false,"{described}");
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/app-state.html")})).await;
    let id = &launched["launch_id"];
    let read = json!({"launch_id":id,"operation":"read","target":{"by":"document"},"field":{"type":"app","provider":"cart"}});
    let before = m.call_tool("state",read.clone()).await;
    let change = json!({"target":{"by":"document"},"field":{"type":"app","provider":"cart"},"value":{"items":[{"sku":"A","qty":1}]}});
    let changes = json!([change,{"target":{"by":"document"},"field":{"type":"app","provider":"sync"},"value":{"pending":9}}]);
    for batch in [json!([change]), changes.clone()] {
        let (failed,error) = m.try_tool("state",json!({"launch_id":id,"request_id":"atomic","operation":"apply",
            "boundary":before["boundary"],"changes":batch,"atomic":true,"progress_ms":1000})).await;
        assert!(failed && error["code"]=="unsupported_capability","{error}");
        assert!(error["message"].as_str().unwrap().contains("transactional provider"));
        let after = m.call_tool("state",read.clone()).await;
        assert_eq!(after["value"],before["value"],"no applied prefix on atomic refusal");
        assert_eq!(after["boundary"],before["boundary"],"refusal leaves time and state revision unchanged");
    }
    let saved = m.call_tool("state",json!({"operation":"save","kind":"recipe","recipe":{"url":format!("http://{addr}/app-state.html")}})).await;
    let count=std::fs::read_dir(ws.root().join("runtimes")).unwrap().count();
    let (failed,error)=m.try_tool("state",json!({"operation":"probe","saved_id":saved["saved_id"],"kind":"recipe",
        "steps":[{"step":"apply","changes":changes,"atomic":true}]})).await;
    assert!(failed && error["code"]=="unsupported_capability","{error}");
    assert_eq!(std::fs::read_dir(ws.root().join("runtimes")).unwrap().count(),count);
    let applied=m.call_tool("state",json!({"launch_id":id,"request_id":"atomic","operation":"apply",
        "boundary":before["boundary"],"changes":[change],"atomic":false})).await;
    assert_eq!(applied["completed"],true,"explicit ordered application remains available: {applied}");
    assert_eq!(m.call_tool("state",read).await["value"],json!({"items":[{"sku":"A","qty":1}]}));
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn recipes_branches_and_probes_grant_bounded_progress_to_app_writes() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let change = |pending| json!({"target":{"by":"document"},"field":{"type":"app","provider":"sync"},"value":{"pending":pending}});
    let recipe = json!({"url":format!("http://{addr}/app-state.html"),
        "barrier":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#count"},"text":"0 items"},"max_ms":1000},
        "changes":[change(1)],"progress_ms":5000});
    let saved = m.call_tool("state",json!({"operation":"save","kind":"recipe","recipe":recipe})).await;
    let described = m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    assert_eq!(described["progress_ms"],5000,"{described}");
    let branch = json!({"operation":"branch","saved_id":saved["saved_id"],"kind":"recipe","request_id":"progress-branch","changes":[change(2)],"progress_ms":5000});
    let child = m.call_tool("state",branch.clone()).await;
    assert_eq!(child["changes"]["receipts"][0]["after"],json!({"pending":1}),"{child}");
    assert_eq!(child["branch_changes"]["receipts"][0]["after"],json!({"pending":2}),"{child}");
    for key in ["changes","branch_changes"] {
        let progressed = child[key]["receipts"][0]["progressed_us"].as_u64().unwrap();
        assert!((100_000..=117_000).contains(&progressed),"{child}");
    }
    assert_eq!(m.call_tool("state",branch.clone()).await,child,"construction retries never repeat writes");
    let mut conflict = branch; conflict["progress_ms"]=json!(4000);
    let (failed,error)=m.try_tool("state",conflict).await;
    assert!(failed && error["code"]=="request_id_conflict","{error}");
    let args=json!({"operation":"probe","saved_id":saved["saved_id"],"kind":"recipe","request_id":"progress-probe",
        "steps":[
            {"step":"apply","changes":[change(3)],"progress_ms":5000},
            {"step":"state","target":{"by":"document"},"field":{"type":"app","provider":"sync"}}
        ]});
    let probed=m.call_tool("state",args.clone()).await;
    assert_eq!(probed["completed"],2,"{probed}");
    assert_eq!(probed["steps"][1]["value"],json!({"pending":3}),"{probed}");
    assert!(probed["steps"][0]["receipts"][0]["progressed_us"].as_u64().unwrap()>=100_000);
    assert_eq!(m.call_tool("state",args).await,probed);
    let count = std::fs::read_dir(ws.root().join("runtimes")).unwrap().count();
    // A write's grant is at most 10 s; the steps' total is not bounded.
    let (failed,error)=m.try_tool("state",json!({"operation":"probe","saved_id":saved["saved_id"],"kind":"recipe",
        "steps":[{"step":"apply","changes":[change(4)],"progress_ms":10_001}]})).await;
    assert!(failed && error["code"]=="invalid_arguments","{error}");
    assert_eq!(std::fs::read_dir(ws.root().join("runtimes")).unwrap().count(),count);
    for id in [&child["launch_id"],&probed["launch_id"]] {
        m.call_tool("stop",json!({"launch_id":id})).await;
        let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
}

#[tokio::test]
async fn an_apps_state_providers_are_read_written_and_replayed() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/app-state.html")})).await;
    let id = &launched["launch_id"];
    let read = |field: Value| json!({"launch_id":id,"operation":"read","target":body(),"field":field});
    let providers = m.call_tool("state", read(json!({"type":"app_providers"}))).await;
    assert_eq!(providers["value"], json!([
        {"id":"cart","writable":true,"description":"the items in the cart","max_ms":0},
        {"id":"session","writable":false,"description":"","max_ms":0},
        {"id":"sync","writable":true,"description":"","max_ms":200},
        {"id":"notes","writable":true,"description":"","max_ms":0},
        {"id":"profile","writable":false,"description":"","max_ms":0}]), "{providers}");
    assert_eq!(m.call_tool("state", read(json!({"type":"app","provider":"cart"}))).await["value"], json!({"items":[]}));
    // A promise read settles on microtasks.
    assert_eq!(m.call_tool("state", read(json!({"type":"app","provider":"profile"}))).await["value"], json!({"plan":"pro"}));
    let (failed, error) = m.try_tool("state", read(json!({"type":"app","provider":"wishlist"}))).await;
    assert!(failed && error["code"] == "invalid_arguments" && error["message"].as_str().unwrap().contains("no state provider wishlist"), "{error}");
    // A snapshot's text reference stands for its element.
    let snapshot = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"snapshot"}})).await;
    let text = snapshot["nodes"].as_array().unwrap().iter().find(|n| n["role"] == "text" && n["name"] == "0 items").unwrap()["ref"].clone();
    let by_text = m.call_tool("state", json!({"launch_id":id,"operation":"read","target":{"by":"ref","id":text},"field":{"type":"app_providers"}})).await;
    assert_eq!(by_text["value"][0]["id"], "cart", "{by_text}");
    // A nested operation is explained.
    let (failed, error) = m.try_tool("state", json!({"launch_id":id,"operation":{"operation":"read"}})).await;
    assert!(failed && error["message"].as_str().unwrap().contains("`operation` is a string"), "{error}");

    // A write is checked against its expected value, applied by the provider, and read back from it.
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let cart = json!({"items":[{"sku":"A-1","qty":2},{"sku":"B-7","qty":1}]});
    let applied = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":body(),"field":{"type":"app","provider":"cart"},"value":cart,"expected":{"value":{"items":[]}}}]})).await;
    assert_eq!(applied["completed"], true, "{applied}");
    assert_eq!(applied["receipts"][0]["after"], cart, "{applied}");
    let count = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#count"},"props":["text"]}})).await;
    assert_eq!(count["values"]["text"], "2 items", "the app rendered its own state");

    // A write that persists to IndexedDB settles at this instant, once the page's work queued for it ran.
    let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].clone();
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let persisted = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":body(),"field":{"type":"app","provider":"notes"},"value":{"text":"ship it"}}]})).await;
    assert_eq!(persisted["completed"], true, "{persisted}");
    assert_eq!((&persisted["receipts"][0]["after"], &persisted["receipts"][0]["served_instant"]), (&json!({"text":"ship it"}), &json!(true)), "{persisted}");
    assert_eq!(m.call_tool("status", json!({"launch_id":id})).await["t_us"], before, "no virtual time passed");
    let shown = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#notes"},"props":["text"]}})).await;
    assert_eq!(shown["values"]["text"], "ship it");
    // An evaluation's promise is awaited at the instant too: what the write persisted reads back from
    // IndexedDB; one that waits for a timer is refused.
    let stored = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":
        "new Promise((done) => { const open = indexedDB.open('app', 1); open.onsuccess = () => { const get = open.result.transaction('notes').objectStore('notes').get('text'); get.onsuccess = () => done(get.result); }; })"})).await;
    assert_eq!(stored["value"], "ship it", "{stored}");
    let (failed, error) = m.try_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"new Promise((done) => setTimeout(done, 100))"})).await;
    assert!(failed && error["message"].as_str().unwrap().contains("did not settle at this instant"), "{error}");
    assert_eq!(m.call_tool("status", json!({"launch_id":id})).await["t_us"], before, "still no virtual time passed");

    // A read-only provider is refused before any write; one whose promise needs a later instant times out.
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let (failed, error) = m.try_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":body(),"field":{"type":"app","provider":"session"},"value":{"user":"ada"}}]})).await;
    assert!(failed && error["code"] == "invalid_arguments" && error["message"].as_str().unwrap().contains("read only"), "{error}");
    let late = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":body(),"field":{"type":"app","provider":"sync"},"value":{"pending":1}}]})).await;
    assert_eq!(late["completed"], false, "{late}");
    assert_eq!(late["receipts"][0]["attempted"], true);
    assert!(late["receipts"][0]["error"].as_str().unwrap().contains("no progress was granted"), "{late}");
    // Given progress, within the provider's own 200 ms, a timer-backed write settles at the frame tick its
    // timer ran by; a grant shorter than its timer times out after it.
    let short = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":late["boundary"],"progress_ms":50,"changes":[
        {"target":body(),"field":{"type":"app","provider":"sync"},"value":{"pending":2}}]})).await;
    assert!(short["receipts"][0]["error"].as_str().unwrap().contains("within 50 ms"), "{short}");
    assert_eq!(short["receipts"][0]["progressed_us"], 50_000, "{short}");
    let started = short["boundary"]["t_us"].as_u64().unwrap();
    let granted = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":short["boundary"],"progress_ms":5000,"changes":[
        {"target":body(),"field":{"type":"app","provider":"sync"},"value":{"pending":3}}]})).await;
    assert_eq!(granted["completed"], true, "{granted}");
    assert_eq!(granted["receipts"][0]["after"], json!({"pending":3}), "{granted}");
    let progressed = granted["receipts"][0]["progressed_us"].as_u64().unwrap();
    assert!((100_000..=117_000).contains(&progressed), "the timer's 100 ms, to the frame tick after: {granted}");
    assert_eq!(granted["boundary"]["t_us"].as_u64().unwrap(), started + progressed, "{granted}");

    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A provider belongs to the document that registered it: a frame's is reached through a target inside
/// the frame, and the top document's providers do not include it.
#[tokio::test]
async fn a_frames_provider_is_reached_through_the_frame() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/app-state-frame.html")})).await;
    let id = &launched["launch_id"];
    let unread = json!({"by":"frame","frame":{"by":"css","selector":"#chat"},"target":{"by":"css","selector":"#unread"}});
    let read = |target: &Value, field: Value| json!({"launch_id":id,"operation":"read","target":target,"field":field});
    let count = json!({"by":"css","selector":"#count"});
    let top = m.call_tool("state", read(&count, json!({"type":"app_providers"}))).await;
    assert_eq!(top["value"].as_array().unwrap().iter().map(|p| p["id"].clone()).collect::<Vec<_>>(), [json!("cart")]);
    let framed = m.call_tool("state", read(&unread, json!({"type":"app_providers"}))).await;
    assert_eq!(framed["value"][0]["id"], "chat");
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let applied = m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":unread,"field":{"type":"app","provider":"chat"},"value":{"unread":3}}]})).await;
    assert_eq!(applied["completed"], true, "{applied}");
    let text = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":unread,"props":["text"]}})).await;
    assert_eq!(text["values"]["text"], "3 unread");
    // A document locator names one document's root however many frames the page has: the top one, or a
    // frame scope's.
    let document = json!({"by":"document"});
    let top = m.call_tool("state", read(&document, json!({"type":"app_providers"}))).await;
    assert_eq!(top["value"][0]["id"], "cart", "{top}");
    let chat = json!({"by":"frame","frame":{"by":"css","selector":"#chat"},"target":document});
    assert_eq!(m.call_tool("state", read(&chat, json!({"type":"app","provider":"chat"}))).await["value"], json!({"unread":3}));
    m.call_tool("stop", json!({"launch_id":id})).await;
}

/// A recipe constructs its starting state through the app's provider at its barrier, and each child
/// starts from it.
#[tokio::test]
async fn a_recipe_builds_its_starting_state_through_an_app_provider() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let recipe = json!({
        "url": format!("http://{addr}/app-state.html"),
        "barrier": {"type": "until", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#count"}, "text": "0 items"}, "max_ms": 1000},
        "changes": [{"target": body(), "field": {"type": "app", "provider": "cart"}, "value": {"items": [{"sku": "A-1", "qty": 1}]}}],
        "verify": [{"name": "one-item", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#count"}, "text": "1 items"}}],
    });
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe})).await;
    let child = m.call_tool("state", json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "recipe", "request_id": "b-1"})).await;
    assert_eq!(child["phase"], "ready", "{child}");
    assert_eq!(child["changes"]["completed"], true, "{child}");
    assert_eq!(child["verification"], json!([{"name": "one-item", "value": "true"}]));
    let cart = m.call_tool("state", json!({"launch_id": child["launch_id"], "operation": "read", "target": body(), "field": {"type": "app", "provider": "cart"}})).await;
    assert_eq!(cart["value"], json!({"items": [{"sku": "A-1", "qty": 1}]}));
    m.call_tool("stop", json!({"launch_id": child["launch_id"]})).await;
    // A probe reads a state field as state read does.
    let probed = m.call_tool("state", json!({"operation": "probe", "saved_id": saved["saved_id"], "kind": "recipe", "steps": [
        {"step": "state", "target": body(), "field": {"type": "app", "provider": "cart"}}]})).await;
    assert_eq!(probed["steps"][0]["value"], json!({"items": [{"sku": "A-1", "qty": 1}]}), "{probed}");
    m.call_tool("stop", json!({"launch_id": probed["launch_id"]})).await;
}

/// A recipe saved from a live generation reconstructs what its frozen boundary has: the cookies and
/// storage the user's click set, and the cart and notes written through the app's providers, the notes'
/// write settling as their IndexedDB transaction completes. It names where it came from and what it leaves
/// out.
#[tokio::test]
async fn a_recipe_saved_from_a_live_generation_reconstructs_its_state() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/app-state.html?live")})).await;
    let id = &launched["launch_id"];
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#remember"}}})).await;
    let scene = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let cart = json!({"items":[{"sku":"A-1","qty":2}]});
    m.call_tool("state", json!({"launch_id":id,"operation":"apply","boundary":scene["boundary"],"changes":[
        {"target":{"by":"document"},"field":{"type":"app","provider":"cart"},"value":cart},
        {"target":{"by":"document"},"field":{"type":"app","provider":"notes"},"value":{"text":"gift wrap"}}]})).await;

    // Databases holding a date or a sparse array as long as an array can be are named as left out; the
    // notes' database is recreated.
    for (name, value) in [("audit", "{at: new Date(0)}"), ("sparse", "{items: new Array(4294967295)}")] {
        m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":format!(
            "new Promise((done) => {{ const open = indexedDB.open('{name}', 1); open.onupgradeneeded = () => open.result.createObjectStore('events', {{autoIncrement: true}}); open.onsuccess = () => {{ const tx = open.result.transaction('events', 'readwrite'); tx.objectStore('events').put({value}); tx.oncomplete = () => {{ open.result.close(); done(true); }}; }}; }})")})).await;
    }

    // The rules the session answers dialogs by go with it: confirming clears the cart.
    m.call_tool("session", json!({"launch_id":id,"operation":"dialog_rules","rules":[{"kind":"confirm","answer":"accept"}]})).await;
    // What each child must show to be ready (app-state rerun: a live recipe could not be given any).
    let verify = json!([{"name":"cart-shown","predicate":{"is":"text_equals","target":{"by":"css","selector":"#count"},"text":"1 items"}}]);
    let saved = m.call_tool("state", json!({"operation":"save","kind":"recipe","from":id,"verify":verify})).await;
    assert_eq!(saved["origin"]["launch_id"], *id, "{saved}");
    assert_eq!(saved["verify_at_source"], json!([{"name":"cart-shown","value":"true"}]), "the source shows what each child must: {saved}");
    assert!(saved["origin"]["boundary"]["t_us"].as_u64().unwrap() > 0, "{saved}");
    for name in ["audit", "sparse"] {
        assert!(saved["excluded"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains(&format!("IndexedDB database {name}"))), "{name}: {saved}");
    }
    let described = m.call_tool("state", json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    assert_eq!(described["origin"], saved["origin"]);
    assert_eq!(described["initial_state"]["dom_storage"]["local"][0]["keys"], json!(["remembered"]), "{described}");
    assert_eq!(described["initial_state"]["cookies"][0]["name"], "remember", "{described}");
    assert_eq!(described["changes"], json!([
        {"target":{"by":"document"},"field":{"type":"app","provider":"cart"}},
        {"target":{"by":"document"},"field":{"type":"app","provider":"notes"}}]), "{described}");
    assert_eq!(described["dialog_rules"], json!([{"kind":"confirm","answer":"accept"}]), "{described}");
    assert_eq!(described["initial_state"]["indexed_db"].as_array().unwrap().iter().map(|d| d["name"].clone()).collect::<Vec<_>>(), [json!("app")], "{described}");
    assert_eq!(described["verify"], verify, "{described}");
    assert!(described["passthrough_origins"].is_null() || described["passthrough_origins"].is_array(), "{described}");

    let child = m.call_tool("state", json!({"operation":"branch","saved_id":saved["saved_id"],"kind":"recipe","request_id":"from-live"})).await;
    assert_eq!(child["phase"], "ready", "{child}");
    assert_eq!(child["verification"], json!([{"name":"cart-shown","value":"true"}]), "{child}");
    let c = &child["launch_id"];
    let read = |field: Value| json!({"launch_id":c,"operation":"read","target":{"by":"document"},"field":field});
    assert_eq!(m.call_tool("state", read(json!({"type":"app","provider":"cart"}))).await["value"], cart);
    assert_eq!(m.call_tool("state", read(json!({"type":"app","provider":"notes"}))).await["value"], json!({"text":"gift wrap"}));
    let child_scope = m.call_tool("state", json!({"launch_id":c,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let stored_notes = m.call_tool("state", read(json!({"type":"indexed_db","scope":child_scope,"database":"app","store":"notes","key":"text"}))).await;
    assert_eq!(stored_notes["value"], "gift wrap", "the child's IndexedDB holds what the source's did: {stored_notes}");
    assert_eq!(child["changes"]["receipts"][1]["served_instant"], true, "{child}");
    let cookies = m.call_tool("state", read(json!({"type":"cookies"}))).await;
    assert_eq!((cookies["value"][0]["name"].as_str(), cookies["value"][0]["value"].as_str()), (Some("remember"), Some("yes")), "{cookies}");
    let scope = m.call_tool("state", json!({"launch_id":c,"operation":"storage_scope","target":{"by":"document"}})).await["scope"].clone();
    let stored = m.call_tool("state", read(json!({"type":"storage","area":"local","scope":scope,"key":"remembered"}))).await;
    assert_eq!(stored["value"], "yes");
    let count = m.call_tool("observe", json!({"launch_id":c,"op":{"type":"query","target":{"by":"css","selector":"#count"},"props":["text"]}})).await;
    assert_eq!(count["values"]["text"], "1 items", "the app rendered the reconstructed cart");
    m.call_tool("act", json!({"launch_id":c,"action":{"type":"click","target":{"by":"css","selector":"#clear"}}})).await;
    let cleared = m.call_tool("observe", json!({"launch_id":c,"op":{"type":"query","target":{"by":"css","selector":"#count"},"props":["text"]}})).await;
    assert_eq!(cleared["values"]["text"], "0 items", "the child confirmed by the saved rules");
    for g in [c, id] { m.call_tool("stop", json!({"launch_id":g})).await; }
}
