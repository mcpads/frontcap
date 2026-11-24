//! What every session loads stays small (emucap's context budget): Server Instructions, the tool list,
//! and the entry call, measured as serialized bytes from the real server.
mod support;

use serde_json::json;

#[tokio::test]
async fn the_always_loaded_surface_stays_within_its_budget() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let instructions = m.instructions().to_string();
    assert!(instructions.len() <= 4096, "Server Instructions: {} bytes", instructions.len());
    for loop_tool in ["launch", "observe", "act", "wait", "stop", "describe"] {
        assert!(instructions.contains(loop_tool), "the instructions name {loop_tool}");
    }
    let tools = m.request("tools/list", json!({})).await;
    let listed = tools["result"].to_string().len();
    assert!(listed <= 24 * 1024, "tools/list: {listed} bytes");
    for t in tools["result"]["tools"].as_array().unwrap() {
        // Only buttons are listed: a feature adds a choice name, never a nested schema.
        assert!(t["inputSchema"].get("$defs").is_none(), "{} lists nested definitions", t["name"]);
        if t["description"].as_str().unwrap().starts_with("Drawer:") {
            assert!(t.to_string().len() <= 1024, "{}: {} bytes", t["name"], t.to_string().len());
            // Tool entries leave routing to the Server Instructions: every drawer is routed there.
            let drawer = t["name"].as_str().unwrap();
            let routed = if drawer == "session" { "session list".to_string() } else { format!("{drawer} describe") };
            assert!(instructions.contains(&routed), "the instructions route {drawer}");
        }
    }
    // Every front-panel tool gives its full schema on request.
    for front in ["launch", "status", "act", "wait", "observe", "capture", "stop"] {
        let described = m.call_tool(front, json!({"describe": true})).await;
        assert!(described["input_schema"]["properties"].is_object(), "{front}: {described}");
    }
    let bootstrap = m.call_tool("bootstrap", json!({})).await.to_string().len();
    assert!(bootstrap <= 1024, "bootstrap: {bootstrap} bytes");
}

/// Every `describe` answer stays within 8 KiB: a larger schema names its definitions, each described
/// alone, so no answer spills out of an agent's context.
#[tokio::test]
async fn every_describe_answer_stays_within_its_budget() {
    const BUDGET: usize = 8 * 1024;
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    for front in ["launch", "status", "act", "wait", "observe", "capture", "stop"] {
        let described = m.call_tool(front, json!({"describe": true})).await.to_string().len();
        assert!(described <= BUDGET, "{front} describe: {described} bytes");
    }
    let mut asked = 0;
    for drawer in ["session", "state", "debug", "input", "network", "watch"] {
        let index = m.call_tool(drawer, json!({"operation": "describe"})).await;
        assert!(index.to_string().len() <= BUDGET, "{drawer} describe: {} bytes", index.to_string().len());
        let mut todo: Vec<String> = index["operations"].as_object().unwrap().keys().cloned().collect();
        if drawer == "state" {
            todo.push("recipe".into());
        }
        while let Some(of) = todo.pop() {
            let described = m.call_tool(drawer, json!({"operation": "describe", "of": of})).await;
            let bytes = described.to_string().len();
            assert!(bytes <= BUDGET, "{drawer} describe of: {of}: {bytes} bytes");
            asked += 1;
            // A definition named only by its describe is reachable, and within budget too.
            for definition in described["schema"]["$defs"].as_object().into_iter().flatten().map(|(_, d)| d) {
                if let Some(of) = definition["describe"]["of"].as_str() {
                    todo.push(of.to_string());
                }
            }
        }
    }
    assert!(asked > 50, "only {asked} describe answers checked");
}

/// A large page answers each loop call in a few kilobytes: repeated rows fold, effects hold their
/// first entries and say what they left out, and each detail stays one call away.
#[tokio::test]
async fn a_large_page_answers_each_step_within_its_budget() {
    const BUDGET: usize = 4 * 1024;
    let rows: String = (0..200).map(|i| format!("<tr><td>Product {i}</td><td>${i}.99</td><td><button>Add {i}</button></td></tr>")).collect();
    let images: String = (0..60).map(|i| format!("<img src='/img/{i}.png' alt='pic {i}'>")).collect();
    let page = format!(
        "<!doctype html><h1>Catalog</h1><table>{rows}</table>{images}\
         <script>for (const b of document.querySelectorAll('button')) b.onclick = () => {{\
           for (const td of document.querySelectorAll('td:nth-child(2)')) td.textContent += '!'; }}</script>"
    );
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([
        {"id": "page", "match": {"url": "https://shop.example.test/"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": page}},
        {"id": "img", "match": {"url": "https://shop.example.test/img/*"}, "action": {"type": "mock", "status": 404, "body": ""}}
    ]);
    let launched = m.call_tool("launch", json!({"url": "https://shop.example.test/", "rules": rules})).await;
    let id = launched["launch_id"].clone();
    assert!(launched.to_string().len() <= BUDGET, "launch: {} bytes", launched.to_string().len());
    assert!(launched["clock"]["date"].as_str().unwrap().starts_with("2000-01-01T00:00:"), "{}", launched["clock"]);
    let effects = &launched["effects"];
    assert_eq!(effects["omitted"]["entries"]["requests"], 41, "{effects}");
    assert_eq!(effects["console_messages"].as_array().unwrap().len(), 1, "{effects}");
    assert_eq!(effects["console_messages"][0]["count"], 60, "{effects}");

    let snapshot = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    assert!(snapshot.to_string().len() <= BUDGET, "snapshot: {} bytes", snapshot.to_string().len());
    let marker = snapshot["nodes"].as_array().unwrap().iter().find(|n| n.get("folded").is_some()).expect("folded rows");
    assert_eq!(marker["folded"]["count"], 197, "{marker}");
    let scene = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "scene"}})).await;
    assert!(scene.to_string().len() <= BUDGET, "scene: {} bytes", scene.to_string().len());

    // A row below the viewport is refused with the scroll that centers it.
    let add = json!({"by": "role", "role": "button", "name": "Add 150"});
    let (refused, error) = m.try_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": add}})).await;
    assert!(refused && error["code"] == "not_actionable", "{error}");
    let recovery = error["recovery"].as_str().unwrap();
    let delta_y: i32 = recovery.split("delta_y ").nth(1).and_then(|r| r.split(',').next()).and_then(|n| n.parse().ok()).expect(recovery);
    assert!(delta_y > 0, "{recovery}");
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "scroll", "target": add, "delta_y": delta_y}})).await;
    let act = m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": add}})).await;
    assert!(act.to_string().len() <= BUDGET, "act: {} bytes", act.to_string().len());
    // Scrolled there, a snapshot keeps the rows on screen, as the image shows them.
    let scrolled = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    assert!(scrolled.to_string().len() <= BUDGET, "scrolled snapshot: {} bytes", scrolled.to_string().len());
    let shown: Vec<u64> = scrolled["nodes"].as_array().unwrap().iter()
        .filter(|n| n["role"] == "cell" && n["name"].as_str().unwrap().starts_with("Product "))
        .map(|n| n["name"].as_str().unwrap()["Product ".len()..].parse().unwrap()).collect();
    assert!(shown.len() == 3 && shown.iter().all(|row| (130..=150).contains(row)), "{shown:?}");
    let diff = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "diff", "since": snapshot["observation_id"]}})).await;
    assert!(diff.to_string().len() <= BUDGET, "diff: {} bytes", diff.to_string().len());

    // Each detail stays one call away.
    let rows = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot", "scope": {"by": "ref", "id": marker["folded"]["scope"]}, "depth": 1, "unfold": true}})).await;
    assert_eq!(rows["nodes"].as_array().unwrap().iter().filter(|n| n["role"] == "row").count(), 200, "{rows}");
    // Counting needs no listing: cells that say "Product 1…" (1, 10–19, 100–199), buttons, and none.
    for (target, n) in [(json!({"by": "text", "text": "Product 1", "contains": true}), 111), (json!({"by": "role", "role": "button"}), 200), (json!({"by": "text", "text": "Overdue"}), 0)] {
        let counted = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "count", "target": target}})).await;
        assert_eq!(counted["count"], n, "{target}: {counted}");
    }
    // Unfolded whole, the page is paged rather than answered in one piece.
    let whole = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot", "unfold": true, "max_nodes": 2000}})).await;
    assert!(whole["result_id"].is_string() && whole["total_bytes"].as_u64().unwrap() > 16 * 1024, "{}", &whole.to_string()[..300]);
    let requests = m.call_tool("debug", json!({"launch_id": id, "operation": "requests"})).await;
    assert_eq!(requests["entries"].as_array().unwrap().len(), 61, "{requests}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}
