//! Fresh UIKit app data, before XCTest can launch the consumer. Runner reattachment never applies it.
use super::*;
use crate::{app_data::Prepared, products::BundleInventory};
use frontcap_core::{
    canonical::{canonical_of, sha256_hex},
    state::load,
};
use std::collections::BTreeMap;

fn app_path(config: &XctestLaunch, directory: &Path) -> PathBuf {
    directory
        .join("products/Debug-iphonesimulator")
        .join(format!("{}.app", config.manifest.names.app))
}

pub(super) fn admit(
    config: &XctestLaunch,
    directory: &Path,
) -> io::Result<Option<(Prepared, BundleInventory)>> {
    let Some(source) = &config.initial_data else {
        return Ok(None);
    };
    if !source.is_absolute() {
        return Err(io::Error::other("initial app data must be absolute"));
    }
    let product = BundleInventory::capture(&app_path(config, directory))?;
    let digest = sha256_hex(&canonical_of(&product));
    let prepared = Prepared::read(source, &config.app.bundle, &digest)?;
    if config.bootstrap_app.is_none() {
        return Err(io::Error::other("initial data requires an independent XCTest bootstrap app"));
    }
    Ok(Some((prepared, product)))
}

pub(super) fn stage(
    simulator: &mut Simulator,
    config: &XctestLaunch,
    directory: &Path,
    input: Prepared,
    product: BundleInventory,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let id = simulator.udid().to_owned();
    let source = config
        .initial_data
        .as_ref()
        .ok_or_else(|| io::Error::other("initial data absent"))?;
    let frozen = directory.join("initial-data");
    let child = input.recipe.construct(
        &source.join("data"),
        &frozen,
        &id.to_ascii_lowercase(),
        &config.app.bundle,
        &sha256_hex(&canonical_of(&product)),
    )?;
    simulator.ensure_booted(deadline, cancelled)?;
    let env = BTreeMap::new();
    let apps = simulator.command_with_environment(&["listapps", &id], &env, deadline, cancelled)?;
    let apps: BTreeMap<String, serde::de::IgnoredAny> =
        plist::from_bytes(&apps).map_err(io::Error::other)?;
    if apps.contains_key(&config.app.bundle) {
        return Err(io::Error::other(
            "initial app data requires a fresh installation",
        ));
    }
    save_atomic(&directory.join("initial-data-product.json"), &product)?;
    save_atomic(&directory.join("initial-data-staged.json"), &child)
}

/// Called only after the runner identity is verified and before any app-launch request.
pub(super) fn apply(owner: &mut Xctest, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    let directory = owner.directory.clone();
    let Some(staged): Option<Prepared> = load(&directory.join("initial-data-staged.json"))? else {
        return Ok(());
    };
    if owner.ready_process.as_ref().is_none_or(|p| p.has_exited().unwrap_or(true)) {
        return Err(io::Error::other("initial data requires a verified live runner"));
    }
    if directory.join("initial-data-applied.json").try_exists()? {
        return Err(io::Error::other("initial data was already applied"));
    }
    let config: XctestLaunch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("initial data launch identity absent"))?;
    config.manifest.verify(&directory.join("products"))?;
    let product: BundleInventory = load(&directory.join("initial-data-product.json"))?
        .ok_or_else(|| io::Error::other("initial data product identity absent"))?;
    let frozen = directory.join("initial-data");
    let child = Prepared::read(&frozen, &config.app.bundle, &sha256_hex(&canonical_of(&product)))?;
    if child != staged { return Err(io::Error::other("staged initial data changed")); }
    let simulator = owner.simulator_mut();
    let id = simulator.udid().to_owned();
    let env = BTreeMap::new();
    let app = app_path(&config, &directory);
    // XCTest may defer installation of the target until its first launch request.
    // Install after runner setup so its setup cannot discard the prepared container.
    simulator.command_with_environment(
        &["install", &id, app.to_str().ok_or_else(|| io::Error::other("non-UTF8 app path"))?],
        &env, deadline, cancelled,
    )?;
    let mut container = |kind: &str| -> io::Result<PathBuf> {
        let bytes = simulator.command_with_environment(
            &["get_app_container", &id, &config.app.bundle, kind],
            &env,
            deadline,
            cancelled,
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(io::Error::other)?
            .trim();
        let path = PathBuf::from(text);
        if !path.is_absolute() || text.lines().count() != 1 {
            return Err(io::Error::other("invalid installed container path"));
        }
        simulator.canonical_container(&path)
    };
    let installed_path = container("app")?;
    let installed = BundleInventory::capture(&installed_path)?;
    save_atomic(
        &directory.join("initial-data-installed-product.json"),
        &installed,
    )?;
    let libraries = product
        .files
        .keys()
        .filter_map(
            |name| match crate::simulator_app::prepared::shared_library(&app.join(name)) {
                Ok(true) => Some(Ok(name.clone())),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            },
        )
        .collect::<io::Result<std::collections::BTreeSet<_>>>()?;
    crate::simulator_app::prepared::validate_installed_inventory(&product, &installed, &libraries)?;
    let data = container("data")?;
    // Ordinary XCTest installation has completed before the runner endpoint is admitted.
    let runner = directory
        .join("products/Debug-iphonesimulator")
        .join(format!("{}-Runner.app", config.manifest.names.tests));
    let runner_product = BundleInventory::capture(&runner)?;
    let output = simulator.command_with_environment(
        &["get_app_container", &id, &config.app.runner_bundle, "app"],
        &env,
        deadline,
        cancelled,
    )?;
    let runner_path = PathBuf::from(
        std::str::from_utf8(&output)
            .map_err(io::Error::other)?
            .trim(),
    );
    let runner_path = simulator.canonical_container(&runner_path)?;
    let runner_installed = BundleInventory::capture(&runner_path)?;
    save_atomic(
        &directory.join("initial-data-installed-runner.json"),
        &runner_installed,
    )?;
    let mut libraries = runner_product
        .files
        .keys()
        .filter_map(|name| {
            match crate::simulator_app::prepared::shared_library(&runner.join(name)) {
                Ok(true) => Some(Ok(name.clone())),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<io::Result<std::collections::BTreeSet<_>>>()?;
    let bundle = format!("PlugIns/{0}.xctest/{0}", config.manifest.names.tests);
    if !crate::simulator_app::prepared::test_bundle(&runner.join(&bundle))? {
        return Err(io::Error::other(
            "declared XCTest executable is not a Mach-O bundle",
        ));
    }
    libraries.insert(bundle);
    crate::simulator_app::prepared::validate_installed_inventory(
        &runner_product,
        &runner_installed,
        &libraries,
    )?;
    // No application process may have started before these fresh-container writes.
    let mut command = Command::new("/bin/ps");
    command.args(["-axo", "comm="]);
    let processes = simulator.capture_owned(&mut command, deadline, cancelled, 1024 * 1024,
        &directory.join("initial-data-processes"))?;
    if let Some(error) = processes.cleanup_failure.as_ref() { simulator.retain_failure(error.clone()); }
    if let Some(error) = processes.failure.or(processes.cleanup_failure).or(processes.evidence_failure) {
        return Err(io::Error::other(error));
    }
    if std::str::from_utf8(&processes.stdout).map_err(io::Error::other)?.lines()
        .any(|line| Path::new(line.trim()).starts_with(&installed_path)) {
        return Err(io::Error::other("initial data requires an unstarted application"));
    }
    crate::app_data::apply_new_files(&child, &frozen.join("data"), &data)?;
    save_atomic(&directory.join("initial-data-product.json"), &product)?;
    save_atomic(&directory.join("initial-data-applied.json"), &(child, data))
}

/// XCTest may install products again. Never accept a different app data container at readiness.
pub(super) fn verify_container(directory: &Path, container: &Path) -> io::Result<()> {
    let applied: Option<(Prepared, PathBuf)> = load(&directory.join("initial-data-applied.json"))?;
    if applied.is_some_and(|(_, expected)| expected != container) {
        return Err(io::Error::other(
            "initial data container changed during XCTest launch",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "xctest_app_data_tests.rs"]
mod tests;
