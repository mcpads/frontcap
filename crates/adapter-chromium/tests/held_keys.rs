//! E06: held keys and an input method's composition outlive the session that began them.
mod support;

use frontcap_chromium::Generation;
use frontcap_chromium::generation::ReconnectOptions;
use frontcap_core::action::Action;
use frontcap_core::ids::{RequestId, RunId};
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

#[tokio::test]
async fn held_keys_and_a_composition_survive_reconnection() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut o = support::options(runs.path(), "keys-rc", format!("http://{}/keys.html", support::serve()), RuleSet::default());
    profile_dir.apply(&mut o);
    let profile = o.profile.clone();
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    g.act_input(RequestId::new("focus").unwrap(), &Action::Click { target: Locator::Css { selector: "#field".into() } }).await.unwrap();
    g.act_input(RequestId::new("compose").unwrap(), &Action::Compose { text: "서".into() }).await.unwrap();
    assert_eq!(g.pointer_state().await.unwrap().composition.as_deref(), Some("서"));
    let (endpoint, launch) = (g.endpoint(), g.launch_id().clone());
    g.abandon();
    let options = ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: RunId::new("keys-rc").unwrap(),
        launch_id: launch,
        profile,
        endpoint,
        watches: Default::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    };
    let (mut g, _) = Generation::reconnect(options).await.unwrap();
    assert!(g.may_compose());
    assert_eq!(g.pointer_state().await.unwrap().composition.as_deref(), Some("서"), "the composition is read from the page after reconnection");
    g.act_input(RequestId::new("commit").unwrap(), &Action::Commit { key: None }).await.unwrap();
    g.act_input(RequestId::new("shift").unwrap(), &Action::KeyDown { key: "Shift".into() }).await.unwrap();
    let (endpoint, launch) = (g.endpoint(), g.launch_id().clone());
    g.abandon();
    let options = ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: RunId::new("keys-rc").unwrap(),
        launch_id: launch,
        profile: support::options(runs.path(), "keys-rc", String::new(), RuleSet::default()).profile,
        endpoint,
        watches: Default::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    };
    let (mut g, _) = Generation::reconnect(options).await.unwrap();
    assert_eq!(g.held_keys(), ["Shift"]);
    g.act_input(RequestId::new("click").unwrap(), &Action::Click { target: Locator::Css { selector: "#b".into() } }).await.unwrap();
    g.act_input(RequestId::new("unshift").unwrap(), &Action::KeyUp { key: "Shift".into() }).await.unwrap();
    let seen = g.eval_main("seen.filter(e => e[0] !== 'keydown' || e[1] === 'Shift')").await.unwrap();
    assert_eq!(seen, serde_json::json!([
        ["click", null, false], ["compositionstart", "", null], ["compositionupdate", "서", null],
        // A commit updates the composition to the committed text, then ends it.
        ["compositionupdate", "서", null], ["compositionend", "서", null],
        ["keydown", "Shift", true], ["click", null, true], ["keyup", "Shift", false]
    ]));
    g.stop().await.unwrap();
}
