//! One active window, with focus transitions at the operation's virtual instant.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::{ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};

const PAGE: &str = r#"<!doctype html><button id=open onclick="window.open('https://auth.example.test/', 'child')">open</button>
<button id=page>page</button><script>
window.events = [];
for (const kind of ['focus', 'blur']) addEventListener(kind, () => events.push([kind, Date.now(), document.hasFocus()]));
</script>"#;
const POPUP: &str = r#"<!doctype html><button id=button>popup</button><button id=close onclick="window.close()">close</button><script>
window.events = [];
window.focusedAtLoad = document.hasFocus();
for (const kind of ['focus', 'blur']) addEventListener(kind, () => events.push([kind, Date.now(), document.hasFocus()]));
</script>"#;

async fn read(g: &mut Generation, context: &str) -> Value {
    g.enter_context(context).unwrap();
    let value = g.eval_main("JSON.stringify({focused:document.hasFocus(),events,loaded:globalThis.focusedAtLoad})").await.unwrap();
    g.leave_context();
    serde_json::from_str(value.as_str().unwrap()).unwrap()
}

async fn click(g: &mut Generation, context: &str, id: &str, selector: &str) {
    g.enter_context(context).unwrap();
    g.click(RequestId::new(id).unwrap(), &Locator::Css { selector: selector.into() }).await.unwrap();
    g.leave_context();
}

#[tokio::test]
async fn opening_switching_and_closing_windows_deliver_focus_once_at_their_instants() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mock = |id: &str, url: &str, body: &str| json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}});
    let rules = RuleSet::from_json(&json!([
        mock("page", "https://app.example.test/", PAGE),
        mock("popup", "https://auth.example.test/", POPUP)
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "window-focus", "https://app.example.test/".into(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(read(&mut g, "page").await, json!({"focused":true,"events":[]}));

    let opened_at = epoch + g.now().0 / 1000;
    click(&mut g, "page", "open", "#open").await;
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(read(&mut g, "page").await, json!({"focused":false,"events":[["blur",opened_at,false]]}));
    assert_eq!(read(&mut g, "popup-1").await, json!({"focused":true,"loaded":true,"events":[]}));

    let at = epoch + g.now().0 / 1000;
    // Clicking an already active window must not invent another activation.
    click(&mut g, "popup-1", "popup-click", "#button").await;
    assert_eq!(read(&mut g, "popup-1").await, json!({"focused":true,"loaded":true,"events":[]}));
    click(&mut g, "page", "page-click", "#page").await;
    assert_eq!(read(&mut g, "page").await, json!({"focused":true,"events":[["blur",opened_at,false],["focus",at,true]]}));
    assert_eq!(read(&mut g, "popup-1").await, json!({"focused":false,"loaded":true,"events":[["blur",at,false]]}));
    click(&mut g, "popup-1", "popup-again", "#button").await;
    assert_eq!(read(&mut g, "page").await, json!({"focused":false,"events":[["blur",opened_at,false],["focus",at,true],["blur",at,false]]}));
    assert_eq!(read(&mut g, "popup-1").await, json!({"focused":true,"loaded":true,"events":[["blur",at,false],["focus",at,true]]}));
    click(&mut g, "popup-1", "close", "#close").await;
    assert_eq!(read(&mut g, "page").await, json!({"focused":true,"events":[["blur",opened_at,false],["focus",at,true],["blur",at,false],["focus",at,true]]}));
    assert_eq!(epoch + g.now().0 / 1000, at, "input and observation do not advance time");
    g.stop().await.unwrap();
}
