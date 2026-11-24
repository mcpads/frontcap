//! Ordered document admission (engine patches 50 and 51): child documents parse in the order their
//! navigations started, each once the instant can do nothing else, so strict replay agrees whatever
//! the host timing of commits and grants. No storage initialization or IO is involved.
mod support;
use serde_json::{Value, json};

#[tokio::test]
async fn sibling_and_nested_document_admission_replays_without_storage() {
    check_replay(true).await;
}

#[tokio::test]
async fn document_admission_replays_without_storage_or_relocation() {
    check_replay(false).await;
}

async fn check_replay(relocate: bool) {
    let origin = "https://app.ancestry.test";
    let sibling = "https://sub.ancestry.test";
    let cross = "https://cross-ancestor.test";
    let leaf = |role: &str| {
        format!(
            "<!doctype html><script>parent.postMessage({{role:'{role}'}},'*')</script><p>{role}</p>"
        )
    };
    let bridge = format!(
        r#"<!doctype html><script>addEventListener('message',e=>{{if(e.source===document.querySelector('iframe').contentWindow)parent.postMessage(e.data,'*')}})</script><iframe src="{origin}/nested"></iframe>"#
    );
    let parent = format!(
        r#"<!doctype html><output id="status">loading</output><script>
const received=new Set();addEventListener('message',e=>{{const expected={{direct:'direct',sibling:'sibling',nested:'bridge'}}[e.data.role];if(!expected||e.source!==document.querySelector('#'+expected).contentWindow)return;received.add(e.data.role);if(received.size===3)document.querySelector('#status').textContent='ready'}});
</script><iframe id="direct" src="{origin}/direct"></iframe><iframe id="sibling" src="{sibling}/leaf"></iframe><iframe id="bridge" src="{cross}/bridge"></iframe>"#
    );
    let rules = vec![
        html("parent", &format!("{origin}/start"), &parent),
        html("direct", &format!("{origin}/direct"), &leaf("direct")),
        html("sibling", &format!("{sibling}/leaf"), &leaf("sibling")),
        html("bridge", &format!("{cross}/bridge"), &bridge),
        html("nested", &format!("{origin}/nested"), &leaf("nested")),
    ];
    let relocated = relocate.then_some("https://moved.ancestry.test/start");
    let status = run(rules, &format!("{origin}/start"), "ready", relocated).await;
    assert_eq!(status, "ready");
}

/// A child whose parser waits on a stylesheet and a blocking script finishes those counted loads
/// before a later sibling's body is admitted.
#[tokio::test]
async fn a_childs_blocking_script_and_stylesheet_precede_a_later_siblings_body() {
    let origin = "https://app.order.test";
    let sibling = "https://other.order.test";
    let child = r#"<!doctype html><link rel="stylesheet" href="/a.css"><script src="/a.js"></script><p>a</p>"#;
    let rules = vec![
        html(
            "parent",
            &format!("{origin}/start"),
            &parent(2, &format!(r#"<iframe src="{origin}/a"></iframe><iframe src="{sibling}/b"></iframe>"#)),
        ),
        html("a", &format!("{origin}/a"), child),
        typed("a-css", &format!("{origin}/a.css"), "text/css", "p{color:red}"),
        typed("a-js", &format!("{origin}/a.js"), "text/javascript", "parent.postMessage({role:'a'},'*')"),
        html("b", &format!("{sibling}/b"), &leaf("b")),
    ];
    let status = run(rules, &format!("{origin}/start"), "a,b", None).await;
    assert_eq!(status, "a,b");
}

/// A redirected navigation keeps its place; one answered without content commits nothing and
/// does not hold the instant.
#[tokio::test]
async fn a_redirected_child_keeps_its_place_and_a_no_content_child_ends_its_turn() {
    let origin = "https://app.order.test";
    let sibling = "https://other.order.test";
    let rules = vec![
        html(
            "parent",
            &format!("{origin}/start"),
            &parent(
                2,
                &format!(
                    r#"<iframe src="{origin}/r"></iframe><iframe id="n" src="{origin}/n"></iframe><iframe src="{sibling}/s"></iframe>"#
                ),
            ),
        ),
        json!({"id":"r","match":{"url":format!("{origin}/r")},"action":{"type":"mock","status":302,"headers":{"location":format!("{origin}/r2")},"body":""}}),
        html("r2", &format!("{origin}/r2"), &leaf("r")),
        json!({"id":"n","match":{"url":format!("{origin}/n")},"action":{"type":"mock","status":204,"headers":{},"body":""}}),
        html("s", &format!("{sibling}/s"), &leaf("s")),
    ];
    let (status, requests) = run_traced(rules, &format!("{origin}/start"), "r,s", None).await;
    assert_eq!(status, "r,s");
    let paths: Vec<&str> = requests.iter().map(|u| u.rsplit('/').next().unwrap_or_default()).collect();
    assert_eq!(paths, ["start", "r", "r2", "n", "s"], "the redirect chain keeps the first hop's place");
}

/// A document removed before its body is admitted gives up its turn: one removed while its
/// navigation is in flight, and one a document admitted earlier removes.
#[tokio::test]
async fn documents_removed_before_admission_give_up_their_turn() {
    let origin = "https://app.order.test";
    let sibling = "https://other.order.test";
    let remover = "<!doctype html><script>parent.document.querySelector('#c').remove();parent.postMessage({role:'a'},'*')</script>";
    let rules = vec![
        html(
            "parent",
            &format!("{origin}/start"),
            &parent(
                2,
                &format!(
                    r#"<iframe src="{origin}/a"></iframe><iframe id="c" srcdoc="<script>parent.postMessage({{role:'c'}},'*')</script>"></iframe><iframe id="d" srcdoc="<script>parent.postMessage({{role:'d'}},'*')</script>"></iframe><script>document.querySelector('#d').remove()</script><iframe src="{sibling}/b"></iframe>"#
                ),
            ),
        ),
        html("a", &format!("{origin}/a"), remover),
        html("b", &format!("{sibling}/b"), &leaf("b")),
    ];
    let status = run(rules, &format!("{origin}/start"), "a,b", None).await;
    assert_eq!(status, "a,b");
}

/// A native dialog in a child's admitted body suspends admission; the later sibling follows once
/// the dialog is answered.
#[tokio::test]
async fn a_native_dialog_in_an_admitted_child_keeps_the_admission_order() {
    let origin = "https://app.order.test";
    let foreign = "https://foreign.order.test";
    let rules = vec![
        html(
            "parent",
            &format!("{origin}/start"),
            &parent(2, &format!(r#"<iframe src="{foreign}/a"></iframe><iframe src="{origin}/b"></iframe>"#)),
        ),
        html(
            "a",
            &format!("{foreign}/a"),
            "<!doctype html><script>alert('a');parent.postMessage({role:'a'},'*')</script>",
        ),
        html("b", &format!("{origin}/b"), &leaf("b")),
    ];
    let status = run(rules, &format!("{origin}/start"), "a,b", None).await;
    assert_eq!(status, "a,b");
}

fn html(id: &str, url: &str, body: &str) -> Value {
    typed(id, url, "text/html", body)
}

fn typed(id: &str, url: &str, content_type: &str, body: &str) -> Value {
    json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":content_type},"body":body}})
}

fn leaf(role: &str) -> String {
    format!("<!doctype html><script>parent.postMessage({{role:'{role}'}},'*')</script><p>{role}</p>")
}

/// A page whose `#status` becomes the order its children's messages arrived in, once `expected` arrived.
fn parent(expected: usize, frames: &str) -> String {
    format!(
        r#"<!doctype html><output id="status">loading</output><script>
const order=[];addEventListener('message',e=>{{order.push(e.data.role);if(order.length==={expected})document.querySelector('#status').textContent=order.join(',')}});
</script>{frames}"#
    )
}

/// Launches `start`, waits for `#status` to read `ready`, and requires the replay (relocated to
/// `relocate` when given) to equal the recording. Returns the status text.
async fn run(rules: Vec<Value>, start: &str, ready: &str, relocate: Option<&str>) -> String {
    run_traced(rules, start, ready, relocate).await.0
}

/// As `run`, also returning the recorded requests' URLs in trace order.
async fn run_traced(rules: Vec<Value>, start: &str, ready: &str, relocate: Option<&str>) -> (String, Vec<String>) {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m
        .call_tool(
            "launch",
            json!({"url":start,"rules":rules,"then":{"type":"none"}}),
        )
        .await;
    let id = &launched["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":ready},"max_ms":2000}})).await;
    let state=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#status"},"props":["text"]}})).await;
    let status = state["values"]["text"].as_str().unwrap_or_default().to_string();
    let stopped = m.call_tool("stop", json!({"launch_id":id})).await;
    let recorded = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let requests = recorded
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|e| e["kind"] == "request")
        .map(|e| e["payload"]["url"].as_str().unwrap_or_default().to_string())
        .collect();
    let replay = m
        .call_tool(
            "debug",
            match relocate {
                Some(url) => json!({"launch_id":id,"operation":"replay","url":url}),
                None => json!({"launch_id":id,"operation":"replay"}),
            },
        )
        .await;
    if replay["equal"] != true {
        let trace = &recorded;
        let replay_trace = std::fs::read_to_string(
            ws.root()
                .join("runs")
                .join(replay["run"].as_str().unwrap())
                .join("trace.jsonl"),
        )
        .unwrap();
        eprintln!("recorded trace:\n{trace}\nreplayed trace:\n{replay_trace}");
    }
    println!("DOCUMENT_ADMISSION_REPLAY {replay}");
    assert_eq!(replay["equal"], true, "{replay}");
    (status, requests)
}
