//! Saved recipes and independent recipe branches through the public MCP surface: an unusual starting
//! state (a large cart, a signed-in session, a failing payment route) explored as separate children.
mod support;
use serde_json::{Value, json};

fn cart() -> String {
    let items: Vec<Value> = (0..120).map(|i| json!({"name": format!("item {i}"), "price": 1.25})).collect();
    serde_json::to_string(&items).unwrap()
}

fn recipe(origin: &str) -> Value {
    json!({
        "url": format!("{origin}/checkout.html"),
        "profile": {
            "initial_storage": {"origin": origin, "local": {"cart": cart()}},
            "initial_cookies": [{"origin": origin, "name": "session", "value": "ada", "path": "/", "same_site": "lax"}],
        },
        "rules": [{"id": "pay", "match": {"method": "POST", "url": format!("{origin}/api/pay")}, "action": {"type": "sequence",
            "after_last": "repeat_last", "steps": [{"status": 402, "body": "declined", "latency_ms": 1000}, {"status": 200, "body": "ok"}]}}],
        "barrier": {"type": "until", "predicate": {"is": "attached", "target": {"by": "test_id", "id": "cart-ready"}}, "max_ms": 2000},
        "changes": [{"target": {"by": "css", "selector": "#note"}, "field": {"type": "value"}, "value": "leave at door"}],
        "verify": [
            {"name": "signed-in", "predicate": {"is": "text_equals", "target": {"by": "test_id", "id": "user"}, "text": "Signed in as ada"}},
            {"name": "cart-loaded", "predicate": {"is": "text_equals", "target": {"by": "test_id", "id": "cart-count"}, "text": "120 items"}},
        ],
    })
}

fn css(selector: &str) -> Value {
    json!({"by": "css", "selector": selector})
}

async fn text(m: &mut support::Mcp, id: &Value, selector: &str) -> Value {
    m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": css(selector), "props": ["text"]}})).await["values"]["text"].clone()
}

async fn click(m: &mut support::Mcp, id: &Value, selector: &str, settle: bool) {
    let then = if settle { json!({"type": "settle", "idle_window_ms": 50, "max_ms": 3000}) } else { json!({"type": "none"}) };
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": css(selector)}, "then": then})).await;
}

async fn stored_cart(m: &mut support::Mcp, id: &Value) -> Value {
    let scope = m.call_tool("state", json!({"launch_id": id, "operation": "storage_scope", "target": css("#note")})).await["scope"].clone();
    let field = json!({"type": "storage", "area": "local", "scope": scope, "key": "cart"});
    m.call_tool("state", json!({"launch_id": id, "operation": "read", "target": css("#note"), "field": field})).await["value"].clone()
}

fn children(ws: &support::Workspace) -> usize {
    std::fs::read_dir(ws.root().join("runtimes")).map_or(0, |d| d.count())
}

#[tokio::test]
async fn recipe_children_explore_alternatives_independently() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;

    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe(&origin)})).await;
    let saved_id = saved["saved_id"].clone();
    assert_eq!(saved["kind"], "recipe");
    assert_eq!(m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe(&origin)})).await, saved, "same content, same basis");
    let described = m.call_tool("state", json!({"operation": "describe_saved", "saved_id": saved_id})).await;
    assert_eq!(described["sha256"], saved["sha256"]);
    assert_eq!(described["verify"][1]["name"], "cart-loaded");
    assert_eq!(described["verify"][1]["predicate"]["text"], "120 items", "predicates are shown in full");
    assert_eq!(described["initial_state"]["cookies"][0]["name"], "session");
    assert!(described["initial_state"]["cookies"][0].get("value").is_none(), "values stay private");
    assert_eq!(described["initial_state"]["storage"]["local_keys"], json!(["cart"]));
    assert_eq!(described["rules"][0]["match"]["method"], "POST");
    assert_eq!(described["rules"][0]["answers"], json!([{"status": 402, "latency_ms": 1000}, {"status": 200, "latency_ms": 0}]), "statuses and latencies, not bodies");
    assert_eq!(described["changes"][0]["field"]["type"], "value");
    assert!(!described.to_string().contains("leave at door"), "change values stay private");
    assert_eq!(children(&ws), 0, "saving launches nothing");

    let branch = |request: &str| json!({"operation": "branch", "saved_id": saved_id, "kind": "recipe", "request_id": request});
    let success = m.call_tool("state", branch("success")).await;
    assert_eq!(success["phase"], "ready");
    assert_eq!(success["origin"]["saved_id"], saved_id);
    assert_eq!(success["verification"], json!([{"name": "signed-in", "value": "true"}, {"name": "cart-loaded", "value": "true"}]));
    assert_eq!(success["barrier"]["outcome"]["type"], "condition_met", "{success}");
    assert_eq!(success["changes"]["completed"], true);
    assert_eq!(success["branch_changes"], Value::Null);
    let a = success["launch_id"].clone();
    let origin_file: Value = serde_json::from_str(&std::fs::read_to_string(std::path::Path::new(success["bundle"].as_str().unwrap()).join("origin.json")).unwrap()).unwrap();
    assert_eq!(origin_file["saved_id"], saved_id);
    assert_eq!(origin_file["sha256"], saved["sha256"]);
    let ready_at = success["clock"]["t_us"].as_u64().unwrap();
    assert!((100_000..2_000_000).contains(&ready_at), "the child's own clock reached the barrier: {ready_at}");

    // Success: a rule revision in this child only.
    let rules = m.call_tool("network", json!({"launch_id": a, "operation": "list_rules"})).await;
    m.call_tool("network", json!({"launch_id": a, "operation": "add_rule", "expected_revision": 0, "boundary": rules["boundary"], "before": "pay",
        "rule": {"id": "pay-ok", "match": {"method": "POST", "url": format!("{origin}/api/pay")}, "action": {"type": "mock", "status": 200, "body": "ok"}}})).await;
    click(&mut m, &a, "#pay", true).await;
    assert_eq!(text(&mut m, &a, "#status").await, "Payment accepted");
    assert_eq!(stored_cart(&mut m, &a).await, "[]");

    // Failure under the recipe's own route; the same child takes the sequence's next step.
    let failure = m.call_tool("state", branch("failure")).await;
    let b = failure["launch_id"].clone();
    assert_ne!(a, b);
    assert_eq!(text(&mut m, &b, "#note").await, "", "a value is not text");
    click(&mut m, &b, "#pay", true).await;
    assert_eq!(text(&mut m, &b, "#status").await, "Payment declined (402)");
    assert_eq!(stored_cart(&mut m, &b).await, cart(), "the success child's cart change did not leak");
    click(&mut m, &b, "#pay", true).await;
    assert_eq!(text(&mut m, &b, "#status").await, "Payment accepted");

    // Cancellation while the declined answer is still on its way.
    let cancel = m.call_tool("state", branch("cancel")).await;
    let c = cancel["launch_id"].clone();
    click(&mut m, &c, "#pay", false).await;
    assert_eq!(text(&mut m, &c, "#status").await, "Paying…");
    click(&mut m, &c, "#cancel", true).await;
    assert_eq!(text(&mut m, &c, "#status").await, "Payment cancelled");

    // A later child starts from the recipe: no cookie, storage or sequence step of earlier children.
    let later = m.call_tool("state", branch("later")).await;
    let d = later["launch_id"].clone();
    assert_eq!(later["verification"], success["verification"]);
    assert_eq!(later["clock"]["t_us"], success["clock"]["t_us"], "every child reaches the barrier at the same instant");
    let returning = m.call_tool("observe", json!({"launch_id": d, "op": {"type": "query", "target": css("#returning"), "props": ["visible"]}})).await;
    assert_eq!(returning["values"]["visible"], false, "the success child's cookie did not leak");
    click(&mut m, &d, "#pay", true).await;
    assert_eq!(text(&mut m, &d, "#status").await, "Payment declined (402)", "sequence steps are per child");

    // Retrying a branch finds its child; another child's lease does not reach it.
    let count = children(&ws);
    assert_eq!(m.call_tool("state", branch("success")).await["launch_id"], a);
    assert_eq!(children(&ws), count, "a retried branch creates no child");
    let (refused, error) = m.try_tool("act", json!({"launch_id": d, "lease": failure["lease"], "action": {"type": "click", "target": css("#pay")}})).await;
    assert!(refused, "{error}");
    // So is another child's node reference, though both children show the same page.
    let foreign = m.call_tool("observe", json!({"launch_id": b, "op": {"type": "query", "target": css("#pay"), "props": ["text"]}})).await["ref"].clone();
    let (refused, error) = m.try_tool("observe", json!({"launch_id": d, "op": {"type": "query", "target": {"by": "ref", "id": foreign}, "props": ["text"]}})).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "{error}");
    assert!(error["message"].as_str().unwrap().contains("another generation"), "{error}");
    assert_eq!(m.call_tool("state", json!({"operation": "describe_saved", "saved_id": saved_id})).await, described, "the basis is unchanged");

    for id in [&b, &c, &d] {
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
    let history = m.call_tool("watch", json!({"launch_id": d, "operation": "history", "name": "cart-loaded", "changes_only": true})).await;
    assert_eq!(history["samples"][0]["value"], "true", "a stopped child's watch history comes from its evidence: {history}");
    m.call_tool("stop", json!({"launch_id": a})).await;

    // A deleted basis can no longer be branched; its children's evidence is their own.
    assert_eq!(m.call_tool("state", json!({"operation": "delete_saved", "saved_id": saved_id})).await, json!({"deleted": saved_id}));
    for op in [json!({"operation": "describe_saved", "saved_id": saved_id}), branch("after"), json!({"operation": "delete_saved", "saved_id": saved_id})] {
        let (refused, error) = m.try_tool("state", op).await;
        assert!(refused);
        assert_eq!(error["code"], "not_found", "{error}");
    }
    let replay = m.call_tool("debug", json!({"launch_id": a, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn unavailable_methods_and_changed_bases_are_refused_before_any_child() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (refused, error) = m.try_tool("state", json!({"operation": "save", "kind": "checkpoint"})).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "a checkpoint basis comes from a live generation: {error}");
    let (refused, error) = m.try_tool("state", json!({"operation": "save", "kind": "replay", "recipe": recipe(&origin)})).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "a replay basis comes from a stopped generation: {error}");
    let mut portable = recipe(&origin);
    portable["changes"][0]["target"] = json!({"by": "ref", "id": "e3"});
    let (refused, error) = m.try_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": portable})).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "{error}");

    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe(&origin)})).await;
    let saved_id = saved["saved_id"].clone();
    for kind in ["checkpoint", "replay"] {
        let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved_id, "kind": kind})).await;
        assert!(refused);
        assert_eq!(error["code"], "method_mismatch", "{error}");
    }
    let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": "b-0000000000000000", "kind": "recipe"})).await;
    assert!(refused);
    assert_eq!(error["code"], "not_found", "{error}");
    let (refused, error) = m.try_tool("state", json!({"operation": "branch", "launch_id": "g-0000000000000000", "saved_id": saved_id, "kind": "recipe"})).await;
    assert!(refused, "{error}");

    let manifest = ws.root().join("saved").join(saved_id.as_str().unwrap()).join("manifest.json");
    let original = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, original.replace("Signed in as ada", "Signed in as eve")).unwrap();
    let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved_id, "kind": "recipe"})).await;
    assert!(refused);
    assert_eq!(error["code"], "basis_corrupt", "{error}");
    assert_eq!(children(&ws), 0, "no refusal created a child");
}

/// A process image needs the browser in a cell (a Linux host with FRONTCAP_CELLS): elsewhere a checkpoint
/// basis is refused, nothing is kept and the generation goes on.
#[tokio::test]
async fn a_checkpoint_basis_needs_a_browser_in_a_cell() {
    if std::env::var_os("FRONTCAP_CELLS").is_some() {
        return;
    }
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let live = m.call_tool("launch", json!({"url": format!("{origin}/checkout.html"), "then": {"type": "none"}})).await;
    let (refused, error) = m.try_tool("state", json!({"operation": "save", "kind": "checkpoint", "from": live["launch_id"]})).await;
    assert!(refused);
    assert_eq!(error["code"], "unsupported_capability", "{error}");
    assert!(error["message"].as_str().unwrap().contains("outside a cell"), "{error}");
    assert_eq!(std::fs::read_dir(ws.root().join("images")).map_or(0, |d| d.count()), 0, "nothing is kept");
    m.call_tool("wait", json!({"launch_id": live["launch_id"], "op": {"type": "advance", "by_ms": 16}})).await;
    m.call_tool("stop", json!({"launch_id": live["launch_id"]})).await;
}

#[tokio::test]
async fn a_failed_construction_never_publishes_a_ready_child() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut wrong = recipe(&origin);
    wrong["verify"][1]["predicate"]["text"] = json!("99 items");
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": wrong})).await;
    let branch = json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "recipe", "request_id": "wrong-cart"});
    let (refused, error) = m.try_tool("state", branch.clone()).await;
    assert!(refused);
    assert_eq!(error["code"], "construction_failed", "{error}");
    assert!(error["message"].as_str().unwrap().contains("verify: cart-loaded is false"), "{error}");
    let runtimes: Vec<_> = std::fs::read_dir(ws.root().join("runtimes")).unwrap().map(|d| d.unwrap().path()).collect();
    assert_eq!(runtimes.len(), 1);
    let runtime = frontcap_recorder::runtime::load_runtime(&runtimes[0]).unwrap().unwrap();
    assert_eq!(runtime.state, frontcap_recorder::runtime::RuntimeState::Stopped, "the failed child is stopped with its evidence");
    let child = runtimes[0].file_name().unwrap().to_str().unwrap().to_string();
    let bundle = m.call_tool("debug", json!({"launch_id": child, "operation": "bundle"})).await;
    assert_eq!(bundle["finalized"], true, "{bundle}");
    let (refused, again) = m.try_tool("state", branch).await;
    assert!(refused);
    assert_eq!(again["message"], error["message"], "a retry reports the same failure of the same child");
    assert_eq!(children(&ws), 1);

    let mut late = recipe(&origin);
    late["barrier"] = json!({"type": "until", "predicate": {"is": "attached", "target": {"by": "test_id", "id": "never"}}, "max_ms": 300});
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": late})).await;
    let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "recipe"})).await;
    assert!(refused);
    assert!(error["message"].as_str().unwrap().contains("failed at barrier"), "{error}");
}
