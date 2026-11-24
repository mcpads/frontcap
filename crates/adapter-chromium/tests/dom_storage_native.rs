//! Native DOM storage capture/restore, including strings JSON cannot preserve.
mod support;
use cdp::protocol::{page, storage, system_info, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::rules::RuleSet;

async fn native(g: &Generation) -> Session {
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}

async fn launch(runs: &std::path::Path, name: &str) -> Generation {
    let url = "https://raw.storage.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":url},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><body>storage</body>"}}
    ]).to_string()).unwrap();
    Generation::launch(support::options(runs, name, url.into(), rules)).await.unwrap()
}

fn entry(key: &str, value: &str) -> storage::FrontcapDOMStorageEntry {
    storage::FrontcapDOMStorageEntry { key: cdp::Binary(key.into()), value: cdp::Binary(value.into()) }
}

#[tokio::test]
async fn raw_restore_preserves_unpaired_surrogates_and_refuses_overwrite() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let g = launch(runs.path(), "raw-storage-restore").await;
    let session = native(&g).await;
    let frame = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame;
    let key = session.call(&storage::GetStorageKeyForFrameParams::new(frame.id)).await.unwrap().storage_key;
    let at = g.now();
    // Native local: Latin-1 key x, UTF-16 value U+D800. Session: UTF-8 key x,
    // untagged UTF-16 value U+D800. No UTF-8 or JSON string round-trip is allowed.
    let local = entry("AXg=", "AADY");
    let session_value = entry("eA==", "ANg=");
    for (is_local, value) in [(true, local.clone()), (false, session_value.clone())] {
        session.call(&storage::FrontcapRestoreDomStorageAreaParams::new(
            key.clone(), is_local, vec![value],
        )).await.unwrap();
    }
    let rejected = session.call(&storage::FrontcapRestoreDomStorageAreaParams::new(
        key.clone(), true, vec![entry("AXg=", "AWNoYW5nZWQ=")],
    )).await;
    assert!(rejected.is_err(), "existing values must not be overwritten");
    let captured = session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert!(captured.session_namespace.is_some());
    assert_eq!(captured.local_storage.iter().find(|a| a.storage_key == key).unwrap().entries, vec![local]);
    assert_eq!(captured.session_storage.iter().find(|a| a.storage_key == key).unwrap().entries, vec![session_value]);
    assert_eq!(g.eval_main("[localStorage.getItem('x').charCodeAt(0),sessionStorage.getItem('x').charCodeAt(0)]").await.unwrap(), serde_json::json!([55296,55296]));
    session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert_eq!(g.now(), at, "native restore/capture must not advance virtual time");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn capture_includes_writes_from_a_removed_frame() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let mut g = launch(runs.path(), "removed-frame-native-writes").await;
    g.advance(frontcap_core::time::VirtualDuration(1_000)).await.unwrap();
    let session = native(&g).await;
    let at = g.now();
    g.eval_main("(()=>{const f=document.createElement('iframe');document.body.append(f);f.contentWindow.localStorage.setItem('kept','kept');f.contentWindow.sessionStorage.setItem('kept','kept');f.remove();return true})()").await.unwrap();
    let captured = session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert!(captured.local_storage.iter().any(|a| a.entries.contains(&entry("AWtlcHQ=", "AWtlcHQ="))));
    assert!(captured.session_storage.iter().any(|a| a.entries.contains(&entry("a2VwdA==", "awBlAHAAdAA="))));
    let model = frontcap_chromium::dom_storage::read_target(&session, "page").await.unwrap();
    assert_eq!(model.session.context, "page");
    assert!(model.has_session_namespace);
    assert!(model.local.iter().any(|a| a.entries.iter().any(|e| e.key == b"\x01kept" && e.value == b"\x01kept")));
    assert!(model.session.areas.iter().any(|a| a.entries.iter().any(|e| e.key == b"kept" && e.value == b"k\0e\0p\0t\0")));

    assert_eq!(g.now(), at);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn duplicate_raw_keys_are_rejected_without_partial_restore() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let g = launch(runs.path(), "duplicate-native-storage-keys").await;
    let session = native(&g).await;
    let frame = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame;
    let key = session.call(&storage::GetStorageKeyForFrameParams::new(frame.id)).await.unwrap().storage_key;
    for is_local in [true, false] {
        assert!(session.call(&storage::FrontcapRestoreDomStorageAreaParams::new(
            key.clone(), is_local, vec![entry("AXg=", "AQ=="), entry("AXg=", "Ag==")],
        )).await.is_err());
    }
    let captured = session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert!(captured.local_storage.iter().all(|a| a.entries.is_empty()));
    assert!(captured.session_storage.iter().all(|a| a.entries.is_empty()));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn adapter_restore_checks_scope_aliases_before_writing_and_verifies_bytes() {
    use frontcap_core::{dom_storage::{DomStorageArea, DomStorageEntry, DomStorageEncoding, SessionStorageState}, storage::StoragePartition};
    let runs = tempfile::tempdir().unwrap();
    if !support::controlled_engine_or_check_baseline().await { return; }
    let g = launch(runs.path(), "adapter-native-restore").await;
    let session = native(&g).await;
    let at = g.now();
    let local = DomStorageArea { origin: "https://raw.storage.test".into(), partition: None,
        entries: vec![DomStorageEntry { key: vec![1, b'x'], value: vec![0, 0, 0xd8] }] };
    let mut alias = local.clone();
    alias.partition = Some(StoragePartition { top_level_site: "https://storage.test".into(), has_cross_site_ancestor: Some(false) });
    let session_state = SessionStorageState { context: "page".into(), areas: vec![] };
    let encoding = DomStorageEncoding::ChromiumLittleEndian;
    assert!(frontcap_chromium::dom_storage::restore_target(&session, encoding, &[local.clone(), alias], &session_state).await.is_err());
    let untouched = frontcap_chromium::dom_storage::read_target(&session, "page").await.unwrap();
    assert!(untouched.local.iter().all(|a| a.entries.is_empty()));
    let restored = frontcap_chromium::dom_storage::restore_target(&session, encoding, std::slice::from_ref(&local), &session_state).await.unwrap();
    assert!(restored.local.iter().any(|a| a.entries == local.entries));
    assert_eq!(g.eval_main("localStorage.getItem('x').charCodeAt(0)").await.unwrap(), serde_json::json!(55296));
    assert_eq!(g.now(), at);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn removed_other_origin_frame_storage_roundtrips_into_a_fresh_generation() {
    use frontcap_core::{dom_storage::DomStorageEncoding, time::VirtualDuration};
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://top.storage.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><body><script>window.report='empty';addEventListener('message',e=>{if(e.origin!=='https://child.storage.test')return;document.querySelectorAll('iframe').forEach(f=>f.remove());window.report=e.data})</script>"}},
        {"id":"seed","match":{"url":"https://child.storage.test/seed"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>localStorage.setItem('kept','kept');sessionStorage.setItem('kept','kept');parent.postMessage('seeded','*')</script>"}},
        {"id":"probe","match":{"url":"https://child.storage.test/probe"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>parent.postMessage(String(localStorage.getItem('kept'))+':'+String(sessionStorage.getItem('kept')),'*')</script>"}}
    ]).to_string()).unwrap();
    let mut source = Generation::launch(support::options(runs.path(), "other-origin-source", url.into(), rules.clone())).await.unwrap();
    source.advance(VirtualDuration(1_000)).await.unwrap();
    source.eval_main("(()=>{const f=document.createElement('iframe');f.src='https://child.storage.test/seed';document.body.append(f);return true})()").await.unwrap();
    source.advance(VirtualDuration(50_000)).await.unwrap();
    assert_eq!(source.eval_main("window.report").await.unwrap(), serde_json::json!("seeded"));
    assert_eq!(source.eval_main("document.querySelectorAll('iframe').length").await.unwrap(), serde_json::json!(0));
    let source_native = native(&source).await;
    let at = source.now();
    let captured = frontcap_chromium::dom_storage::read_target(&source_native, "page").await.unwrap();
    assert_eq!(source.now(), at);
    assert!(captured.local.iter().any(|a| a.origin == "https://child.storage.test" && !a.entries.is_empty()));
    assert!(captured.session.areas.iter().any(|a| a.origin == "https://child.storage.test" && !a.entries.is_empty()));

    let mut options = support::options(runs.path(), "other-origin-restored", url.into(), rules);
    options.profile.initial_dom_storage = Some(frontcap_core::dom_storage::DomStorageState {
        encoding: DomStorageEncoding::ChromiumLittleEndian,
        local: captured.local,
        sessions: vec![captured.session],
    });
    // Exercise the persisted construction profile, not a post-launch injection.
    options.profile = serde_json::from_slice(&serde_json::to_vec(&options.profile).unwrap()).unwrap();
    let mut child = Generation::launch(options).await.unwrap();
    assert_eq!(child.now().0, 0);
    child.advance(VirtualDuration(1_000)).await.unwrap();
    child.eval_main("(()=>{const f=document.createElement('iframe');f.src='https://child.storage.test/probe';document.body.append(f);return true})()").await.unwrap();
    child.advance(VirtualDuration(50_000)).await.unwrap();
    assert_eq!(child.eval_main("window.report").await.unwrap(), serde_json::json!("kept:kept"));
    child.stop().await.unwrap();
    source.stop().await.unwrap();
}

#[tokio::test]
async fn initial_native_storage_is_visible_to_the_first_application_script() {
    use frontcap_core::dom_storage::*;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://raw.storage.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>window.first=[localStorage.getItem('x').charCodeAt(0),sessionStorage.getItem('x').charCodeAt(0)]</script>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "initial-native-storage", url.into(), rules);
    let area = |key, value| DomStorageArea {
        origin: "https://raw.storage.test".into(), partition: None,
        entries: vec![DomStorageEntry { key, value }],
    };
    options.profile.initial_dom_storage = Some(DomStorageState {
        encoding: DomStorageEncoding::ChromiumLittleEndian,
        local: vec![area(vec![1, b'x'], vec![0, 0, 0xd8])],
        sessions: vec![SessionStorageState { context: "page".into(),
            areas: vec![area(vec![b'x'], vec![0, 0xd8])] }],
    });
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(g.now().0, 0);
    assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined");
    g.advance(frontcap_core::time::VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("first").await.unwrap(), serde_json::json!([55296, 55296]));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_initial_storage_ownership_is_rejected_before_browser_setup() {
    use frontcap_core::dom_storage::*;
    let runs = tempfile::tempdir().unwrap();
    for legacy in [false, true] {
        let mut options = support::options(runs.path(), "invalid-native-storage",
            "https://raw.storage.test/start".into(), RuleSet::default());
        // If validation reaches engine discovery it cannot succeed here.
        options.repo_root = runs.path().join("no-engine");
        options.profile.initial_dom_storage = Some(DomStorageState {
            encoding: DomStorageEncoding::ChromiumLittleEndian, local: vec![],
            sessions: vec![SessionStorageState {
                context: if legacy { "page" } else { "popup-1" }.into(), areas: vec![],
            }],
        });
        if legacy {
            options.profile.initial_storage = Some(frontcap_core::intervention::InitialStorage {
                origin: "https://raw.storage.test".into(), ..Default::default()
            });
        }
        let error = match Generation::launch(options).await {
            Ok(g) => { g.stop().await.unwrap(); panic!("ambiguous initial storage must fail"); }
            Err(error) => error,
        };
        let expected = if legacy { "cannot both be supplied" } else { "require context topology restoration" };
        assert!(error.to_string().contains(expected), "{error}");
    }
}

async fn renderers(client: &cdp::Client) -> std::collections::BTreeSet<i64> {
    client.call(None, &system_info::GetProcessInfoParams::default()).await.unwrap()
        .process_info.into_iter().filter(|p| p.r#type == "renderer").map(|p| p.id).collect()
}

async fn gone(client: &cdp::Client, writers: &std::collections::BTreeSet<i64>) {
    for _ in 0..100 {
        if renderers(client).await.is_disjoint(writers) { return; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the writer's renderer {writers:?} did not leave");
}

#[tokio::test]
async fn capture_includes_writes_of_a_renderer_that_has_left() {
    use frontcap_core::time::VirtualDuration;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://retired.storage.test/start";
    // The writes go out just before a navigation to an error page, whose document
    // needs another renderer, and from the unload handler (which also keeps the page
    // out of the back-forward cache): the writer's process leaves right after them.
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>addEventListener('unload',()=>localStorage.setItem('unload','unload'));setTimeout(()=>{localStorage.setItem('kept','kept');sessionStorage.setItem('kept','kept');location.href='http://127.0.0.1:1/x'},10)</script>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "retired-renderer-writes", url.into(), rules);
    options.passthrough_origins = Some(vec!["http://127.0.0.1:1".into()]);
    let mut g = Generation::launch(options).await.unwrap();
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url).await.unwrap();
    let before = renderers(&client).await;
    g.advance(VirtualDuration::from_ms(1_000)).await.unwrap();
    let session = native(&g).await;
    let frame = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame;
    assert_eq!(frame.url, "chrome-error://chromewebdata/");
    // The process leaves by the real clock once its last document is gone.
    gone(&client, &before).await;
    let at = g.now();
    let captured = frontcap_chromium::dom_storage::read_target(&session, "page").await.unwrap();
    assert!(captured.local.iter().any(|a| a.origin == "https://retired.storage.test"
        && a.entries.iter().any(|e| e.key == b"\x01kept" && e.value == b"\x01kept")
        && a.entries.iter().any(|e| e.key == b"\x01unload" && e.value == b"\x01unload")), "{:?}", captured.local);
    assert!(captured.session.areas.iter().any(|a| a.origin == "https://retired.storage.test"
        && a.entries.iter().any(|e| e.key == b"kept" && e.value == b"k\0e\0p\0t\0")), "{:?}", captured.session.areas);
    assert_eq!(g.now(), at);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_renderer_that_ends_unconfirmed_leaves_capture_refused() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let g = launch(runs.path(), "crashed-renderer-writes").await;
    g.eval_main("localStorage.setItem('kept','kept')").await.unwrap();
    let session = native(&g).await;
    session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    let writers = renderers(&session.client).await;
    // A crash gives the renderer no chance to confirm what it wrote: the backend may
    // lack writes the page saw succeed, so no later capture can vouch for it.
    let _ = session.call(&page::CrashParams::default()).await;
    gone(&session.client, &writers).await;
    let other = session.client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap();
    let attached = session.client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(other.target_id)
    }).await.unwrap();
    let reader = Session::new(session.client.clone(), attached.session_id.0);
    let error = reader.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap_err();
    assert!(error.to_string().contains("ended before confirming its DOM storage writes"), "{error}");
    let _ = g.stop().await;
}
