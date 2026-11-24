mod support;

use frontcap_chromium::ChromiumError;
use frontcap_chromium::generation::{Generation, Prop};
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    launched_at(support::serve(), run, page).await
}

async fn launched_at(addr: std::net::SocketAddr, run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    (runs, g)
}

fn ref_of(s: &frontcap_chromium::generation::Snapshot, role: &str, name: &str) -> Locator {
    let n = s.nodes.iter().find(|n| n.role == role && n.name == name).unwrap_or_else(|| panic!("{role} {name} in {:?}", s.nodes));
    Locator::Ref { id: n.r#ref.clone() }
}

#[tokio::test]
async fn snapshot_lists_interactive_nodes_and_text() {
    let (_runs, mut g) = launched("obs-1", "form.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let lines: Vec<String> = s.nodes.iter().map(|n| format!("{} {:?} {:?}", n.role, n.name, n.value)).collect();
    for expected in ["heading \"Profile\" None", "button \"Save\" None", "textbox \"Name\" Some(\"x\")", "link \"Help\" None", "listitem \"\" None", "dialog \"Panel\" None", "text \"Inside\" None"] {
        assert!(lines.iter().any(|l| l == expected), "{expected} in {lines:#?}");
    }
    assert!(s.nodes.iter().all(|n| n.r#ref.starts_with('e')));
    assert_eq!(s.omitted, 0);
}

#[tokio::test]
async fn refs_are_stable_within_a_document_and_act_on_a_ref() {
    let (_runs, mut g) = launched("obs-2", "form.html").await;
    let a = g.snapshot(None, None, 400).await.unwrap();
    let b = g.snapshot(None, None, 400).await.unwrap();
    assert_eq!(ref_of(&a, "button", "Save"), ref_of(&b, "button", "Save"));
    assert!(b.observation_id > a.observation_id);
    g.click(RequestId::new("c-1").unwrap(), &ref_of(&b, "button", "Save")).await.unwrap();
    assert_eq!(g.eval_main("window.__clicks").await.unwrap(), 1);
}

#[tokio::test]
async fn refs_go_stale_on_navigation() {
    let (_runs, mut g) = launched("obs-3", "form.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let save = ref_of(&s, "button", "Save");
    g.eval_main("leave()").await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    let err = g.click(RequestId::new("c-1").unwrap(), &save).await.unwrap_err();
    assert!(matches!(err, ChromiumError::StaleRef(_)), "{err:?}");
    g.snapshot(None, None, 400).await.unwrap();
}

#[tokio::test]
async fn a_detached_node_is_a_stale_ref() {
    let (_runs, mut g) = launched("obs-4", "form.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    g.eval_main("detach()").await.unwrap();
    let err = g.click(RequestId::new("c-1").unwrap(), &ref_of(&s, "button", "Save")).await.unwrap_err();
    assert!(matches!(err, ChromiumError::StaleRef(_)), "{err:?}");
}

#[tokio::test]
async fn diff_reports_added_removed_changed_and_cursors_expire() {
    let (_runs, mut g) = launched("obs-5", "form.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    let save = ref_of(&s, "button", "Save");
    g.eval_main("later()").await.unwrap();
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let d = g.diff(s.observation_id, 400, true).await.unwrap();
    assert!(d.added.iter().any(|n| n.role == "listitem"), "{d:?}");
    assert!(d.changed.iter().any(|n| Locator::Ref { id: n.r#ref.clone() } == save && n.name == "Saved"), "{d:?}");
    for _ in 0..16 {
        g.snapshot(None, None, 10).await.unwrap();
    }
    assert!(matches!(g.diff(s.observation_id, 400, true).await, Err(ChromiumError::StaleCursor(_))));
}

#[tokio::test]
async fn query_reads_only_the_requested_props() {
    let (_runs, mut g) = launched("obs-6", "form.html").await;
    let props = [Prop::Value, Prop::Visible, Prop::Attr("aria-label".into()), Prop::Style("display".into())];
    let q = g.query(&Locator::Css { selector: "#name".into() }, &props).await.unwrap();
    assert_eq!(q.values.len(), 4, "{q:?}");
    assert_eq!(q.values["value"], "x");
    assert_eq!(q.values["visible"], true);
    assert_eq!(q.values["attr:aria-label"], "Name");
    assert_eq!(q.values["style:display"], "inline-block");
    assert!(q.r#ref.starts_with('e'));
    let link = g.query(&Locator::Text { text: "Help".into(), contains: false }, &[Prop::Text, Prop::Bbox, Prop::Style("color".into())]).await.unwrap();
    assert_eq!(link.values["text"], "Help");
    assert!(link.values["bbox"]["width"].as_f64().unwrap() > 0.0);
    assert_eq!(link.values["style:color"], "rgb(0, 0, 255)");
}

#[tokio::test]
async fn snapshots_truncate_with_an_omitted_count() {
    let (_runs, mut g) = launched("obs-7", "big.html").await;
    let s = g.snapshot(None, None, 400).await.unwrap();
    assert_eq!((s.nodes.len(), s.omitted), (400, 1600));
}

#[tokio::test]
async fn snapshots_do_not_change_the_behavior_log() {
    async fn run(addr: std::net::SocketAddr, name: &str, look: bool) -> String {
        let (_runs, mut g) = launched_at(addr, name, "form.html").await;
        g.eval_main("later()").await.unwrap();
        for _ in 0..3 {
            if look {
                g.snapshot(None, None, 400).await.unwrap();
                g.query(&Locator::Css { selector: "#name".into() }, &[Prop::Value]).await.unwrap();
            }
            g.advance(VirtualDuration::from_ms(60)).await.unwrap();
        }
        let path = g.trace_path();
        g.stop().await.unwrap();
        std::fs::read_to_string(path).unwrap()
    }
    let addr = support::serve();
    let plain = run(addr, "obs-8a", false).await;
    let looked = run(addr, "obs-8b", true).await;
    assert_eq!(frontcap_core::projection::compare_behavior(&plain, &looked), Ok(()));
}

#[tokio::test]
async fn diff_keeps_the_scope_and_ignores_truncation() {
    let (_runs, mut g) = launched("obs-9", "form.html").await;
    let dialog = Locator::Css { selector: "[role=dialog]".into() };
    let scoped = g.snapshot(Some(&dialog), None, 400).await.unwrap();
    let d = g.diff(scoped.observation_id, 400, true).await.unwrap();
    assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty(), "a scoped diff stays in its scope: {d:?}");
    let (_runs2, mut big) = launched("obs-10", "big.html").await;
    let s = big.snapshot(None, None, 400).await.unwrap();
    big.eval_main("document.documentElement.prepend(Object.assign(document.createElement('button'), {textContent: 'first'}))").await.unwrap();
    let d = big.diff(s.observation_id, 400, true).await.unwrap();
    assert_eq!(d.added.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["first"], "{d:?}");
    assert!(d.removed.is_empty(), "nothing left the page: {:?}", d.removed);
}

/// Criterion 12: an action that used a snapshot-issued reference re-executes from its resolution record
/// without the snapshot.
#[tokio::test]
async fn a_reference_re_executes_from_its_resolution_record() {
    let addr = support::serve();
    let (_a, mut first) = launched_at(addr, "obs-r1", "form.html").await;
    let s = first.snapshot(None, None, 400).await.unwrap();
    let save = ref_of(&s, "button", "Save");
    let Locator::Ref { id } = &save else { unreachable!() };
    let resolution = first.resolution_of(id).await.unwrap().expect("reachable");
    assert_eq!((resolution.document, resolution.path.as_slice(), resolution.role.as_deref(), resolution.name.as_deref()), (1, &[1.into(), 1.into()][..], Some("button"), Some("Save")));
    first.click(RequestId::new("c-1").unwrap(), &save).await.unwrap();
    first.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let recorded = first.trace_path();
    first.stop().await.unwrap();

    let (_b, mut second) = launched_at(addr, "obs-r2", "form.html").await;
    second.bind_ref(id, &resolution).await.unwrap();
    second.click(RequestId::new("c-1").unwrap(), &save).await.unwrap();
    assert_eq!(second.eval_main("window.__clicks").await.unwrap(), serde_json::json!(1));
    second.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let replayed = second.trace_path();
    second.stop().await.unwrap();
    let (a, b) = (std::fs::read_to_string(recorded).unwrap(), std::fs::read_to_string(replayed).unwrap());
    assert_eq!(frontcap_core::projection::compare_behavior(&a, &b), Ok(()));
}

#[tokio::test]
async fn a_bound_reference_to_a_missing_node_is_stale() {
    let (_runs, mut g) = launched("obs-r3", "form.html").await;
    let save = |path: Vec<u32>| frontcap_core::inputs::Resolution { document: 1, frames: vec![], path: path.into_iter().map(Into::into).collect(), role: Some("button".into()), name: Some("Save".into()), context: None };
    // The button is the body's second element; the first is the heading, which is not the recorded node.
    g.bind_ref("e9", &save(vec![1, 1])).await.unwrap();
    assert!(matches!(g.bind_ref("e9", &save(vec![1, 0])).await, Err(ChromiumError::StaleRef(_))));
    let gone = frontcap_core::inputs::Resolution { document: 1, frames: vec![], path: vec![1.into(), 40.into()], role: None, name: None, context: None };
    assert!(matches!(g.bind_ref("e9", &gone).await, Err(ChromiumError::StaleRef(_))));
    let other = frontcap_core::inputs::Resolution { document: 2, frames: vec![], path: vec![1.into(), 0.into()], role: None, name: None, context: None };
    assert!(matches!(g.bind_ref("e9", &other).await, Err(ChromiumError::StaleRef(_))));
}
