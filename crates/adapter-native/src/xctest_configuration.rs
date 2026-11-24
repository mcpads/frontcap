//! Validate the declared UIKit product identities before acquiring a Simulator.
use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Test {
    #[serde(rename = "IsUITestBundle")]
    is_ui_test_bundle: bool,
    test_host_bundle_identifier: String,
    test_bundle_path: String,
    test_host_path: String,
    #[serde(rename = "UITargetAppPath")]
    ui_target_app_path: String,
}

fn derived(config: &RunnerLaunch, directory: &Path) -> io::Result<Vec<u8>> {
    let products = directory.join("products");
    let mut document: BTreeMap<String, plist::Value> =
        plist::from_file(products.join(&config.manifest.xctestrun)).map_err(io::Error::other)?;
    let version = document
        .get("__xctestrun_metadata__")
        .and_then(plist::Value::as_dictionary)
        .and_then(|d| d.get("FormatVersion"))
        .and_then(plist::Value::as_unsigned_integer);
    if version != Some(1) || document.len() != 2 {
        return Err(io::Error::other(
            "initial data requires a single-target format-1 XCTest configuration",
        ));
    }
    let value = document
        .remove(&config.manifest.names.tests)
        .ok_or_else(|| io::Error::other("configured XCTest target absent"))?;
    let mut bytes = Vec::new();
    value.to_writer_xml(&mut bytes).map_err(io::Error::other)?;
    let test: Test = plist::from_bytes(&bytes).map_err(io::Error::other)?;
    if !test.is_ui_test_bundle
        || test.test_host_bundle_identifier != config.app.runner_bundle
        || test.test_bundle_path
            != format!(
                "__TESTHOST__/PlugIns/{}.xctest",
                config.manifest.names.tests
            )
        || test.test_host_path
            != format!(
                "__TESTROOT__/Debug-iphonesimulator/{}-Runner.app",
                config.manifest.names.tests
            )
        || test.ui_target_app_path
            != format!(
                "__TESTROOT__/Debug-iphonesimulator/{}.app",
                config.manifest.names.app
            )
    {
        return Err(io::Error::other(
            "XCTest destination products differ from configured identities",
        ));
    }
    let mut settings = value.into_dictionary().ok_or_else(|| io::Error::other("invalid XCTest settings"))?;
    let bootstrap = directory.join("bootstrap.app");
    settings.insert("UITargetAppPath".into(), plist::Value::String(bootstrap.to_string_lossy().into_owned()));
    if let Some(dependencies) = settings.get_mut("DependentProductPaths") {
        let paths = dependencies.as_array_mut().ok_or_else(|| io::Error::other("invalid XCTest product dependencies"))?;
        for path in paths {
            let name = path.as_string().ok_or_else(|| io::Error::other("invalid XCTest product dependency"))?;
            if name == test.ui_target_app_path { *path = plist::Value::String(bootstrap.to_string_lossy().into_owned()); }
        }
    }
    document.insert(config.manifest.names.tests.clone(), plist::Value::Dictionary(settings));
    fn expand(value: &mut plist::Value, root: &str) {
        match value {
            plist::Value::String(s) => *s = s.replace("__TESTROOT__", root),
            plist::Value::Array(a) => for v in a { expand(v, root); },
            plist::Value::Dictionary(d) => for (_, v) in d { expand(v, root); },
            _ => (),
        }
    }
    let root = products.to_str().ok_or_else(|| io::Error::other("non-UTF8 XCTest product path"))?;
    for value in document.values_mut() { expand(value, root); }
    bytes.clear();
    plist::to_writer_xml(&mut bytes, &document).map_err(io::Error::other)?;
    Ok(bytes)
}

#[derive(Deserialize)]
struct Bundle {
    #[serde(rename = "CFBundleIdentifier")]
    identifier: String,
    #[serde(rename = "CFBundleExecutable")]
    executable: String,
    #[serde(rename = "CFBundlePackageType")]
    package_type: String,
}
fn verify_bootstrap(config: &RunnerLaunch, path: &Path) -> io::Result<()> {
    let bootstrap = config.bootstrap_app.as_ref().ok_or_else(|| io::Error::other("bootstrap app absent"))?;
    bootstrap.inventory.verify(path)?;
    let bundle: Bundle = plist::from_file(path.join("Info.plist")).map_err(io::Error::other)?;
    if bundle.identifier.is_empty() || bundle.identifier == config.app.bundle
        || bundle.identifier == config.app.runner_bundle || bundle.package_type != "APPL"
        || bundle.executable.is_empty() || bundle.executable.contains('/') || bundle.executable == "." || bundle.executable == ".."
        || !bootstrap.inventory.files.get(&bundle.executable).is_some_and(|f| f.executable) {
        return Err(io::Error::other("bootstrap must be an independent executable application"));
    }
    Ok(())
}

pub(super) fn prepare(config: &RunnerLaunch, directory: &Path) -> io::Result<()> {
    let Some(bootstrap) = &config.bootstrap_app else { return Ok(()); };
    if !bootstrap.path.is_absolute() { return Err(io::Error::other("bootstrap app path must be absolute")); }
    verify_bootstrap(config, &bootstrap.path)?;
    let bytes = derived(config, directory)?;
    bootstrap.inventory.copy_verified(&bootstrap.path, &directory.join("bootstrap.app"))?;
    frontcap_core::state::save_atomic_bytes(&directory.join("bootstrap.xctestrun"), &bytes)
}

pub(super) fn verified_path(config: &RunnerLaunch, directory: &Path) -> io::Result<PathBuf> {
    if config.bootstrap_app.is_none() { return Ok(directory.join("products").join(&config.manifest.xctestrun)); }
    verify_bootstrap(config, &directory.join("bootstrap.app"))?;
    let path = directory.join("bootstrap.xctestrun");
    if std::fs::read(&path)? != derived(config, directory)? {
        return Err(io::Error::other("derived XCTest configuration changed"));
    }
    Ok(path)
}
