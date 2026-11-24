//! Streams a `hold` rule keeps for the agent: it answers the handshake, sends each message when it
//! decides, and closes them as their server would.
mod support;
use serde_json::{Value, json};

const CHAT: &str = r#"<!doctype html><meta charset="utf-8"><p id=state>Connecting…</p><ul id=items></ul><p id=ws>none</p><p id=echo></p>
<script>
const state = document.getElementById('state'), items = document.getElementById('items');
const feed = new EventSource('https://api.example.test/chat');
feed.onopen = () => { state.textContent = 'Live'; };
feed.onerror = () => { state.textContent = feed.readyState === EventSource.CONNECTING ? 'Reconnecting…' : 'Offline'; };
feed.onmessage = (e) => { const li = document.createElement('li'); li.textContent = e.data + (e.lastEventId ? ' #' + e.lastEventId : ''); items.append(li); };
feed.addEventListener('typing', (e) => { state.textContent = e.data + ' is typing'; });
const ws = new WebSocket('wss://api.example.test/live');
ws.onopen = () => { document.getElementById('ws').textContent = 'open'; ws.send('hello'); };
ws.onmessage = (e) => { document.getElementById('echo').textContent = e.data; };
ws.onclose = (e) => { document.getElementById('ws').textContent = 'closed ' + e.code; };
</script>"#;

fn rules(chat: Value) -> Value {
    json!([
        {"id": "app", "match": {"url": "https://app.example.test/"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": CHAT}},
        {"id": "chat", "match": {"url": "https://api.example.test/chat"}, "action": chat},
        {"id": "live", "match": {"url": "wss://api.example.test/live"}, "action": {"type": "hold"}},
    ])
}

async fn text(m: &mut support::Mcp, id: &Value, selector: &str) -> String {
    let read = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": selector}, "props": ["text"]}})).await;
    read["values"]["text"].as_str().unwrap().to_string()
}

/// The held streams by URL: `(ordinal, stream state)`.
async fn held(m: &mut support::Mcp, id: &Value) -> Vec<(String, String, String)> {
    let listed = m.call_tool("network", json!({"launch_id": id, "operation": "list_held"})).await;
    listed["held"].as_array().unwrap().iter()
        .filter(|h| h.get("stream").is_some())
        .map(|h| (h["url"].as_str().unwrap().to_string(), h["ordinal"].as_str().unwrap().to_string(), h["stream"].as_str().unwrap().to_string()))
        .collect()
}

async fn network(m: &mut support::Mcp, id: &Value, request: &str, args: Value) -> Value {
    let mut call = json!({"launch_id": id, "request_id": request});
    call.as_object_mut().unwrap().extend(args.as_object().unwrap().clone());
    m.call_tool("network", call).await
}

/// A chat's EventSource and WebSocket held for the agent: each waits connecting until answered, takes
/// messages when the agent sends them (named events and ids included), and closes as its server would. An
/// EventSource closed by its server reconnects and is held again; failing that handshake leaves it
/// offline. The run replays.
#[tokio::test]
async fn held_streams_take_the_agents_messages() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules(json!({"type": "hold"})), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let streams = held(&mut m, &id).await;
    let ordinal = |url: &str| streams.iter().find(|(u, _, _)| u == url).map(|(_, o, s)| (o.clone(), s.clone())).unwrap();
    let (chat, state) = ordinal("https://api.example.test/chat");
    assert_eq!(state, "connecting", "{streams:?}");
    let (live, _) = ordinal("wss://api.example.test/live");
    assert_eq!((text(&mut m, &id, "#state").await, text(&mut m, &id, "#ws").await), ("Connecting…".into(), "none".into()));

    network(&mut m, &id, "open-chat", json!({"operation": "respond", "ordinal": chat, "response": {"status": 200}})).await;
    assert_eq!(text(&mut m, &id, "#state").await, "Live");
    assert_eq!(held(&mut m, &id).await.iter().find(|(_, o, _)| *o == chat).unwrap().2, "open");
    network(&mut m, &id, "hi", json!({"operation": "send", "ordinal": chat, "data": "Ada: hi"})).await;
    assert_eq!(text(&mut m, &id, "#items").await, "Ada: hi", "the page's handler ran at once");
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 1000}})).await;
    network(&mut m, &id, "typing", json!({"operation": "send", "ordinal": chat, "data": "Grace", "event": "typing"})).await;
    assert_eq!(text(&mut m, &id, "#state").await, "Grace is typing");
    network(&mut m, &id, "hello", json!({"operation": "send", "ordinal": chat, "data": "Grace: hello", "id": "7"})).await;
    assert_eq!(text(&mut m, &id, "#items").await, "Ada: hiGrace: hello #7");

    let (refused, error) = m.try_tool("network", json!({"launch_id": id, "request_id": "early", "operation": "send", "ordinal": live, "data": "too soon"})).await;
    assert!(refused && error["message"].as_str().unwrap().contains("respond"), "{error}");
    network(&mut m, &id, "open-live", json!({"operation": "respond", "ordinal": live, "response": {"status": 101}})).await;
    assert_eq!(text(&mut m, &id, "#ws").await, "open");
    network(&mut m, &id, "pong", json!({"operation": "send", "ordinal": live, "data": "pong"})).await;
    assert_eq!(text(&mut m, &id, "#echo").await, "pong");
    network(&mut m, &id, "close-live", json!({"operation": "close", "ordinal": live, "code": 4000})).await;
    assert_eq!(text(&mut m, &id, "#ws").await, "closed 4000");
    let (refused, _) = m.try_tool("network", json!({"launch_id": id, "request_id": "late", "operation": "send", "ordinal": live, "data": "after"})).await;
    assert!(refused, "a closed stream takes nothing more");

    // The server closes the feed: the EventSource reconnects after 3 s, a new request the rule holds.
    network(&mut m, &id, "close-chat", json!({"operation": "close", "ordinal": chat})).await;
    assert_eq!(text(&mut m, &id, "#state").await, "Reconnecting…");
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 3100}})).await;
    let again = held(&mut m, &id).await;
    let (_, reconnect, state) = again.iter().find(|(u, o, _)| u == "https://api.example.test/chat" && *o != chat).cloned().expect("a held reconnect");
    assert_eq!(state, "connecting");
    network(&mut m, &id, "fail-chat", json!({"operation": "fail", "ordinal": reconnect, "reason": "connection_refused"})).await;
    assert_eq!(text(&mut m, &id, "#state").await, "Offline");
    assert!(held(&mut m, &id).await.is_empty());
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A rule that cannot answer a stream says so in the effects, with what can.
#[tokio::test]
async fn a_rule_that_cannot_answer_a_stream_says_what_can() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules(json!({"type": "mock", "status": 200, "body": "data: x\n\n"})), "passthrough_origins": []})).await;
    let requests = launched["effects"]["requests"].as_array().unwrap();
    let chat = requests.iter().filter_map(Value::as_str).find(|r| r.contains("/chat")).unwrap();
    assert!(chat.contains("fail(rule=chat)") && chat.contains("a stream_mock or hold rule can"), "{chat}");
    m.call_tool("stop", json!({"launch_id": launched["launch_id"]})).await;
}

/// `wait next {event: stream}` stops at each message a scripted stream delivers, its handler run, and
/// the run replays.
#[tokio::test]
async fn a_next_stream_wait_stops_at_each_message() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let feed = json!({"type": "stream_mock", "messages": [{"at_ms": 1000, "data": "one"}, {"at_ms": 2000, "data": "two"}]});
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules(feed), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let opened = m.call_tool("status", json!({"launch_id": id})).await["t_us"].as_u64().unwrap();
    let mut stops = Vec::new();
    for _ in 0..2 {
        let next = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "next", "event": "stream", "max_ms": 5000}})).await;
        assert_eq!(next["outcome"]["type"], "event", "{next}");
        assert_eq!(next["outcome"]["triggers"][0]["event"], "message", "{next}");
        stops.push((next["t_us"].as_u64().unwrap(), text(&mut m, &id, "#items").await));
    }
    assert_eq!(stops[1].0 - stops[0].0, 1_000_000, "{stops:?} after {opened}");
    assert_eq!((stops[0].1.as_str(), stops[1].1.as_str()), ("one", "onetwo"), "each stop after its message's handler");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}
