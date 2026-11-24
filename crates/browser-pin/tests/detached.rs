use std::time::Duration;

#[test]
fn detached_browser_outlives_its_handle_and_uses_the_given_profile() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let pin = browser_pin::load_pin(&root).unwrap();
    let binary = browser_pin::install(&root, &pin, browser_pin::current_platform().unwrap()).unwrap();
    let profile = tempfile::tempdir().unwrap();
    let d = browser_pin::launch_detached(&binary, &[], profile.path(), Duration::from_secs(30)).unwrap();
    assert!(profile.path().join("DevToolsActivePort").exists());
    let id = frontcap_core::process::ProcessIdentity::of(d.pid).unwrap().unwrap();
    drop(d);
    std::thread::sleep(Duration::from_millis(300));
    assert!(id.is_alive(), "the browser must survive its launcher's handle");
    assert!(id.kill().unwrap());
}
