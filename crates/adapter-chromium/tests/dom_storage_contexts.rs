//! Session namespace ownership and protection of already-read renderer caches.
mod support;
use cdp::protocol::{page, storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration};

async fn attach(client: &cdp::Client, id: target::TargetID) -> Session {
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(id)
    }).await.unwrap();
    Session::new(client.clone(), attached.session_id.0)
}

async fn key(session: &Session) -> storage::SerializedStorageKey {
    let frame = session.call(&page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame;
    session.call(&storage::GetStorageKeyForFrameParams::new(frame.id)).await.unwrap().storage_key
}

#[tokio::test]
async fn restoring_an_empty_popup_clone_keeps_its_read_source_unchanged() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://app.storage.test/start";
    let popup_url = "https://app.storage.test/popup";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><button id=open onclick=\"window.open('/popup','child')\">Open</button>"}},
        {"id":"popup","match":{"url":popup_url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>child</p>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "session-clone-restore", url.into(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("sessionStorage.length").await.unwrap(), serde_json::json!(0));
    g.click(RequestId::new("open").unwrap(), &Locator::Css { selector: "#open".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let source = attach(&client, target::TargetID(endpoint.target_id)).await;
    let infos = client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos;
    let popup_id = infos.into_iter().find(|info| info.url == popup_url).expect("enrolled popup target").target_id;
    let popup = attach(&client, popup_id).await;
    let at = g.now();
    popup.call(&storage::FrontcapRestoreDomStorageAreaParams::new(key(&popup).await, false,
        vec![storage::FrontcapDOMStorageEntry { key: cdp::Binary("eA==".into()), value: cdp::Binary("ANg=".into()) }]
    )).await.unwrap();
    assert_eq!(source.eval("sessionStorage.length", None).await.unwrap(), serde_json::json!(0));
    assert_eq!(popup.eval("sessionStorage.getItem('x').charCodeAt(0)", None).await.unwrap(), serde_json::json!(55296));
    let source_state = source.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    let popup_state = popup.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert_ne!(source_state.session_namespace, popup_state.session_namespace);
    assert!(source_state.session_storage.iter().all(|area| area.entries.is_empty()));
    assert!(popup_state.session_storage.iter().any(|area| !area.entries.is_empty()));
    let combined = frontcap_chromium::dom_storage::read_live_targets(
        &[("page", &source), ("popup-1", &popup)]).await.unwrap();
    let persisted: frontcap_core::dom_storage::DomStorageState =
        serde_json::from_slice(&serde_json::to_vec(&combined.storage).unwrap()).unwrap();
    assert_eq!(persisted.sessions.len(), 2);
    assert!(persisted.sessions.iter().find(|s| s.context == "page").unwrap().areas.iter().all(|a| a.entries.is_empty()));
    assert!(persisted.sessions.iter().find(|s| s.context == "popup-1").unwrap().areas.iter().any(|a| !a.entries.is_empty()));
    assert!(frontcap_chromium::dom_storage::read_live_targets(
        &[("page", &source), ("alias", &source)]).await.is_err());

    let owned = g.read_live_context_dom_storage().await.unwrap();
    assert_eq!(owned.storage, persisted);
    g.enter_context("popup-1").unwrap();
    assert_eq!(g.read_live_context_dom_storage().await.unwrap().storage, persisted);
    assert_eq!(g.eval_main("sessionStorage.getItem('x').charCodeAt(0)").await.unwrap(), serde_json::json!(55296));
    g.leave_context();
    assert_eq!(g.eval_main("sessionStorage.length").await.unwrap(), serde_json::json!(0));

    assert_eq!(g.now(), at);
    // A closed window's session storage leaves with it; it stays a context to reconstruct.
    popup.eval("window.close()", None).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert!(g.contexts().await.unwrap().iter().any(|context| context.id == "popup-1" && !context.open));
    let after_close = g.read_live_context_dom_storage().await.unwrap();
    assert_eq!(after_close.storage.sessions.iter().map(|s| s.context.as_str()).collect::<Vec<_>>(), ["page"]);
    let inputs = g.read_context_storage_inputs().await.unwrap();
    assert_eq!(inputs.contexts, [frontcap_core::profile::InitialContext {
        opener: "page".into(), url: "about:blank".into(), name: String::new(), no_opener: false, closed: true,
    }]);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn closed_windows_are_reconstructed_closed_and_keep_later_identities() {
    use frontcap_core::profile::InitialContext;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let origin = "https://closed.storage.test";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"pages","match":{"url":format!("{origin}/*")},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>window.started=true</script>"}}
    ]).to_string()).unwrap();
    let context = |opener: &str, path: &str, name: &str| InitialContext {
        opener: opener.into(), url: format!("{origin}/{path}"), name: name.into(), no_opener: false, closed: false,
    };
    let mut options = support::options(runs.path(), "closed-window-source", format!("{origin}/root"), rules.clone());
    options.profile.popups = PopupPolicy::Controlled;
    options.profile.initial_contexts = vec![context("page", "child", "child"), context("popup-1", "grandchild", "grandchild"), context("page", "sibling", "sibling")];
    let mut g = Generation::launch(options).await.unwrap();
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url).await.unwrap();
    for (id, value) in [("popup-2", "grandchild"), ("popup-3", "sibling")] {
        g.enter_context(id).unwrap();
        g.eval_main(&format!("sessionStorage.setItem('owner','{value}');true")).await.unwrap();
        g.leave_context();
    }
    g.enter_context("popup-1").unwrap();
    let child = g.endpoint().target_id;
    g.leave_context();
    client.call(None, &target::CloseTargetParams::new(target::TargetID(child))).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let listed = g.contexts().await.unwrap();
    assert!(listed.iter().any(|c| c.id == "popup-1" && !c.open), "{listed:?}");
    let live = g.read_live_context_dom_storage().await.unwrap();
    // Closing the window that opened it severed the grandchild's opener.
    assert!(live.windows.iter().find(|w| w.context == "popup-2").unwrap().opener_frame_id.is_none());
    let at = g.now();
    let inputs = g.read_context_storage_inputs().await.unwrap();
    assert_eq!(g.now(), at);
    assert_eq!(inputs.contexts, [
        InitialContext { opener: "page".into(), url: "about:blank".into(), name: String::new(), no_opener: false, closed: true },
        context("popup-1", "grandchild", "grandchild"),
        context("page", "sibling", "sibling"),
    ]);
    let mut restored_options = support::options(runs.path(), "closed-window-restored", inputs.url, rules);
    restored_options.profile.popups = PopupPolicy::Controlled;
    restored_options.profile.initial_contexts = inputs.contexts;
    restored_options.profile.initial_dom_storage = Some(inputs.storage);
    let mut restored = Generation::launch(restored_options).await.unwrap();
    let contexts = restored.contexts().await.unwrap();
    assert_eq!(contexts.iter().map(|c| (c.id.as_str(), c.open)).collect::<Vec<_>>(),
        [("page", true), ("popup-1", false), ("popup-2", true), ("popup-3", true)]);
    let roundtrip = restored.read_live_context_dom_storage().await.unwrap();
    assert_eq!(serde_json::to_value(&roundtrip.storage).unwrap(), serde_json::to_value(&live.storage).unwrap());
    assert!(roundtrip.windows.iter().find(|w| w.context == "popup-2").unwrap().opener_frame_id.is_none());
    let page_frame = &roundtrip.windows[0].frame_id;
    assert_eq!(roundtrip.windows.iter().find(|w| w.context == "popup-3").unwrap().opener_frame_id.as_ref(), Some(page_frame));
    for id in ["popup-2", "popup-3"] {
        restored.enter_context(id).unwrap();
        assert_eq!(restored.eval_main("typeof started").await.unwrap(), "undefined");
        restored.leave_context();
    }
    assert_eq!(restored.now().0, 0);
    restored.stop().await.unwrap();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn restore_refuses_empty_areas_after_the_renderer_has_read_them() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://app.storage.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>empty</p>"}}
    ]).to_string()).unwrap();
    let g = Generation::launch(support::options(runs.path(), "read-cache-refusal", url.into(), rules)).await.unwrap();
    assert_eq!(g.eval_main("[localStorage.length,sessionStorage.length]").await.unwrap(), serde_json::json!([0,0]));
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let session = attach(&client, target::TargetID(endpoint.target_id)).await;
    let storage_key = key(&session).await;
    let at = g.now();
    for (local, raw_key, raw_value) in [(true, "AXg=", "AADY"), (false, "eA==", "ANg=")] {
        let result = session.call(&storage::FrontcapRestoreDomStorageAreaParams::new(storage_key.clone(), local,
            vec![storage::FrontcapDOMStorageEntry { key: cdp::Binary(raw_key.into()), value: cdp::Binary(raw_value.into()) }]
        )).await;
        assert!(result.is_err(), "restoration must not invalidate an existing renderer cache");
    }
    let native = session.call(&storage::FrontcapReadDomStorageParams::default()).await.unwrap();
    assert!(native.local_storage.iter().all(|area| area.entries.is_empty()));
    assert!(native.session_storage.iter().all(|area| area.entries.is_empty()));
    assert_eq!(g.eval_main("[localStorage.length,sessionStorage.length]").await.unwrap(), serde_json::json!([0,0]));
    assert_eq!(g.now(), at);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn generation_capture_refuses_a_browser_page_outside_its_enrollment() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://app.storage.test/start";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><p>owned</p>"}}
    ]).to_string()).unwrap();
    let g = Generation::launch(support::options(runs.path(), "unenrolled-storage-page", url.into(), rules)).await.unwrap();
    let at = g.now();
    g.read_live_context_dom_storage().await.unwrap();
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url).await.unwrap();
    // A separate protocol client can create a page without going through the
    // generation's popup ownership. Browser state, not just the book, must win.
    let extra = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap();
    let error = g.read_live_context_dom_storage().await.unwrap_err();
    assert!(error.to_string().contains("do not match enrolled page contexts"), "{error}");
    assert_eq!(g.now(), at);
    client.call(None, &target::CloseTargetParams::new(extra.target_id)).await.unwrap();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn frozen_popup_construction_restores_distinct_sessions_before_scripts() {
    use frontcap_core::{dom_storage::*, action::Action};
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://app.storage.test/start";
    let popup_url = "https://app.storage.test/child";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>window.first=sessionStorage.getItem('owner')</script>"}},
        {"id":"child","match":{"url":popup_url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>window.first=[name,!!opener,sessionStorage.getItem('owner')]</script>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "frozen-popup-construction", url.into(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined");
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let source = attach(&client, target::TargetID(endpoint.target_id.clone())).await;
    let before = client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos.len();
    for invalid in [
        target::CreateTargetParams {
            frontcap_window_name: Some("missing-owner".into()),
            ..target::CreateTargetParams::new("about:blank".into())
        },
        target::CreateTargetParams {
            frontcap_opener_target_id: Some(target::TargetID("missing-owner".into())),
            ..target::CreateTargetParams::new("about:blank".into())
        },
        target::CreateTargetParams {
            frontcap_opener_target_id: Some(target::TargetID(endpoint.target_id.clone())),
            ..target::CreateTargetParams::new(popup_url.into())
        },
    ] {
        assert!(client.call(None, &invalid).await.is_err());
    }
    assert_eq!(client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos.len(), before);
    let created = client.call(None, &target::CreateTargetParams {
        frontcap_opener_target_id: Some(target::TargetID(endpoint.target_id)),
        frontcap_window_name: Some("restored-child".into()),
        ..target::CreateTargetParams::new("about:blank".into())
    }).await.unwrap();
    assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined", "source script ran while constructing popup");
    let popup_context = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let contexts = g.contexts().await.unwrap();
            if let Some(context) = contexts.iter().find(|context| context.opener.as_deref() == Some("page")) {
                break context.id.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("browser-created popup was not enrolled");
    g.enter_context(&popup_context).unwrap();
    assert_eq!(g.endpoint().target_id, created.target_id.0);
    g.navigate_action(&RequestId::new("construct-child").unwrap(), &Action::Navigate { url: popup_url.into() }).await.unwrap();
    assert_eq!(source.eval("typeof first", None).await.unwrap(), "undefined", "source script ran while navigating popup");
    assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined");
    let popup = attach(&client, target::TargetID(g.endpoint().target_id)).await;
    let state = |context: &str, value: &str| SessionStorageState {
        context: context.into(),
        areas: vec![DomStorageArea {
            origin: "https://app.storage.test".into(), partition: None,
            entries: vec![DomStorageEntry {
                key: b"owner".to_vec(),
                value: value.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            }],
        }],
    };
    // All empty namespaces exist before any source is seeded, avoiding
    // accidental copying of source values into the reconstructed child.
    frontcap_chromium::dom_storage::restore_target(&source,
        DomStorageEncoding::ChromiumLittleEndian, &[], &state("page", "parent")).await.unwrap();
    frontcap_chromium::dom_storage::restore_target(&popup,
        DomStorageEncoding::ChromiumLittleEndian, &[], &state(&popup_context, "child")).await.unwrap();
    // Construction's protocol client does not own the popup's lifetime.
    client.close();
    g.leave_context();
    assert_eq!(g.now().0, 0);
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("first").await.unwrap(), "parent");
    g.enter_context(&popup_context).unwrap();
    assert_eq!(g.eval_main("first").await.unwrap(), serde_json::json!(["restored-child",true,"child"]));
    assert_eq!(g.eval_main("opener.first").await.unwrap(), "parent");
    g.leave_context();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn construction_profile_restores_nested_and_noopener_contexts_before_scripts() {
    use frontcap_core::{dom_storage::*, profile::InitialContext};
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let origin = "https://app.storage.test";
    let pages: Vec<_> = ["root", "child", "grandchild", "separate"].into_iter().map(|name| {
        serde_json::json!({"id":name,"match":{"url":format!("{origin}/{name}")},
            "action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
                "body":"<!doctype html><script>window.first=[name,!!opener,sessionStorage.getItem('owner')]</script>"}})
    }).collect();
    let rules = RuleSet::from_json(&serde_json::to_string(&pages).unwrap()).unwrap();
    let mut options = support::options(runs.path(), "context-profile-restoration", format!("{origin}/root"), rules);
    options.profile.popups = PopupPolicy::Controlled;
    options.profile.initial_contexts = vec![
        InitialContext { opener: "page".into(), url: format!("{origin}/child"), name: "child".into(), no_opener: false, closed: false },
        InitialContext { opener: "popup-1".into(), url: format!("{origin}/grandchild"), name: "grandchild".into(), no_opener: false, closed: false },
        InitialContext { opener: "page".into(), url: format!("{origin}/separate"), name: "separate".into(), no_opener: true, closed: false },
    ];
    options.profile.initial_dom_storage = Some(DomStorageState {
        encoding: DomStorageEncoding::ChromiumLittleEndian, local: vec![],
        // Serialization order must not determine the namespace owner.
        sessions: ["popup-3", "popup-2", "popup-1", "page"].into_iter().map(|context| SessionStorageState {
            context: context.into(),
            areas: vec![DomStorageArea { origin: origin.into(), partition: None,
                entries: vec![DomStorageEntry { key: b"owner".to_vec(),
                    value: context.encode_utf16().flat_map(u16::to_le_bytes).collect() }] }],
        }).collect(),
    });
    options.profile = serde_json::from_slice(&serde_json::to_vec(&options.profile).unwrap()).unwrap();
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(g.now().0, 0);
    let contexts = g.contexts().await.unwrap();
    assert_eq!(contexts.len(), 4);
    assert_eq!(contexts[2].opener.as_deref(), Some("popup-1"));
    assert!(contexts[3].no_opener);
    for context in ["page", "popup-1", "popup-2", "popup-3"] {
        g.enter_context(context).unwrap();
        assert_eq!(g.eval_main("typeof first").await.unwrap(), "undefined");
    }
    g.leave_context();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("first").await.unwrap(), serde_json::json!(["",false,"page"]));
    for (context, name, opener) in [
        ("popup-1", "child", true), ("popup-2", "grandchild", true), ("popup-3", "separate", false),
    ] {
        g.enter_context(context).unwrap();
        assert_eq!(g.eval_main("first").await.unwrap(), serde_json::json!([name,opener,context]));
    }
    g.leave_context();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn native_metadata_tracks_current_names_fragments_and_severed_openers() {
    use frontcap_core::profile::InitialContext;
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let origin = "https://metadata.storage.test";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"pages","match":{"url":format!("{origin}/*")},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>window.started=true</script>"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "native-window-metadata", format!("{origin}/root"), rules.clone());
    options.profile.popups = PopupPolicy::Controlled;
    options.profile.initial_contexts = vec![InitialContext {
        opener: "page".into(), url: format!("{origin}/child"), name: "declared".into(), no_opener: false, closed: false,
    }];
    let mut g = Generation::launch(options).await.unwrap();
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url).await.unwrap();
    let root = attach(&client, target::TargetID(g.endpoint().target_id)).await;
    g.eval_main("window.name='current-root';sessionStorage.setItem('owner','root');localStorage.setItem('shared','retained');history.replaceState(null,'','#current');true").await.unwrap();
    let top = frontcap_chromium::dom_storage::read_target(&root, "page").await.unwrap();
    assert_eq!(top.window_name, "current-root");
    assert_eq!(top.url, format!("{origin}/root#current"));
    assert!(top.opener_frame_id.is_none());
    assert_eq!(g.eval_main("typeof started").await.unwrap(), "undefined");
    g.enter_context("popup-1").unwrap();
    let popup = attach(&client, target::TargetID(g.endpoint().target_id)).await;
    let before = frontcap_chromium::dom_storage::read_target(&popup, "popup-1").await.unwrap();
    assert_eq!(before.window_name, "declared");
    assert_eq!(before.opener_frame_id.as_deref(), Some(top.frame_id.as_str()));
    g.eval_main("window.name='current-child';sessionStorage.setItem('owner','child');window.opener=null;history.replaceState(null,'','#changed');true").await.unwrap();
    let after = frontcap_chromium::dom_storage::read_target(&popup, "popup-1").await.unwrap();
    assert_eq!(after.window_name, "current-child");
    assert_eq!(after.url, format!("{origin}/child#changed"));
    assert!(after.opener_frame_id.is_none());
    assert_eq!(after.frame_id, before.frame_id);
    let live = g.read_live_context_dom_storage().await.unwrap();
    let root_window = live.windows.iter().find(|window| window.context == "page").unwrap();
    let child_window = live.windows.iter().find(|window| window.context == "popup-1").unwrap();
    assert_eq!(root_window.name, "current-root");
    assert_eq!(root_window.url, format!("{origin}/root#current"));
    assert_eq!(child_window.name, "current-child");
    assert_eq!(child_window.url, format!("{origin}/child#changed"));
    assert!(child_window.opener_frame_id.is_none());
    assert_eq!(live.storage.sessions.len(), 2);
    assert_eq!(g.eval_main("typeof started").await.unwrap(), "undefined");
    assert_eq!(g.now().0, 0);
    let inputs = g.read_context_storage_inputs().await.unwrap();
    assert!(inputs.contexts[0].no_opener);
    assert_eq!(inputs.contexts[0].opener, "page");
    let mut restored_options = support::options(runs.path(), "restored-window-metadata", inputs.url, rules);
    restored_options.profile.popups = PopupPolicy::Controlled;
    restored_options.profile.initial_window_name = Some(inputs.window_name);
    restored_options.profile.initial_contexts = inputs.contexts;
    restored_options.profile.initial_dom_storage = Some(inputs.storage);
    let mut restored = Generation::launch(restored_options).await.unwrap();
    let roundtrip = restored.read_live_context_dom_storage().await.unwrap();
    assert_eq!(serde_json::to_value(&roundtrip.storage).unwrap(), serde_json::to_value(&live.storage).unwrap());
    for (expected, actual) in live.windows.iter().zip(&roundtrip.windows) {
        assert_eq!(actual.context, expected.context);
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.url, expected.url);
        assert!(actual.opener_frame_id.is_none());
    }
    assert_eq!(roundtrip.windows.len(), live.windows.len());
    assert_eq!(restored.eval_main("typeof started").await.unwrap(), "undefined");
    restored.enter_context("popup-1").unwrap();
    assert_eq!(restored.eval_main("typeof started").await.unwrap(), "undefined");
    assert_eq!(restored.now().0, 0);
    restored.stop().await.unwrap();
    g.leave_context();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_window_that_closes_right_after_writing_leaves_its_writes_certified() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let url = "https://closing.storage.test/start";
    // Large writes take the storage service longest to answer; the window closes in the same task.
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"top","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><button id=open onclick=\"window.open('/popup','child')\">Open</button>"}},
        {"id":"popup","match":{"url":"https://closing.storage.test/popup"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>const big='x'.repeat(2*1024*1024);sessionStorage.setItem('big',big);localStorage.setItem('big',big);localStorage.setItem('done','yes');close()</script>"}}
    ]).to_string()).unwrap();
    for attempt in 0..8 {
        let mut options = support::options(runs.path(), &format!("closing-writer-{attempt}"), url.into(), rules.clone());
        options.profile.popups = PopupPolicy::Controlled;
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(10)).await.unwrap();
        g.click(RequestId::new("open").unwrap(), &Locator::Css { selector: "#open".into() }).await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        assert!(g.contexts().await.unwrap().iter().any(|c| c.id == "popup-1" && !c.open), "attempt {attempt}");
        let live = g.read_live_context_dom_storage().await.unwrap_or_else(|e| panic!("attempt {attempt}: {e}"));
        let local: Vec<_> = live.storage.local.iter().flat_map(|a| &a.entries).map(|e| e.key.clone()).collect();
        assert!(local.contains(&b"\x01done".to_vec()), "attempt {attempt}: {local:?}");
        g.stop().await.unwrap();
    }
}
