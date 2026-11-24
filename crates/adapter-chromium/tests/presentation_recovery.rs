//! A recovered controller reads the existing presentation, including an older pending image.
mod support;
use frontcap_chromium::{Generation, generation::ReconnectOptions};
use frontcap_core::{action::Action, ids::{RequestId, RunId}, predicate::{Locator, WatchRegistry}, rules::RuleSet, time::VirtualDuration};
use cdp::protocol::{emulation, page, target};
use frontcap_chromium::session::Session;

async fn native(g: &Generation) -> Session {
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}

fn pixel(path: &std::path::Path) -> [u8; 3] {
    let mut reader = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).unwrap())).read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    let at = (5 * info.width as usize + 5) * info.color_type.samples();
    [pixels[at], pixels[at + 1], pixels[at + 2]]
}

async fn recover(pending: bool) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("presentation recovery evidence (retained on failure): {}", runs.display());
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "presentation", format!("http://{addr}/presentation_recovery.html"), RuleSet::default());
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    assert!(g.screenshot().await.unwrap().current);
    if pending {
        g.act_input(RequestId::new("change").unwrap(), &Action::Click { target: Locator::Css { selector: "#change".into() } }).await.unwrap();
        // Stop between frame ticks: logical green, still-presented red, distinct virtual instants.
        g.advance(VirtualDuration::from_ms(5)).await.unwrap();
    }
    let before = g.screenshot().await.unwrap();
    let original_bytes = std::fs::read(&before.path).unwrap();
    let native = native(&g).await;
    let receipt = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
    assert!(receipt.available && receipt.current_document);
    assert_eq!(before.current, !pending);
    assert_eq!(pixel(&before.path), [255, 0, 0]);
    let now = g.now();
    if pending { assert!(before.t_us < now); }
    let endpoint = g.endpoint();
    let launch_id = g.launch_id().clone();
    let observations = g.observation_state();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (mut g, recovered) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(), runs_root: runs.clone(), run: RunId::new("presentation").unwrap(),
        launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(), upstream: None,
    }).await.unwrap();
    assert!(recovered.affected.is_empty());
    g.restore_observations(observations);
    let after = g.screenshot().await.expect("recover the existing surface without requesting a new frame");
    assert!(after.frame > before.frame);
    assert_ne!(after.path, before.path);
    assert_eq!(std::fs::read(&before.path).unwrap(), original_bytes);
    assert_eq!((after.current, after.t_us, after.document), (before.current, before.t_us, before.document));
    assert_eq!(pixel(&after.path), [255, 0, 0]);
    assert_eq!(g.now(), now);
    assert_eq!(native.call(&page::GetPresentedFrameParams::default()).await.unwrap(), receipt, "recovery and capture must preserve native activation identity");
    if pending { assert!(!after.exact, "recovery cannot invent the lost controller's change-to-frame evidence"); }
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    let progressed = g.screenshot().await.unwrap();
    assert!(progressed.current);
    assert_eq!(pixel(&progressed.path), if pending { [0, 255, 0] } else { [255, 0, 0] });
    native.client.close();
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}

#[tokio::test]
async fn an_idle_presentation_survives_controller_loss_without_a_new_frame() {
    recover(false).await;
}

#[tokio::test]
async fn a_pending_presentation_keeps_its_pixels_and_original_instant_after_recovery() {
    recover(true).await;
}

/// An engine effect can happen without reaching the adapter's render callback or
/// persisted observation state. The next controller must adopt that native surface.
#[tokio::test]
async fn a_native_presentation_and_replaced_surface_survive_without_controller_bookkeeping() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("native presentation evidence (retained on failure): {}", runs.display());
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "native-presentation", format!("http://{addr}/presentation_recovery.html"), RuleSet::default());
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let previous = g.screenshot().await.unwrap();
    assert_eq!(pixel(&previous.path), [255, 0, 0]);
    let previous_bytes = std::fs::read(&previous.path).unwrap();
    let observations = g.observation_state();
    let native = native(&g).await;
    let old = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
    native.eval("document.documentElement.style.background='rgb(0,255,0)'", None).await.unwrap();
    native.call(&emulation::SetDeviceMetricsOverrideParams::new(960, 640, 1.0, false)).await.unwrap();
    assert_eq!(native.eval("[innerWidth,innerHeight,performance.now()]", None).await.unwrap(), serde_json::json!([960, 640, 201]));
    assert!(native.call(&emulation::RunMainFrameParams::default()).await.unwrap().presented);
    let activated = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
    assert!(activated.available && activated.current_document);
    assert_ne!(activated.surface_id, old.surface_id);
    assert_ne!(activated.frame_token, old.frame_token);
    assert_eq!(activated.document_token, old.document_token);
    assert_eq!(activated.paint_time_ms, profile.initial_date_ms as f64 + 201.0);
    let endpoint = g.endpoint();
    let launch_id = g.launch_id().clone();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (mut g, recovered) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(), runs_root: runs.clone(), run: RunId::new("native-presentation").unwrap(),
        launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(), upstream: None,
    }).await.unwrap();
    assert!(recovered.affected.is_empty());
    g.restore_observations(observations);
    let shot = g.screenshot().await.unwrap();
    assert!(shot.frame > previous.frame);
    assert_ne!(shot.path, previous.path);
    assert_eq!(std::fs::read(&previous.path).unwrap(), previous_bytes, "old pixels must remain immutable evidence");
    assert!(shot.current && shot.exact);
    assert_eq!(shot.t_us.0, 201_000);
    assert_eq!(pixel(&shot.path), [0, 255, 0]);
    let reader = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&shot.path).unwrap())).read_info().unwrap();
    assert_eq!((reader.info().width, reader.info().height), (960, 640));
    assert_eq!(native.call(&page::GetPresentedFrameParams::default()).await.unwrap(), activated);
    native.client.close();
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}

#[tokio::test]
async fn recovery_never_attributes_the_previous_documents_surface_to_a_new_document() {
    for cross_origin in [false, true] {
        let addr = support::serve();
        let runs = tempfile::tempdir().unwrap().keep();
        eprintln!("document presentation evidence (retained on failure): {}", runs.display());
        let detached = support::DetachedProfile::new();
        let mut options = support::options(&runs, "document-presentation", format!("http://{addr}/presentation_recovery.html"), RuleSet::default());
        options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
        detached.apply(&mut options);
        let profile = options.profile.clone();
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(201)).await.unwrap();
        g.act_input(RequestId::new("change").unwrap(), &Action::Click { target: Locator::Css { selector: "#change".into() } }).await.unwrap();
        g.advance(VirtualDuration::from_ms(17)).await.unwrap();
        assert_eq!(pixel(&g.screenshot().await.unwrap().path), [0, 255, 0]);
        let native = native(&g).await;
        let old = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
        let host = if cross_origin { format!("localhost:{}", addr.port()) } else { addr.to_string() };
        g.navigate_action(&RequestId::new("navigate").unwrap(), &Action::Navigate {
            url: format!("http://{host}/presentation_recovery.html?new-document"),
        }).await.unwrap();
        let now = g.now();
        let observations = g.observation_state();
        let endpoint = g.endpoint();
        let launch_id = g.launch_id().clone();
        g.abandon();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (mut g, recovered) = Generation::reconnect(ReconnectOptions {
            repo_root: support::repo_root(), runs_root: runs.clone(), run: RunId::new("document-presentation").unwrap(),
            launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
            granted_until: None, dialog_rules: Default::default(), upstream: None,
        }).await.unwrap();
        assert!(recovered.affected.is_empty());
        g.restore_observations(observations);
        assert!(matches!(g.screenshot().await, Err(frontcap_chromium::ChromiumError::NoFrame | frontcap_chromium::ChromiumError::StaleFrame)));
        assert_eq!(g.now(), now);
        g.advance(VirtualDuration::from_ms(17)).await.unwrap();
        let shot = g.screenshot().await.unwrap();
        assert_eq!((pixel(&shot.path), shot.current, shot.document), ([255, 0, 0], true, 2));
        let new = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
        assert!(new.available && new.current_document);
        assert_ne!(new.document_token, old.document_token);
        native.client.close();
        g.stop().await.unwrap();
        std::fs::remove_dir_all(runs).unwrap();
    }
}

/// Allocating the next viewport's surface must not make capture wait for a new
/// render. Until that render is requested, the old activated surface is the image.
#[tokio::test]
async fn a_pending_viewport_change_copies_the_existing_activated_surface() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "pending-surface", format!("http://{addr}/presentation_recovery.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let before = g.screenshot().await.unwrap();
    let native = native(&g).await;
    let old = native.call(&page::GetPresentedFrameParams::default()).await.unwrap();
    native.call(&emulation::SetDeviceMetricsOverrideParams::new(960, 640, 1.0, false)).await.unwrap();
    assert_eq!(native.eval("[innerWidth,innerHeight,performance.now()]", None).await.unwrap(), serde_json::json!([960, 640, 201]));
    assert!(native.call(&emulation::IsMainFrameWantedParams::default()).await.unwrap().wanted);
    let after = g.screenshot().await.expect("copy the old activated surface while its replacement is pending");
    assert!(!after.current);
    assert_eq!(std::fs::read(&after.path).unwrap(), std::fs::read(&before.path).unwrap());
    assert_eq!(native.call(&page::GetPresentedFrameParams::default()).await.unwrap(), old);
    assert_eq!(g.now().0, 201_000);
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert!(g.screenshot().await.unwrap().current);
    native.client.close();
    g.stop().await.unwrap();
}
