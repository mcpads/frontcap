//! Native frozen drags, explicitly paced drags, and uploads.
mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::action::{Action, Offset};
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched_at(addr: std::net::SocketAddr, run: &str) -> (tempfile::TempDir, Generation) {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/drag.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    (runs, g)
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

async fn log(g: &Generation) -> serde_json::Value {
    serde_json::from_str(g.eval_main("JSON.stringify(log.splice(0))").await.unwrap().as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn a_pointer_drag_moves_with_the_button_held_and_releases_at_the_offset() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-1").await;
    let t = g.now();
    g.act_input(rid("d-1"), &Action::Drag { target: css("#handle"), to: None, offset: Some(Offset { dx: 100, dy: 0 }), steps: Some(4), interval_us: 0 }).await.unwrap();
    let events = log(&g).await;
    let names: Vec<&str> = events.as_array().unwrap().iter().map(|e| e[0].as_str().unwrap()).collect();
    assert_eq!(names, ["pointermove", "pointerdown", "pointermove", "pointermove", "pointermove", "pointermove", "pointerup"], "{events}");
    let held: Vec<(i64, i64)> = events.as_array().unwrap()[2..6].iter().map(|e| (e[1].as_i64().unwrap(), e[3].as_i64().unwrap())).collect();
    assert_eq!(held, [(65, 1), (90, 1), (115, 1), (140, 1)], "moves with the button held");
    assert_eq!(&events[6][1], 140, "{events}");
    assert_eq!(g.eval_main("document.getElementById('handle').style.left").await.unwrap(), "120px");
    assert_eq!(g.now(), t, "the default drag grants no virtual progress");
    assert_eq!(starved(&g), 0, "every move must be delivered while frozen");
    for event in events.as_array().unwrap() {
        assert_eq!(event.as_array().unwrap().last().unwrap(), 50, "{events}");
    }
}

fn starved_in(trace: &str) -> usize {
    trace.matches("\"frame_starvation\"").count()
}

fn starved(g: &Generation) -> usize {
    starved_in(&std::fs::read_to_string(g.trace_path()).unwrap())
}

#[tokio::test]
async fn an_html_drag_and_drop_delivers_its_data_to_the_drop_target() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-2").await;
    g.act_input(rid("d-1"), &Action::Drag { target: css("#src"), to: Some(css("#dst")), offset: None, steps: None, interval_us: 0 }).await.unwrap();
    let events = log(&g).await;
    let names: Vec<&str> = events.as_array().unwrap().iter().map(|e| e[0].as_str().unwrap()).collect();
    assert_eq!(names.first(), Some(&"dragstart"), "{events}");
    assert!(names.contains(&"drop") && names.last() == Some(&"dragend"), "{events}");
    let drop = events.as_array().unwrap().iter().find(|e| e[0] == "drop").unwrap();
    assert_eq!(drop[1], "payload");
}

#[tokio::test]
async fn a_drag_is_refused_before_any_input_when_it_cannot_happen() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-3").await;
    let both = Action::Drag { target: css("#handle"), to: Some(css("#dst")), offset: Some(Offset { dx: 1, dy: 1 }), steps: None, interval_us: 0 };
    assert!(matches!(g.act_input(rid("d-1"), &both).await, Err(ChromiumError::InvalidArgument(_))));
    let off = Action::Drag { target: css("#handle"), to: None, offset: Some(Offset { dx: -500, dy: 0 }), steps: None, interval_us: 0 };
    assert!(matches!(g.act_input(rid("d-2"), &off).await, Err(ChromiumError::NotActionable(_))));
    assert_eq!(log(&g).await, serde_json::json!([]));
}

#[tokio::test]
async fn an_upload_sets_the_files_from_the_bundle_at_the_initial_date() {
    let (runs, mut g) = launched_at(support::serve(), "dr-4").await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.txt"), dir.path().join("photo.png"));
    std::fs::write(&a, "hello").unwrap();
    std::fs::write(&b, [0x89u8, b'P', b'N', b'G']).unwrap();
    let files = vec![a.display().to_string(), b.display().to_string()];
    g.act_input(rid("u-1"), &Action::Upload { target: css("#f"), files }).await.unwrap();
    let events = log(&g).await;
    let expected = serde_json::json!([["a.txt", 5, "text/plain", 946_684_800_000u64], ["photo.png", 4, "image/png", 946_684_800_000u64]]);
    assert_eq!((&events[0][0], &events[0][1]), (&serde_json::json!("input"), &expected), "{events}");
    assert_eq!(events[1][0], "change");
    let copies = std::fs::read_dir(runs.path().join("dr-4/inputs/uploads")).unwrap().count();
    assert_eq!(copies, 2, "each file is kept in the bundle");
    let not_file = Action::Upload { target: css("#handle"), files: vec![a.display().to_string()] };
    assert!(matches!(g.act_input(rid("u-2"), &not_file).await, Err(ChromiumError::NotActionable(_))));
}

/// Two runs with the same drag and upload record the same behaviour (criterion 2's projection).
#[tokio::test]
async fn drag_and_upload_repeat_exactly() {
    async fn run(addr: std::net::SocketAddr, name: &str, file: &str) -> String {
        let (_runs, mut g) = launched_at(addr, name).await;
        g.act_input(rid("d-1"), &Action::Drag { target: css("#handle"), to: None, offset: Some(Offset { dx: 60, dy: 30 }), steps: None, interval_us: 0 }).await.unwrap();
        g.advance(VirtualDuration::from_ms(30)).await.unwrap();
        g.act_input(rid("d-2"), &Action::Drag { target: css("#src"), to: Some(css("#dst")), offset: None, steps: None, interval_us: 0 }).await.unwrap();
        g.act_input(rid("u-1"), &Action::Upload { target: css("#f"), files: vec![file.into()] }).await.unwrap();
        g.advance(VirtualDuration::from_ms(30)).await.unwrap();
        let path = g.trace_path();
        g.stop().await.unwrap();
        std::fs::read_to_string(path).unwrap()
    }
    // Repeatability holds for runs whose moves all met a frame; a run where one did not says so.
    let untainted = |t: &str| match starved_in(t) {
        0 => true,
        1 => false,
        n => panic!("{n} moves missed their frame"),
    };
    let addr = support::serve();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "hello").unwrap();
    let file = file.display().to_string();
    let first = run(addr, "dr-5a", &file).await;
    let second = run(addr, "dr-5b", &file).await;
    if untainted(&first) && untainted(&second) {
        assert_eq!(frontcap_core::projection::compare_behavior(&first, &second), Ok(()));
    }
    assert!(first.contains("\"sha256\":\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\""), "the trace names the file by its hash");
}

/// A sortable list removes the dragged item during the drag: no `dragend` reaches the page's window,
/// and the drop is the release's receipt.
#[tokio::test]
async fn an_html_drag_whose_source_is_removed_still_drops() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-6").await;
    g.act_input(rid("d-1"), &Action::Drag { target: css("#gone"), to: Some(css("#dst")), offset: None, steps: None, interval_us: 17_000 }).await.unwrap();
    let events = log(&g).await;
    let drop = events.as_array().unwrap().iter().find(|e| e[0] == "drop").unwrap_or_else(|| panic!("{events}"));
    assert_eq!(drop[1], "gone");
}

/// A press that navigates: the drag stops with a defined error, the button is released, and the next
/// action in the new document works.
#[tokio::test]
async fn a_drag_whose_document_changes_stops_and_releases_the_button() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-7").await;
    let leave = Action::Drag { target: css("#leave"), to: None, offset: Some(Offset { dx: 0, dy: 100 }), steps: Some(3), interval_us: 0 };
    match g.act_input(rid("d-1"), &leave).await {
        Err(ChromiumError::NotActionable(m)) => assert!(m.contains("document changed"), "{m}"),
        other => panic!("{other:?}"),
    }
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(trace.contains("\"input_aborted\""), "the release is recorded");
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.act_input(rid("c-1"), &Action::Click { target: css("#save") }).await.unwrap();
    assert_eq!(g.eval_main("window.__clicks").await.unwrap(), serde_json::json!(1), "no drag holds the pointer");
}

#[tokio::test]
async fn uploads_to_hidden_inputs_and_refusals() {
    let (_runs, mut g) = launched_at(support::serve(), "dr-8").await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();
    let (a, b) = (a.display().to_string(), b.display().to_string());
    g.act_input(rid("u-1"), &Action::Upload { target: css("#hidden"), files: vec![a.clone()] }).await.unwrap();
    assert_eq!(log(&g).await.as_array().unwrap().iter().filter(|e| e[0] == "change-hidden").count(), 1);
    // Through the control in front of it, as a user does: the chooser it opens names the input.
    g.act_input(rid("u-2"), &Action::Upload { target: css("#choose"), files: vec![b.clone()] }).await.unwrap();
    let changes: Vec<serde_json::Value> = log(&g).await.as_array().unwrap().iter().filter(|e| e[0] == "change-hidden").cloned().collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0][1], serde_json::json!(["b.txt"]));
    let opens_nothing = Action::Upload { target: css("#b"), files: vec![a.clone()] };
    assert!(matches!(g.act_input(rid("u-3"), &opens_nothing).await, Err(ChromiumError::NotActionable(m)) if m.contains("no file chooser")));
    log(&g).await;
    let refused = [
        (Action::Upload { target: css("#disabled"), files: vec![a.clone()] }, "disabled"),
        (Action::Upload { target: css("#hidden"), files: vec![a.clone(), b.clone()] }, "one file"),
        (Action::Upload { target: css("#f"), files: vec![] }, "at least one"),
        (Action::Upload { target: css("#f"), files: vec![dir.path().display().to_string()] }, "regular file"),
        (Action::Drag { target: css("#handle"), to: None, offset: Some(Offset { dx: 0, dy: 0 }), steps: None, interval_us: 0 }, "nowhere"),
    ];
    for (i, (action, why)) in refused.into_iter().enumerate() {
        let e = g.act_input(rid(&format!("x-{i}")), &action).await.unwrap_err();
        assert!(e.to_string().contains(why), "{why}: {e}");
    }
    assert_eq!(log(&g).await, serde_json::json!([]), "nothing reached the page");
}

const DROP_ZONE: &str = r#"<!doctype html><style>body{margin:0}#dst{width:150px;height:100px}#inner{width:60px;height:30px}</style><div id=dst>drop here</div><div id=inner draggable=true>inner</div><script>
const P = (m) => parent.postMessage(m, '*');
const d = document.getElementById('dst');
for (const t of ['dragenter', 'dragover']) d.addEventListener(t, (e) => { e.preventDefault(); P(t); });
d.addEventListener('drop', (e) => { e.preventDefault(); P('drop ' + e.dataTransfer.getData('text/plain')); });
const i = document.getElementById('inner');
i.addEventListener('dragstart', (e) => { e.dataTransfer.setData('text/plain', 'inner'); P('inner dragstart'); });
i.addEventListener('dragend', (e) => P('inner dragend ' + e.dataTransfer.dropEffect));
</script>"#;

/// An HTML drag between documents: into or out of a frame of its origin it is dragged and dropped as
/// within one; Chrome lets a drag between a frame of another origin and its page enter nothing
/// (crbug.com/59081). Each drag completes at its instant with no move left undelivered: the engine's
/// completion of a move no page event received is its receipt.
#[tokio::test]
async fn html_drags_cross_documents_as_chrome_lets_them() {
    let page = format!(r#"<!doctype html><style>body{{margin:0}}#src{{position:absolute;left:20px;top:20px;width:60px;height:40px}}#pdst{{position:absolute;left:20px;top:300px;width:150px;height:80px}}iframe{{position:absolute;top:120px;width:200px;height:150px;border:0}}#same{{left:20px}}#other{{left:300px}}</style>
<div id=src draggable=true>drag me</div><div id=pdst>parent drop</div><iframe id=same srcdoc="{}"></iframe><iframe id=other src="https://zone.example.test/"></iframe>
<script>window.log=[];addEventListener('message',(e)=>log.push(e.data));const s=document.getElementById('src');s.addEventListener('dragstart',(e)=>{{e.dataTransfer.setData('text/plain','payload');log.push('dragstart')}});s.addEventListener('dragend',(e)=>log.push('dragend '+e.dataTransfer.dropEffect));
const p=document.getElementById('pdst');for(const t of ['dragenter','dragover'])p.addEventListener(t,(e)=>e.preventDefault());p.addEventListener('drop',(e)=>{{e.preventDefault();log.push('parent drop '+e.dataTransfer.getData('text/plain'))}});</script>"#, DROP_ZONE.replace('"', "&quot;"));
    let mock = |id: &str, url: &str, body: &str| serde_json::json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": body}});
    let rules = RuleSet::from_json(&serde_json::json!([mock("app", "https://app.example.test/", &page), mock("zone", "https://zone.example.test/", DROP_ZONE)]).to_string()).unwrap();
    let framed = |frame: &str, t: &str| Locator::Frame { frame: Box::new(css(frame)), target: Box::new(css(t)) };
    for (name, from, to, seen) in [
        ("into its origin's frame", css("#src"), framed("#same", "#dst"), &["dragstart", "dragend copy", "dragenter", "dragover", "drop payload"][..]),
        ("into another origin's frame", css("#src"), framed("#other", "#dst"), &["dragstart", "dragend none"][..]),
        ("out of its origin's frame", framed("#same", "#inner"), css("#pdst"), &["parent drop inner", "inner dragstart", "inner dragend copy"][..]),
        ("out of another origin's frame", framed("#other", "#inner"), css("#pdst"), &["inner dragstart", "inner dragend none"][..]),
    ] {
        let runs = tempfile::tempdir().unwrap();
        let mut options = support::options(runs.path(), "dr-xdoc", "https://app.example.test/".into(), rules.clone());
        options.passthrough_origins = Some(vec![]);
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        let at = g.now();
        g.act_input(rid("d-1"), &Action::Drag { target: from, to: Some(to), offset: None, steps: None, interval_us: 0 }).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(g.now(), at, "{name}");
        assert_eq!(starved(&g), 0, "{name}: every move delivered at its instant");
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        let log: Vec<String> = serde_json::from_str(g.eval_main("JSON.stringify(log)").await.unwrap().as_str().unwrap()).unwrap();
        let mut log: Vec<&str> = log.iter().map(String::as_str).collect();
        log.dedup();
        assert_eq!(log, seen, "{name}");
        g.stop().await.unwrap();
    }
}
