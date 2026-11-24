//! Windows the page opens, through the public MCP surface: a popup is a context of the generation,
//! observed and operated by naming it or by its references, recorded in `act` effects, and replayed.
mod support;
use serde_json::{Value, json};

const OPENER: &str = r#"<!doctype html><button id=signin onclick="window.open('https://auth.example.test/login', 'auth')">Sign in</button><p id=status>signed out</p>
<script>addEventListener('message', e => { document.getElementById('status').textContent = String(e.data); });</script>"#;
const LOGIN: &str = r#"<!doctype html><label>Name <input id=name></label>
<button onclick="opener.postMessage('signed in as ' + document.getElementById('name').value, '*'); window.close()">Continue</button>"#;

/// A sign-in window that checks the name with its service, welcomes the user and then says it returns.
const CHECKING: &str = r#"<!doctype html><label>Name <input id=name></label><button id=go>Continue</button><p id=msg></p>
<script>
document.getElementById('go').onclick = async () => {
  const msg = document.getElementById('msg');
  msg.textContent = 'Checking…';
  const r = await fetch('https://auth.example.test/check', { method: 'POST', body: document.getElementById('name').value });
  msg.textContent = 'Welcome, ' + await r.text();
  setTimeout(() => { const done = document.createElement('p'); done.id = 'done'; done.textContent = 'Returning you to the app'; document.body.append(done); }, 1000);
};
</script>"#;

fn checking_rules() -> Value {
    let mut rules = rules();
    rules[1]["action"]["body"] = json!(CHECKING);
    rules.as_array_mut().unwrap().push(json!({"id": "check", "match": {"method": "POST", "url": "https://auth.example.test/check"},
        "action": {"type": "mock", "status": 200, "body": "ada", "latency_ms": 500}}));
    rules
}

fn rules() -> Value {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    json!([page("app", "https://app.example.test/", OPENER), page("login", "https://auth.example.test/login", LOGIN)])
}

fn node<'a>(nodes: &'a Value, role: &str, name: &str) -> &'a Value {
    nodes.as_array().unwrap().iter().find(|n| n["role"] == role && n["name"] == name).unwrap_or_else(|| panic!("no {role} {name}: {nodes}"))
}

#[tokio::test]
async fn a_sign_in_popup_is_operated_by_context_and_replayed() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules(), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let acted = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
    assert_eq!(acted["effects"]["contexts"], json!(["popup-1 opened by page"]), "{acted}");
    assert!(acted["effects"]["requests"].to_string().contains("popup-1: GET https://auth.example.test/login → mock(rule=login) 200"), "{acted}");
    // The popup's document is built within the click: its DOM changes count with the page's.
    assert!(acted["effects"]["dom_mutations"].as_u64().is_some_and(|n| n > 0), "{acted}");
    let contexts = m.call_tool("session", json!({"launch_id": id, "operation": "contexts"})).await;
    assert_eq!(contexts["contexts"][1], json!({"id": "popup-1", "opener": "page", "url": "https://auth.example.test/login", "open": true}), "{contexts}");
    let popup = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    let name = node(&popup["nodes"], "textbox", "Name")["ref"].clone();
    let go = node(&popup["nodes"], "button", "Continue")["ref"].clone();
    // References name their context; the page's own observation does not see the popup's nodes.
    let page = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    assert!(!page["nodes"].to_string().contains("Continue"), "{page}");
    let shot = m.call_tool("capture", json!({"launch_id": id, "context": "popup-1", "op": {"type": "screenshot"}})).await;
    let png = std::fs::read(shot["path"].as_str().unwrap()).unwrap();
    assert!(png.starts_with(b"\x89PNG"), "{shot}");
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "action": {"type": "fill", "target": {"by": "ref", "id": name}, "value": "ada"}})).await;
    let closed = m.call_tool("act", json!({"launch_id": id, "request_id": "c-3", "action": {"type": "click", "target": {"by": "ref", "id": go}}})).await;
    assert_eq!(closed["effects"]["contexts"], json!(["popup-1 closed"]), "{closed}");
    // What the popup told its opener (app-state consumer run: messages between windows were not visible).
    assert_eq!(closed["effects"]["messages"], json!(["from https://auth.example.test \"signed in as ada\""]), "{closed}");
    let status = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#status"}, "props": ["text"]}})).await;
    assert!(status.to_string().contains("signed in as ada"), "{status}");
    let (is_error, gone) = m.try_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    assert!(is_error && gone["code"] == "context_closed", "{gone}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// The user closes the sign-in window instead of continuing: the opener notices on its next look, the
/// close is recorded in effects, and the command replays.
#[tokio::test]
async fn closing_a_sign_in_popup_cancels_the_sign_in() {
    let opener = r#"<!doctype html><button id=signin onclick="watch(window.open('https://auth.example.test/login', 'auth'))">Sign in</button><p id=status>signed out</p>
<script>function watch(w) { const t = setInterval(() => { if (w.closed) { clearInterval(t); document.getElementById('status').textContent = 'Sign-in cancelled'; } }, 500); }</script>"#;
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = json!([page("app", "https://app.example.test/", opener), page("login", "https://auth.example.test/login", LOGIN)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
    let (is_error, refused) = m.try_tool("act", json!({"launch_id": id, "request_id": "x-0", "action": {"type": "close"}})).await;
    assert!(is_error && refused["code"] == "invalid_arguments", "{refused}");
    let closed = m.call_tool("act", json!({"launch_id": id, "request_id": "x-1", "context": "popup-1", "action": {"type": "close"}, "then": {"type": "none"}})).await;
    assert_eq!(closed["effects"]["contexts"], json!(["popup-1 closed"]), "{closed}");
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 600}})).await;
    let status = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#status"}, "props": ["text"]}})).await;
    assert!(status.to_string().contains("Sign-in cancelled"), "{status}");
    let contexts = m.call_tool("session", json!({"launch_id": id, "operation": "contexts"})).await;
    assert_eq!(contexts["contexts"][1]["open"], false, "{contexts}");
    // When it closed and what it last showed (app-state consumer rerun: a closed popup had neither).
    assert_eq!(contexts["contexts"][1]["closed_us"], closed["t_after_us"], "{contexts}");
    assert_eq!(contexts["contexts"][1]["url"], "https://auth.example.test/login", "{contexts}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Scenario 5's Help link: a plain `target=_blank` link opens a window without an opener, which the
/// consumer could only report as refused. It opens at the click, is observed by context and replays.
#[tokio::test]
async fn a_help_link_opens_a_window_without_an_opener() {
    let opener = r#"<!doctype html><p>Workspace</p><a href="https://help.example.test/connect" target=_blank>Help</a>"#;
    let help = r#"<!doctype html><h1>Connecting accounts</h1><p id=opener></p><script>document.getElementById('opener').textContent = window.opener === null ? 'no opener' : 'opener';</script>"#;
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = json!([page("app", "https://app.example.test/", opener), page("help", "https://help.example.test/connect", help)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let acted = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "link", "name": "Help"}}})).await;
    assert_eq!(acted["effects"]["contexts"], json!(["popup-1 opened by page without an opener"]), "{acted}");
    assert!(acted["effects"]["requests"].to_string().contains("popup-1: GET https://help.example.test/connect → mock(rule=help) 200"), "{acted}");
    let contexts = m.call_tool("session", json!({"launch_id": id, "operation": "contexts"})).await;
    assert_eq!(contexts["contexts"][1], json!({"id": "popup-1", "opener": "page", "url": "https://help.example.test/connect", "open": true, "no_opener": true}), "{contexts}");
    let popup = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    assert!(popup["nodes"].to_string().contains("Connecting accounts") && popup["nodes"].to_string().contains("no opener"), "{popup}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Scenario 5's second run: a replay basis saved from the live generation after a click opened a sign-in
/// window, whose document the network answers at once in the recording and slowly in its re-execution.
/// The window's first commit is read before the click completes either way, so the branch's prefix is the
/// recording's.
#[tokio::test]
async fn a_live_generation_with_a_sign_in_window_branches_by_replay() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", server.server_addr().to_ip().unwrap());
    let sign_ins = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let sign_ins = sign_ins.clone();
            std::thread::spawn(move || {
                let signin = req.url().starts_with("/signin");
                let body = if signin { "<!doctype html><h1>Sign in</h1>" } else { r#"<!doctype html><button onclick="window.open('/signin', 'auth'); this.textContent = 'Waiting for sign-in…'">Connect account</button>"# };
                if signin && sign_ins.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 { std::thread::sleep(std::time::Duration::from_millis(300)); }
                let _ = req.respond(tiny_http::Response::from_string(body).with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
            });
        }
    });
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": format!("{origin}/")})).await;
    let id = launched["launch_id"].clone();
    let acted = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Connect account"}},
        "then": {"type": "settle", "idle_window_ms": 100, "max_ms": 1000}})).await;
    assert_eq!(acted["effects"]["contexts"], json!(["popup-1 opened by page"]), "{acted}");
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": id})).await;
    let branch = m.call_tool("state", json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "replay"})).await;
    let child = branch["launch_id"].clone();
    let popup = m.call_tool("observe", json!({"launch_id": child, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    assert!(popup["nodes"].to_string().contains("Sign in"), "{popup}");
    m.call_tool("stop", json!({"launch_id": child})).await;
    m.call_tool("stop", json!({"launch_id": id})).await;
}

const CONSENT: &str = r#"<!doctype html><p>Example wants to read your profile.</p>
<button onclick="opener.postMessage(confirm('Allow Example to read your profile?') ? 'allowed' : 'denied', '*')">Allow</button>
<script>let consent = { scope: 'profile' };
frontcapState.register({ id: 'consent', read: () => consent, write: value => { consent = value; } });</script>"#;

/// A dialog a `hold` rule matches in a popup suspends the operation, as one in the launch page does, until
/// the agent answers it; the answer is recorded with the popup's context and replayed.
#[tokio::test]
async fn a_dialog_held_in_a_popup_waits_for_the_agents_answer() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "passthrough_origins": [],
        "rules": [page("app", "https://app.example.test/", OPENER), page("login", "https://auth.example.test/login", CONSENT)]})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("session", json!({"launch_id": id, "operation": "dialog_rules", "rules": [{"answer": "hold"}]})).await;
    let acted = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
    assert_eq!(acted["effects"]["contexts"], json!(["popup-1 opened by page"]), "{acted}");
    let allow = json!({"launch_id": id, "request_id": "c-2", "context": "popup-1",
        "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Allow"}}, "then": {"type": "none"}});
    let suspended = m.call_tool("act", allow.clone()).await;
    assert_eq!(suspended["state"], "suspended", "{suspended}");
    let dialog = suspended["dialog"].clone();
    assert_eq!((dialog["kind"].as_str(), dialog["message"].as_str(), dialog["context"].as_str()),
        (Some("confirm"), Some("Allow Example to read your profile?"), Some("popup-1")), "{suspended}");
    let receipt = m.call_tool("session", json!({"launch_id": id, "operation": "answer_dialog", "operation_id": "c-2", "dialog_id": dialog["id"], "request_id": "a-1", "accept": true})).await;
    assert_eq!(receipt["parent"]["state"], "completed", "{receipt}");
    // The answered act's effects come with it, as the act would report them.
    assert_eq!(receipt["parent"]["effects"]["dialogs"], json!(["popup-1: confirm \"Allow Example to read your profile?\" → true"]), "{receipt}");
    // Once it is idle, the act, repeated, returns what it did, its dialog and answer included.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !m.call_tool("status", json!({"launch_id": id})).await["running"].is_null() { tokio::task::yield_now().await; }
    }).await.expect("the act finished");
    let done = m.call_tool("act", allow).await;
    // A popup's own state providers are read in its context.
    let consent = m.call_tool("state", json!({"launch_id": id, "operation": "read", "context": "popup-1", "target": {"by": "document"}, "field": {"type": "app", "provider": "consent"}})).await;
    assert_eq!(consent["value"], json!({"scope": "profile"}), "{consent}");
    // And written there: a boundary observed in the popup names its context.
    let scene = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "scene", "image": false}})).await;
    let applied = m.call_tool("state", json!({"launch_id": id, "operation": "apply", "context": "popup-1", "boundary": scene["boundary"], "changes": [
        {"target": {"by": "document"}, "field": {"type": "app", "provider": "consent"}, "value": {"scope": "email"}, "expected": {"value": {"scope": "profile"}}}]})).await;
    assert_eq!(applied["completed"], true, "{applied}");
    let consent = m.call_tool("state", json!({"launch_id": id, "operation": "read", "context": "popup-1", "target": {"by": "document"}, "field": {"type": "app", "provider": "consent"}})).await;
    assert_eq!(consent["value"], json!({"scope": "email"}), "{consent}");
    assert_eq!(done["effects"]["dialogs"], json!(["popup-1: confirm \"Allow Example to read your profile?\" → true"]), "{done}");
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 10}})).await;
    let status = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#status"}, "props": ["text"]}})).await;
    assert_eq!(status["values"]["text"], "allowed");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let trace = std::fs::read_to_string(std::path::Path::new(launched["bundle"].as_str().unwrap()).join("trace.jsonl")).unwrap();
    assert!(!trace.contains("dialog_hold_unsupported"), "the popup's dialog was held, not answered by default");
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A dialog held in a popup is dismissed by cleanup when the lease is released or the recorder replaced,
/// as one in the launch page is, and the generation stays usable.
#[tokio::test]
async fn a_dialog_held_in_a_popup_is_dismissed_on_lease_release_and_recorder_replacement() {
    for cleanup in ["release", "crash"] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
        let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "passthrough_origins": [],
            "rules": [page("app", "https://app.example.test/", OPENER), page("login", "https://auth.example.test/login", CONSENT)]})).await;
        let id = launched["launch_id"].clone();
        m.call_tool("session", json!({"launch_id": id, "operation": "dialog_rules", "rules": [{"answer": "hold"}]})).await;
        m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
        let suspended = m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "context": "popup-1",
            "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Allow"}}, "then": {"type": "none"}})).await;
        assert_eq!(suspended["state"], "suspended", "{cleanup}: {suspended}");
        if cleanup == "release" {
            m.call_tool("session", json!({"launch_id": id, "operation": "release"})).await;
        } else {
            let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
            frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
        }
        m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
        let status = m.call_tool("status", json!({"launch_id": id})).await;
        assert!(status["suspension"].is_null() && status["running"].is_null(), "{cleanup}: {status}");
        // The cleanup dismissed it: the popup's page got the dismissed answer.
        m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 10}})).await;
        let text = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#status"}, "props": ["text"]}})).await;
        assert_eq!(text["values"]["text"], "denied", "{cleanup}");
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}

/// A sign-in window's Allow button is enabled once its consent text has been shown for 0.8 s; the agent
/// waits for it in the window and clicks it, and the run replays.
#[tokio::test]
async fn waiting_for_a_popups_button_judges_it_in_the_popup() {
    const CONSENT: &str = r#"<!doctype html><p>Share your name?</p><button id=allow disabled
onclick="opener.postMessage('allowed', '*'); window.close()">Allow</button>
<script>setTimeout(() => { document.getElementById('allow').disabled = false; }, 800);</script>"#;
    let mut rules = rules();
    rules[1]["action"]["body"] = json!(CONSENT);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let opened = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}, "then": {"type": "none"}})).await;
    assert_eq!(opened["effects"]["contexts"], json!(["popup-1 opened by page"]), "{opened}");
    let allow = json!({"by": "role", "role": "button", "name": "Allow"});
    let waited = m.call_tool("act", json!({"launch_id": id, "request_id": "w-1", "context": "popup-1", "action": {"type": "wait_actionable", "target": allow, "max_ms": 2000}})).await;
    assert_eq!(waited["waited"]["type"], "actionable", "{waited}");
    // Judged at the first frame tick after the window, opened by the click, enabled the button 0.8 s on.
    let after = waited["t_after_us"].as_u64().unwrap() - waited["t_before_us"].as_u64().unwrap();
    assert!((800_000..820_000).contains(&after), "{waited}");
    let closed = m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "context": "popup-1", "action": {"type": "click", "target": allow}})).await;
    assert_eq!(closed["effects"]["messages"], json!(["from https://auth.example.test \"allowed\""]), "{closed}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A sign-in window checks the name with its service before it welcomes the user and closes itself: a
/// watch and predicate waits judged in the window time each step there, by context or by a reference the
/// window's snapshot gave, and the run replays.
#[tokio::test]
async fn watches_and_predicate_waits_judge_in_a_popup() {

    let rules = checking_rules();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
    let msg = json!({"by": "css", "selector": "#msg"});
    let added = m.call_tool("watch", json!({"launch_id": id, "request_id": "w-1", "operation": "add", "name": "welcomed", "context": "popup-1",
        "predicate": {"is": "text_equals", "target": msg, "text": "Welcome, ada"}})).await;
    assert_eq!(added["value"], "false", "{added}");
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "context": "popup-1", "action": {"type": "fill", "target": {"by": "css", "selector": "#name"}, "value": "ada"}})).await;
    let clicked = m.call_tool("act", json!({"launch_id": id, "request_id": "c-3", "context": "popup-1", "action": {"type": "click", "target": {"by": "css", "selector": "#go"}}, "then": {"type": "none"}})).await;
    let at = clicked["t_after_us"].as_u64().unwrap();
    let welcomed = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-1", "op": {"type": "until", "watch": "welcomed", "max_ms": 2000}})).await;
    assert_eq!(welcomed["outcome"]["type"], "condition_met", "{welcomed}");
    let waited = welcomed["t_us"].as_u64().unwrap() - at;
    assert!((500_000..520_000).contains(&waited), "welcomed when the check answered, 0.5 s on: {welcomed}");
    let history = m.call_tool("watch", json!({"launch_id": id, "operation": "history", "name": "welcomed"})).await;
    let values: Vec<&str> = history["samples"].as_array().unwrap().iter().map(|s| s["value"].as_str().unwrap()).collect();
    assert_eq!(values, ["false", "true"], "{history}");
    // A reference from the window's snapshot names the window: no context is needed.
    let snapshot = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    let body = snapshot["nodes"].as_array().unwrap().iter().find(|n| n["name"] == "Welcome, ada").expect("the welcome text")["ref"].clone();
    let same = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-2", "op": {"type": "until", "max_ms": 10, "predicate": {"is": "attached", "target": {"by": "ref", "id": body}}}})).await;
    assert_eq!(same["outcome"]["type"], "condition_met", "{same}");
    let done = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-3", "op": {"type": "until", "max_ms": 2000, "context": "popup-1",
        "predicate": {"is": "attached", "target": {"by": "css", "selector": "#done"}}}})).await;
    assert_eq!(done["outcome"]["type"], "condition_met", "{done}");
    assert!((1_000_000..1_020_000).contains(&(done["t_us"].as_u64().unwrap() - welcomed["t_us"].as_u64().unwrap())), "{done}");
    // The launch page has no #done: judged there, the same predicate times out.
    let page = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-4", "op": {"type": "until", "max_ms": 50,
        "predicate": {"is": "attached", "target": {"by": "css", "selector": "#done"}}}})).await;
    assert_eq!(page["outcome"]["type"], "condition_timed_out", "{page}");
    m.call_tool("watch", json!({"launch_id": id, "request_id": "w-2", "operation": "remove", "name": "welcomed"})).await;
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A watch in a popup outlives the recorder: a relaunched recorder takes the window back with its watch,
/// which then times the welcome as before.
#[tokio::test]
async fn a_popup_watch_survives_recorder_replacement() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": checking_rules(), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Sign in"}}})).await;
    m.call_tool("watch", json!({"launch_id": id, "request_id": "w-1", "operation": "add", "name": "welcomed", "context": "popup-1",
        "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#msg"}, "text": "Welcome, ada"}})).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
    let status = m.call_tool("status", json!({"launch_id": id})).await;
    assert_eq!(status["watches"], json!(["welcomed"]), "{status}");
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "context": "popup-1", "action": {"type": "fill", "target": {"by": "css", "selector": "#name"}, "value": "ada"}})).await;
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-3", "context": "popup-1", "action": {"type": "click", "target": {"by": "css", "selector": "#go"}}, "then": {"type": "none"}})).await;
    let welcomed = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-1", "op": {"type": "until", "watch": "welcomed", "max_ms": 2000}})).await;
    assert_eq!(welcomed["outcome"]["type"], "condition_met", "{welcomed}");
    let history = m.call_tool("watch", json!({"launch_id": id, "operation": "history", "name": "welcomed"})).await;
    let values: Vec<&str> = history["samples"].as_array().unwrap().iter().map(|s| s["value"].as_str().unwrap()).collect();
    assert_eq!(values.first(), Some(&"false"), "{history}");
    assert_eq!(values.last(), Some(&"true"), "{history}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}

/// A help window is taken to its FAQ, back, and reloaded by its user: each lands in the window, named by
/// its context in effects and the context list, and the run replays.
#[tokio::test]
async fn a_popup_is_navigated_back_and_reloaded() {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    const APP: &str = r#"<!doctype html><button onclick="window.open('https://help.example.test/start', 'help')">Help</button>"#;
    const START: &str = r#"<!doctype html><h1>Help</h1><p id=loads></p><script>sessionStorage.n = (+sessionStorage.n || 0) + 1; loads.textContent = 'loaded ' + sessionStorage.n;</script>"#;
    let rules = json!([page("app", "https://app.example.test/", APP), page("start", "https://help.example.test/start", START), page("faq", "https://help.example.test/faq", "<!doctype html><h1>FAQ</h1>")]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Help"}}})).await;
    let url = |contexts: &Value| contexts["contexts"][1]["url"].as_str().unwrap().to_string();
    let heading = |snapshot: &Value| snapshot["nodes"].as_array().unwrap().iter().find(|n| n["role"] == "heading").map(|n| n["name"].as_str().unwrap().to_string());

    let faq = m.call_tool("act", json!({"launch_id": id, "request_id": "n-1", "context": "popup-1", "action": {"type": "navigate", "url": "https://help.example.test/faq"}})).await;
    assert!(faq["effects"]["requests"].to_string().contains("popup-1: GET https://help.example.test/faq"), "{faq}");
    let contexts = m.call_tool("session", json!({"launch_id": id, "operation": "contexts"})).await;
    assert_eq!(url(&contexts), "https://help.example.test/faq", "{contexts}");
    let shown = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "snapshot"}})).await;
    assert_eq!(heading(&shown).as_deref(), Some("FAQ"), "{shown}");

    m.call_tool("act", json!({"launch_id": id, "request_id": "n-2", "context": "popup-1", "action": {"type": "back"}})).await;
    let contexts = m.call_tool("session", json!({"launch_id": id, "operation": "contexts"})).await;
    assert_eq!(url(&contexts), "https://help.example.test/start", "{contexts}");
    m.call_tool("act", json!({"launch_id": id, "request_id": "n-3", "context": "popup-1", "action": {"type": "reload"}})).await;
    let loads = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "query", "target": {"by": "css", "selector": "#loads"}, "props": ["text"]}})).await;
    assert_eq!(loads["values"]["text"], "loaded 3", "the window's first load, its return and its reload: {loads}");
    // The launch page never moved.
    let app = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    assert!(app["nodes"].to_string().contains("Help") && heading(&app).is_none(), "{app}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

const FEED: &str = r#"<!doctype html><h1>Notifications</h1><ul id=items></ul><p id=state>connecting</p>
<script>
const source = new EventSource('https://api.example.test/notifications');
source.onopen = () => { document.getElementById('state').textContent = 'live'; };
source.onmessage = (e) => { const li = document.createElement('li'); li.textContent = e.data; document.getElementById('items').append(li); };
</script>"#;

fn feed_rules() -> Value {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    const APP: &str = r#"<!doctype html><button onclick="window.open('https://app.example.test/feed', 'feed')">Notifications</button>"#;
    json!([page("app", "https://app.example.test/", APP), page("feed", "https://app.example.test/feed", FEED),
        {"id": "notifications", "match": {"url": "https://api.example.test/notifications"}, "action": {"type": "stream_mock",
            "messages": [{"at_ms": 500, "data": "Ada replied"}, {"at_ms": 1500, "data": "Grace joined"}]}}])
}

/// The window's notifications, in order.
async fn items(m: &mut support::Mcp, id: &Value) -> Vec<String> {
    let count = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "count", "target": {"by": "css", "selector": "#items li"}}})).await;
    let mut items = Vec::new();
    for k in 1..=count["count"].as_u64().unwrap() {
        let item = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "query", "target": {"by": "css", "selector": format!("#items li:nth-child({k})")}, "props": ["text"]}})).await;
        items.push(item["values"]["text"].as_str().unwrap().to_string());
    }
    items
}

/// A notifications window reads Server-Sent Events: they reach it at their instants, as the launch page's
/// would, and the run replays.
#[tokio::test]
async fn a_popups_stream_is_delivered_in_the_popup() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": feed_rules(), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    let opened = m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Notifications"}}, "then": {"type": "none"}})).await;
    let at = opened["t_after_us"].as_u64().unwrap();
    let advanced = m.call_tool("wait", json!({"launch_id": id, "request_id": "a-1", "op": {"type": "advance", "by_ms": 100}})).await;
    let requests = advanced["effects"]["requests"].as_array().unwrap();
    assert!(requests.iter().any(|r| r.as_str().unwrap().starts_with("popup-1: GET https://api.example.test/notifications")), "the handshake names its window: {advanced}");
    let state = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "query", "target": {"by": "css", "selector": "#state"}, "props": ["text"]}})).await;
    assert_eq!(state["values"]["text"], "live", "{state}");
    assert!(items(&mut m, &id).await.is_empty());
    let first = m.call_tool("wait", json!({"launch_id": id, "request_id": "u-1", "op": {"type": "until", "context": "popup-1", "max_ms": 2000,
        "predicate": {"is": "count_at_least", "target": {"by": "role", "role": "listitem"}, "n": 1}}})).await;
    assert_eq!(first["outcome"]["type"], "condition_met", "{first}");
    assert!((500_000..520_000).contains(&(first["t_us"].as_u64().unwrap() - at)), "the first message at 0.5 s: {first}");
    assert_eq!(items(&mut m, &id).await, ["Ada replied"]);
    m.call_tool("wait", json!({"launch_id": id, "request_id": "a-2", "op": {"type": "advance", "by_ms": 1100}})).await;
    assert_eq!(items(&mut m, &id).await, ["Ada replied", "Grace joined"]);
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// The recorder dies between a window's two messages; the relaunched one takes the window's stream back
/// and delivers the second at its instant.
#[tokio::test]
async fn a_popups_stream_survives_recorder_replacement() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": feed_rules(), "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Notifications"}}, "then": {"type": "none"}})).await;
    m.call_tool("wait", json!({"launch_id": id, "request_id": "a-1", "op": {"type": "advance", "by_ms": 1000}})).await;
    assert_eq!(items(&mut m, &id).await, ["Ada replied"]);
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
    m.call_tool("wait", json!({"launch_id": id, "request_id": "a-2", "op": {"type": "advance", "by_ms": 1000}})).await;
    assert_eq!(items(&mut m, &id).await, ["Ada replied", "Grace joined"]);
    m.call_tool("stop", json!({"launch_id": id})).await;
}

const VOLUME: &str = r#"<!doctype html><div id=track style="position:absolute;left:20px;top:40px;width:200px;height:20px;background:#ccc"></div>
<p id=val style="margin-top:80px">0%</p><p id=state>idle</p><p id=took></p><input id=name>
<script>
const track = document.getElementById('track'), val = document.getElementById('val'), state = document.getElementById('state'), took = document.getElementById('took');
let down = false, t0 = 0;
const set = (x) => { val.textContent = Math.max(0, Math.min(100, Math.round((x - 20) / 2))) + '%'; };
track.onpointerdown = (e) => { down = true; t0 = performance.now(); track.setPointerCapture(e.pointerId); state.textContent = 'adjusting'; set(e.clientX); };
track.onpointermove = (e) => { if (down) set(e.clientX); };
track.onpointerup = () => { down = false; state.textContent = 'saved ' + val.textContent; took.textContent = Math.round(performance.now() - t0) + ' ms'; };
track.onpointercancel = () => { down = false; state.textContent = 'cancelled'; };
document.getElementById('name').addEventListener('keydown', (e) => { if (e.key === 'Shift') state.textContent = 'shift held'; });
document.getElementById('name').addEventListener('keyup', (e) => { if (e.key === 'Shift') state.textContent = 'shift released'; });
</script>"#;

async fn popup_text(m: &mut support::Mcp, id: &Value, selector: &str) -> String {
    let read = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "query", "target": {"by": "css", "selector": selector}, "props": ["text"]}})).await;
    read["values"]["text"].as_str().unwrap().to_string()
}

/// A settings window's slider is dragged with a held pointer and a key is held in its field: the input is
/// held in the window, later input follows it there without naming it, releasing the lease cancels the
/// window's pointer, and the run replays.
#[tokio::test]
async fn held_input_is_held_in_a_popup() {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    const APP: &str = r#"<!doctype html><button onclick="window.open('https://app.example.test/volume', 'volume')">Volume</button>"#;
    let rules = json!([page("app", "https://app.example.test/", APP), page("volume", "https://app.example.test/volume", VOLUME)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Volume"}}})).await;
    let scene = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "scene"}})).await;
    let track = json!({"by": "css", "selector": "#track"});
    let begun = m.call_tool("input", json!({"launch_id": id, "request_id": "drag", "operation": "begin", "context": "popup-1", "target": track, "point": [40, 50], "boundary": scene["boundary"]})).await;
    assert_eq!(begun["held"]["context"], "popup-1", "{begun}");
    assert_eq!((popup_text(&mut m, &id, "#state").await, popup_text(&mut m, &id, "#val").await), ("adjusting".into(), "10%".into()));
    // The move names no context: it follows the pointer held in the window.
    let moved = m.call_tool("input", json!({"launch_id": id, "request_id": "move", "operation": "move", "handle": begun["held"]["handle"], "point": [160, 50], "boundary": begun["boundary"]})).await;
    assert_eq!(popup_text(&mut m, &id, "#val").await, "70%", "{moved}");
    let (refused, wrong) = m.try_tool("input", json!({"launch_id": id, "request_id": "wrong", "operation": "begin", "context": "page", "target": track, "point": [40, 50], "boundary": moved["boundary"]})).await;
    assert!(refused, "a second pointer is not begun while one is held: {wrong}");
    m.call_tool("input", json!({"launch_id": id, "request_id": "end", "operation": "end", "handle": begun["held"]["handle"]})).await;
    assert_eq!(popup_text(&mut m, &id, "#state").await, "saved 70%");

    m.call_tool("act", json!({"launch_id": id, "request_id": "c-2", "context": "popup-1", "action": {"type": "click", "target": {"by": "css", "selector": "#name"}}})).await;
    let held = m.call_tool("input", json!({"launch_id": id, "request_id": "shift", "operation": "key_down", "key": "Shift", "context": "popup-1"})).await;
    assert_eq!(held["keys"], json!(["Shift"]), "{held}");
    assert_eq!(popup_text(&mut m, &id, "#state").await, "shift held");
    m.call_tool("input", json!({"launch_id": id, "request_id": "unshift", "operation": "key_up", "key": "Shift"})).await;
    assert_eq!(popup_text(&mut m, &id, "#state").await, "shift released");

    // Released with the pointer held, the lease's cleanup cancels it in the window.
    let scene = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "scene"}})).await;
    m.call_tool("input", json!({"launch_id": id, "request_id": "drag-2", "operation": "begin", "context": "popup-1", "target": track, "point": [60, 50], "boundary": scene["boundary"]})).await;
    m.call_tool("session", json!({"launch_id": id, "operation": "release"})).await;
    m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
    assert_eq!(popup_text(&mut m, &id, "#state").await, "cancelled");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// A slow drag in a settings window: the time it grants before each move runs the clock, the window's
/// slider sees the moves and the time between them, and the run replays.
#[tokio::test]
async fn a_granted_drag_runs_in_a_popup() {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    const APP: &str = r#"<!doctype html><button onclick="window.open('https://app.example.test/volume', 'volume')">Volume</button>"#;
    let rules = json!([page("app", "https://app.example.test/", APP), page("volume", "https://app.example.test/volume", VOLUME)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Volume"}}})).await;
    let dragged = m.call_tool("act", json!({"launch_id": id, "request_id": "d-1", "context": "popup-1", "then": {"type": "none"}, "action": {
        "type": "drag", "target": {"by": "css", "selector": "#track"}, "offset": {"dx": 60, "dy": 0}, "steps": 3, "interval_ms": 100
    }})).await;
    assert_eq!(dragged["t_after_us"].as_u64().unwrap() - dragged["t_before_us"].as_u64().unwrap(), 300_000, "{dragged}");
    assert_eq!((popup_text(&mut m, &id, "#state").await, popup_text(&mut m, &id, "#took").await), ("saved 80%".into(), "300 ms".into()));
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// The recorder dies while a pointer is held in a window: the relaunched recorder's cleanup cancels it
/// there, and the generation goes on.
#[tokio::test]
async fn a_pointer_held_in_a_popup_is_cancelled_after_recorder_loss() {
    let page = |id: &str, url: &str, body: &str| json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    const APP: &str = r#"<!doctype html><button onclick="window.open('https://app.example.test/volume', 'volume')">Volume</button>"#;
    let rules = json!([page("app", "https://app.example.test/", APP), page("volume", "https://app.example.test/volume", VOLUME)]);
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/", "rules": rules, "passthrough_origins": []})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", json!({"launch_id": id, "request_id": "c-1", "action": {"type": "click", "target": {"by": "role", "role": "button", "name": "Volume"}}})).await;
    let scene = m.call_tool("observe", json!({"launch_id": id, "context": "popup-1", "op": {"type": "scene"}})).await;
    m.call_tool("input", json!({"launch_id": id, "request_id": "drag", "operation": "begin", "context": "popup-1", "target": {"by": "css", "selector": "#track"}, "point": [40, 50], "boundary": scene["boundary"]})).await;
    assert_eq!(popup_text(&mut m, &id, "#state").await, "adjusting");
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id": id, "operation": "reattach"})).await;
    let status = m.call_tool("input", json!({"launch_id": id, "operation": "status"})).await;
    assert!(status["held"].is_null(), "{status}");
    assert_eq!(popup_text(&mut m, &id, "#state").await, "cancelled");
    m.call_tool("stop", json!({"launch_id": id})).await;
}
