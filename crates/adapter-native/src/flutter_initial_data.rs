//! Product-bound data application while the live SDK worker holds first launch.
use super::*;
use crate::{app_data::Prepared, flutter_app::Launch as App};
use frontcap_core::canonical::{canonical_of, sha256_hex};
use std::{io::Read, os::unix::fs::MetadataExt};

pub(super) struct Application {
    frozen: Prepared,
    receipt: Option<Receipt>,
    started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
enum Installation {
    Android {
        serial: String,
        boot_id: String,
        installation: frontcap_core::native::android_lifecycle::Installation,
    },
    Ios {
        simulator: String,
        bundle: String,
        containers: crate::flutter_ios_app::Containers,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    token: String,
    installation: Installation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    token: String,
    installation: Installation,
    number: u64,
}
#[derive(Serialize)]
struct Notification<'a> {
    command: &'a str,
    token: &'a str,
    number: u64,
}
#[derive(Serialize)]
struct Command<'a> {
    number: u64,
    arguments: &'a [&'a str],
    timeout_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    number: u64,
    error: Option<String>,
}

fn product(config: &Config) -> io::Result<(String, String)> {
    match config
        .installed_app
        .as_ref()
        .ok_or_else(|| io::Error::other("initial data requires a standalone Flutter product"))?
    {
        App::Android(app) => Ok((app.package.clone(), app.apk_sha256.clone())),
        App::Ios(app) => {
            #[derive(Deserialize)]
            struct Info {
                #[serde(rename = "CFBundleIdentifier")]
                bundle: String,
            }
            let info: Info =
                plist::from_file(app.product.join("Info.plist")).map_err(io::Error::other)?;
            Ok((info.bundle, sha256_hex(&canonical_of(&app.inventory))))
        }
    }
}

pub(super) fn admit(config: &Config) -> io::Result<Option<Prepared>> {
    let Some(path) = &config.initial_data else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(io::Error::other("initial data path must be absolute"));
    }
    let (app, digest) = product(config)?;
    Prepared::read(path, &app, &digest).map(Some)
}

pub(super) fn freeze(
    config: &mut Config,
    directory: &Path,
    admitted: Option<Prepared>,
) -> io::Result<Option<Application>> {
    let Some(input) = admitted else {
        return Ok(None);
    };
    let source = config.initial_data.as_ref().unwrap();
    let destination = directory.join("initial-data-input");
    let frozen = input.recipe.construct(
        &source.join("data"),
        &destination,
        &input.child_id,
        &input.recipe.app_id,
        &input.recipe.product_sha256,
    )?;
    config.initial_data = Some(destination);
    Ok(Some(Application {
        frozen,
        receipt: None,
        started: false,
    }))
}

fn apply_ios(
    input: &Prepared,
    source: &Path,
    destination: &Path,
    app: &crate::flutter_ios_app::Launch,
    installation: &Installation,
    supervised: Option<&Path>,
) -> io::Result<Prepared> {
    let Installation::Ios {
        simulator,
        bundle,
        containers,
    } = installation
    else {
        return Err(io::Error::other(
            "initial data requires an iOS installation",
        ));
    };
    if !frontcap_core::native::request::canonical_uuid(simulator) || bundle != &input.recipe.app_id
    {
        return Err(io::Error::other("initial iOS application identity differs"));
    }
    for path in [&containers.app, &containers.data] {
        if !path.is_absolute()
            || path.canonicalize()? != *path
            || !path.is_dir()
            || (supervised.is_none()
                && !path.components().any(|part| {
                    part.as_os_str()
                        .to_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(simulator))
                }))
        {
            return Err(io::Error::other(
                "initial data container is outside owned Simulator",
            ));
        }
        if let Some(directory) = supervised {
            let created: String = load(&directory.join("ready.json"))?
                .ok_or_else(|| io::Error::other("original supervised Simulator absent"))?;
            if !created.eq_ignore_ascii_case(simulator) {
                return Err(io::Error::other(
                    "initial data differs from original Simulator",
                ));
            }
            let launch: crate::simulator::SimulatorLaunch =
                load(&directory.join("launch.json"))?
                    .ok_or_else(|| io::Error::other("original Simulator launch absent"))?;
            if launch.data_volume.is_some() {
                crate::simulator::data::validate_container(directory, &created, path)?;
            } else if !path.components().any(|part| {
                part.as_os_str()
                    .to_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&created))
            }) {
                return Err(io::Error::other(
                    "initial data container differs from supervised Simulator",
                ));
            }
        }
    }
    let stat = std::fs::metadata(&containers.data)?;
    if stat.dev() != containers.data_device || stat.ino() != containers.data_inode {
        return Err(io::Error::other("initial iOS container changed"));
    }
    let libraries = app
        .inventory
        .files
        .keys()
        .filter_map(|name| {
            match crate::simulator_app::prepared::shared_library(&app.product.join(name)) {
                Ok(true) => Some(Ok(name.clone())),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<io::Result<std::collections::BTreeSet<_>>>()?;
    let actual = crate::products::BundleInventory::capture(&containers.app)?;
    crate::simulator_app::prepared::validate_installed_inventory(
        &app.inventory,
        &actual,
        &libraries,
    )?;
    let child = input.recipe.construct(
        &source.join("data"),
        destination,
        simulator,
        bundle,
        &input.recipe.product_sha256,
    )?;
    crate::app_data::apply_new_files(&child, &destination.join("data"), &containers.data)?;
    Ok(child)
}

impl Session {
    fn initial_command(
        &mut self,
        identity: &Identity,
        number: &mut u64,
        arguments: &[&str],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<String> {
        self.initial_live(deadline, cancelled)?;
        *number = number
            .checked_add(1)
            .ok_or_else(|| io::Error::other("initial command overflow"))?;
        let directory = self
            .directory
            .join("worker/prelaunch")
            .join(number.to_string());
        std::fs::create_dir(&directory)?;
        let timeout_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 120_000) as u64;
        save_atomic(
            &directory.join("request.json"),
            &Command {
                number: *number,
                arguments,
                timeout_ms,
            },
        )?;
        self.initial_notify("initial_data_command", &identity.token, *number)?;
        loop {
            self.initial_live(deadline, cancelled)?;
            if let Some(reply) = load::<Reply>(&directory.join("reply.json"))? {
                if reply.number != *number {
                    return Err(io::Error::other("initial command reply differs"));
                }
                if let Some(error) = reply.error {
                    return Err(io::Error::other(error));
                }
                let mut bytes = Vec::new();
                File::open(directory.join("command.log"))?
                    .take(4 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(io::Error::other("initial command output exceeds bound"));
                }
                return String::from_utf8(bytes).map_err(io::Error::other);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn initial_live(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if cancelled.load(Ordering::Acquire)
            || Instant::now() >= deadline
            || self.control.is_none()
            || self
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("Flutter worker retired"))?
                .try_wait()?
                .is_some()
        {
            return Err(io::Error::other("Flutter initial-data authority expired"));
        }
        Ok(())
    }

    fn initial_notify(&mut self, command: &str, token: &str, number: u64) -> io::Result<()> {
        let control = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::other("Flutter worker authority revoked"))?;
        serde_json::to_writer(
            &mut *control,
            &Notification {
                command,
                token,
                number,
            },
        )?;
        control.write_all(b"\n")
    }

    pub(super) fn prepare_initial_data(
        &mut self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        let Some(state) = self.initial_data.as_ref() else {
            return Ok(());
        };
        if state.receipt.is_some() {
            return Ok(());
        }
        if state.started {
            return Err(io::Error::other(
                "initial data application unresolved; no replay",
            ));
        }
        let Some(identity): Option<Identity> =
            load(&self.directory.join("worker/prelaunch/ready.json"))?
        else {
            return Ok(());
        };
        if !frontcap_core::native::request::canonical_uuid(&identity.token) {
            return Err(io::Error::other("invalid live preparation identity"));
        }
        let input = state.frozen.clone();
        self.initial_data.as_mut().unwrap().started = true;
        self.initial_live(deadline, cancelled)?;
        let app = self.config.installed_app.clone().unwrap();
        app.verify(&self.config.platform)?;
        let source = self.config.initial_data.clone().unwrap();
        if Prepared::read(&source, &input.recipe.app_id, &input.recipe.product_sha256)? != input {
            return Err(io::Error::other("frozen initial data changed"));
        }
        let destination = self.directory.join("initial-data-child");
        let mut number = 0;
        let child = match (&app, &identity.installation) {
            (
                App::Android(app),
                Installation::Android {
                    serial,
                    boot_id,
                    installation,
                },
            ) => {
                if !serial.starts_with("emulator-")
                    || !serial[9..].bytes().all(|b| b.is_ascii_digit())
                    || serial.len() == 9
                    || !frontcap_core::native::request::canonical_uuid(boot_id)
                {
                    return Err(io::Error::other("invalid owned emulator identity"));
                }
                installation
                    .validate(&app.package, &app.apk_sha256)
                    .map_err(io::Error::other)?;
                let child = input.recipe.construct(
                    &source.join("data"),
                    &destination,
                    boot_id,
                    &app.package,
                    &app.apk_sha256,
                )?;
                let before = crate::android_installation::installation(
                    &app.package,
                    &app.activity,
                    &app.apk_sha256,
                    |args| self.initial_command(&identity, &mut number, args, deadline, cancelled),
                )?;
                if &before != installation {
                    return Err(io::Error::other("initial Android installation differs"));
                }
                let temporary = format!("/data/local/tmp/frontcap-data-{}", child.child_id);
                let budget = match self.write_budget.as_ref() {
                    Some(budget) => Some(budget.clone()),
                    None => frontcap_core::write_budget::WriteBudget::from_environment()?,
                };
                crate::android_app_data::apply_files_with_budget(
                    &child,
                    &destination.join("data"),
                    &app.package,
                    &temporary,
                    budget.as_ref(),
                    |args| self.initial_command(&identity, &mut number, args, deadline, cancelled),
                )?;
                let after = crate::android_installation::installation(
                    &app.package,
                    &app.activity,
                    &app.apk_sha256,
                    |args| self.initial_command(&identity, &mut number, args, deadline, cancelled),
                )?;
                if before != after {
                    return Err(io::Error::other("initial Android installation changed"));
                }
                child
            }
            (
                App::Ios(app),
                Installation::Ios {
                    simulator,
                    bundle,
                    containers: _,
                },
            ) => {
                if self.config.resource_executable.is_some() {
                    let raw = self.initial_command(
                        &identity,
                        &mut number,
                        &["apply_ios_initial_data"],
                        deadline,
                        cancelled,
                    )?;
                    let child: Prepared = serde_json::from_str(&raw).map_err(io::Error::other)?;
                    let verified =
                        Prepared::read(&destination, bundle, &input.recipe.product_sha256)?;
                    if child != verified
                        || !child.child_id.eq_ignore_ascii_case(simulator)
                        || child.recipe != input.recipe
                    {
                        return Err(io::Error::other(
                            "supervised initial-data completion differs from original input",
                        ));
                    }
                    child
                } else {
                    apply_ios(
                        &input,
                        &source,
                        &destination,
                        app,
                        &identity.installation,
                        None,
                    )?
                }
            }
            _ => {
                return Err(io::Error::other(
                    "initial data platform differs from installed product",
                ));
            }
        };
        self.initial_live(deadline, cancelled)?;
        save_atomic(&self.directory.join("initial-data-applied.json"), &child)?;
        let receipt = Receipt {
            token: identity.token,
            installation: identity.installation,
            number,
        };
        save_atomic(
            &self.directory.join("worker/prelaunch/applied.json"),
            &receipt,
        )?;
        self.initial_notify("initial_data_applied", &receipt.token, number)?;
        self.initial_data.as_mut().unwrap().receipt = Some(receipt);
        Ok(())
    }

    pub(super) fn check_initial_data_ready(&self, ready: &Ready) -> io::Result<()> {
        let Some(state) = &self.initial_data else {
            return Ok(());
        };
        let expected = state
            .receipt
            .as_ref()
            .ok_or_else(|| io::Error::other("Flutter started before initial-data application"))?;
        let completed: Receipt = load(&self.directory.join("worker/prelaunch/completed.json"))?
            .ok_or_else(|| io::Error::other("Flutter initial-data acknowledgement missing"))?;
        let device = match &completed.installation {
            Installation::Android { serial, .. } => serial,
            Installation::Ios { simulator, .. } => simulator,
        };
        let target = match &completed.installation {
            Installation::Android {
                serial,
                installation,
                ..
            } => frontcap_core::native::external_entry::Target::Android {
                serial: serial.clone(),
                package: installation.package.clone(),
            },
            Installation::Ios {
                simulator, bundle, ..
            } => frontcap_core::native::external_entry::Target::Ios {
                simulator: simulator.clone(),
                bundle: bundle.clone(),
            },
        };
        if &completed != expected || !device.eq_ignore_ascii_case(&ready.owner.device) {
            return Err(io::Error::other(
                "Flutter initial-data completion differs from ready owner",
            ));
        }
        if ready.external_url_target.as_ref() != Some(&target) {
            return Err(io::Error::other(
                "Flutter ready app differs from initial-data installation",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "flutter_initial_data_tests.rs"]
mod tests;

/// Private helper dispatched only through the original independent group's bounded capture.
/// Paths are derived from its original session configuration and prelaunch identity, not caller destinations.
pub fn apply_in_group(directory: &Path) -> io::Result<()> {
    let directory = directory.canonicalize()?;
    let config: Config = load(&directory.join("config.json"))?
        .ok_or_else(|| io::Error::other("original Flutter configuration absent"))?;
    let executable = config
        .resource_executable
        .as_ref()
        .ok_or_else(|| io::Error::other("initial-data helper requires independent resources"))?;
    let worker = directory.join("worker");
    let group: crate::resource_group::Config = load(&worker.join("resource-group/config.json"))?
        .ok_or_else(|| io::Error::other("original resource group binding absent"))?;
    if executable.canonicalize()? != std::env::current_exe()?.canonicalize()?
        || group.resource_executable != *executable
        || group.target != worker.join("resource-target")
        || group.evidence_root.as_ref() != Some(&worker)
    {
        return Err(io::Error::other(
            "initial-data helper differs from original resource owner",
        ));
    }
    let identity: Identity = load(&worker.join("prelaunch/ready.json"))?
        .ok_or_else(|| io::Error::other("original Flutter prelaunch identity absent"))?;
    if !frontcap_core::native::request::canonical_uuid(&identity.token) {
        return Err(io::Error::other("invalid original prelaunch token"));
    }
    let (bundle, digest) = product(&config)?;
    let source = config
        .initial_data
        .as_ref()
        .ok_or_else(|| io::Error::other("frozen initial data absent"))?;
    let input = Prepared::read(source, &bundle, &digest)?;
    let app = config.installed_app.as_ref().unwrap();
    app.verify(&config.platform)?;
    let App::Ios(app) = app else {
        return Err(io::Error::other(
            "initial-data helper requires installed iOS",
        ));
    };
    let child = apply_ios(
        &input,
        source,
        &directory.join("initial-data-child"),
        app,
        &identity.installation,
        Some(&worker.join("resource-target/simulator")),
    )?;
    println!("{}", serde_json::to_string(&child)?);
    Ok(())
}
