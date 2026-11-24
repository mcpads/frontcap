use super::*;
use serde_json::json;

struct Echo;

impl Tools for Echo {
    fn list(&self) -> Vec<ToolDescription> {
        vec![ToolDescription {
            name: "echo".into(),
            description: "Returns its arguments.".into(),
            input_schema: schemars::json_schema!({ "type": "object", "properties": {} }),
        }]
    }

    fn call(&self, name: &str, arguments: Option<Box<RawValue>>) -> ToolFuture<'_> {
        let name = name.to_string();
        Box::pin(async move {
            let args = arguments.map(|a| a.get().to_string()).unwrap_or_default();
            CallToolResult::structured(&json!({ "tool": name, "args": args }))
        })
    }
}

async fn ask(s: &Server<Echo>, v: serde_json::Value) -> Option<serde_json::Value> {
    s.handle(&v.to_string()).await.map(|line| serde_json::from_str(&line).unwrap())
}

async fn initialized() -> Server<Echo> {
    let s = Server::new(Echo);
    ask(&s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})).await;
    s
}

#[tokio::test]
async fn initialize_negotiates_the_version() {
    let s = Server::new(Echo);
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})).await.unwrap();
    assert_eq!(r["id"], 1);
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r["result"]["serverInfo"]["name"], "frontcap");
    assert!(r["result"]["capabilities"]["tools"].is_object(), "{r}");
    let s = Server::new(Echo);
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": "a", "method": "initialize", "params": {"protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})).await.unwrap();
    assert_eq!(r["id"], "a");
    assert_eq!(r["result"]["protocolVersion"], SUPPORTED_VERSIONS[0]);
}

#[tokio::test]
async fn calls_before_initialize_are_rejected() {
    let s = Server::new(Echo);
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
    assert_eq!(r["error"]["code"], -32002);
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 3, "method": "ping"})).await.unwrap();
    assert_eq!(r["result"], json!({}));
}

#[tokio::test]
async fn unknown_methods_and_bad_json_are_errors_not_crashes() {
    let s = initialized().await;
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 4, "method": "foo"})).await.unwrap();
    assert_eq!(r["error"]["code"], -32601);
    let r: serde_json::Value = serde_json::from_str(&s.handle("{").await.unwrap()).unwrap();
    assert_eq!(r["error"]["code"], -32700);
    assert!(r["id"].is_null());
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"nope": 1}})).await.unwrap();
    assert_eq!(r["error"]["code"], -32602);
}

#[tokio::test]
async fn tools_are_listed_and_called() {
    let s = initialized().await;
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"})).await.unwrap();
    assert_eq!(r["result"]["tools"][0]["name"], "echo");
    assert_eq!(r["result"]["tools"][0]["inputSchema"]["type"], "object");
    let r = ask(&s, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "echo", "arguments": {"x": 1}}})).await.unwrap();
    let result = &r["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["structuredContent"]["tool"], "echo");
    assert_eq!(result["structuredContent"]["args"], "{\"x\":1}");
}

#[tokio::test]
async fn notifications_get_no_response() {
    let s = initialized().await;
    assert!(ask(&s, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
    assert!(ask(&s, json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}})).await.is_none());
}
