//! Window activation remains observable after recorder adoption and in a replay branch.
mod support;

use serde_json::{Value, json};

const REPORT: &str = r#"<output id=events></output><script>
const events = [];
function report() { document.querySelector('#events').textContent = JSON.stringify(events); }
for (const kind of ['focus', 'blur']) addEventListener(kind, () => {
  events.push([kind, Date.now() - 1700000000000, document.hasFocus()]); report();
});
report();
</script>"#;

async fn events(m: &mut support::Mcp, id: &Value, context: &str) -> Value {
    let result = m.call_tool("observe", json!({"launch_id": id, "context": context,
        "op": {"type": "query", "target": {"by": "css", "selector": "#events"}, "props": ["text"]}})).await;
    serde_json::from_str(result["values"]["text"].as_str().unwrap()).unwrap()
}

async fn click(m: &mut support::Mcp, id: &Value, context: &str, selector: &str) {
    let (failed, answer) = m.try_tool("act", json!({"launch_id": id, "context": context,
        "action": {"type": "click", "target": {"by": "css", "selector": selector}},
        "then": {"type": "none"}})).await;
    assert!(!failed, "click {selector} in {context} of {id}: {answer}");
}

#[tokio::test]
async fn window_focus_survives_recorder_replacement() {
    exercise_window_focus(true).await;
}

#[tokio::test]
async fn window_focus_is_reconstructed_in_a_replay_branch() {
    exercise_window_focus(false).await;
}

async fn exercise_window_focus(replace_recorder: bool) {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = format!(r#"<!doctype html><button id=open onclick="window.open('https://auth.example.test/', 'auth')">Sign in</button><button id=page>Page</button>{REPORT}"#);
    let popup = format!(r#"<!doctype html><button id=popup>Popup</button><button id=close onclick="window.close()">Close</button>{REPORT}"#);
    let rule = |name: &str, url: &str, body: String| json!({"id": name, "match": {"url": url},
        "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "then": {"type": "none"},
        "profile": {"initial_date_ms": 1700000000000u64}, "passthrough_origins": [],
        "rules": [rule("page", "https://app.example.test/", page), rule("popup", "https://auth.example.test/", popup)]})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 50}})).await;
    click(&mut m, &id, "page", "#open").await;
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 50}})).await;
    assert_eq!(events(&mut m, &id, "page").await, json!([["blur", 50, false]]));
    assert_eq!(events(&mut m, &id, "popup-1").await, json!([]));

    if replace_recorder {
        let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
        frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
        m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
    }
    // Adoption and reads must not activate the opener or duplicate popup focus.
    assert_eq!(events(&mut m, &id, "page").await, json!([["blur", 50, false]]));
    assert_eq!(events(&mut m, &id, "popup-1").await, json!([]));
    click(&mut m, &id, "popup-1", "#popup").await;
    assert_eq!(events(&mut m, &id, "popup-1").await, json!([]));
    click(&mut m, &id, "page", "#page").await;
    let page_events = json!([["blur", 50, false], ["focus", 100, true]]);
    let popup_events = json!([["blur", 100, false]]);
    assert_eq!(events(&mut m, &id, "page").await, page_events);
    assert_eq!(events(&mut m, &id, "popup-1").await, popup_events);

    let mut executions = vec![id.clone()];
    if !replace_recorder {
        // A recording with a recorder fault is explicitly not replayable. Check
        // reconstruction using an intact recording of the same window actions.
        let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": id})).await;
        let branch = m.call_tool("state", json!({"operation": "branch", "kind": "replay", "saved_id": saved["saved_id"]})).await;
        let child = branch["launch_id"].clone();
        assert_eq!(events(&mut m, &child, "page").await, page_events);
        assert_eq!(events(&mut m, &child, "popup-1").await, popup_events);
        executions.push(child);
    }
    // Continue both executions from the same focused window and virtual instant.
    for current in &executions {
        click(&mut m, current, "popup-1", "#popup").await;
        assert_eq!(events(&mut m, current, "popup-1").await,
            json!([["blur", 100, false], ["focus", 100, true]]));
        click(&mut m, current, "popup-1", "#close").await;
        assert_eq!(events(&mut m, current, "page").await,
            json!([["blur", 50, false], ["focus", 100, true], ["blur", 100, false], ["focus", 100, true]]));
        assert_eq!(m.call_tool("status", json!({"launch_id": current})).await["t_us"], 100_000);
        m.call_tool("stop", json!({"launch_id": current})).await;
        if !replace_recorder {
            let replay = m.call_tool("debug", json!({"launch_id": current, "operation": "replay"})).await;
            assert_eq!(replay["equal"], true, "{replay}");
        }
    }
}
