//! Same-install View replacement under the existing live emulator owner.
use super::*;
use frontcap_core::{
    ids::RequestId,
    native::android_lifecycle::{Installation, Process, Replacement},
    state::load,
};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    previous: crate::android_app::Ready,
    process: Process,
    installation: Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    replacement: Replacement,
    commands: BTreeMap<String, String>,
    capture: frontcap_core::native::android_capture::Capture,
}

/// Original evidence lookup only. It never opens ADB, constructs a Device or republishes ownership.
pub fn read_view_replacement(
    root: &Path,
    id: &RequestId,
    generation: &str,
    url: Option<&str>,
) -> io::Result<Option<Replacement>> {
    let directory = root.join("view-restarts").join(id.as_str());
    let Some(intent): Option<Intent> = load(&directory.join("intent.json"))? else {
        return Ok(None);
    };
    if intent.previous.generation != generation || intent.url.as_deref() != url {
        return Err(io::Error::other("Android restart generation or URL conflicts"));
    }
    let Some(proof): Option<Proof> = load(&directory.join("receipt.json"))? else {
        return Ok(None);
    };
    proof.replacement.validate().map_err(io::Error::other)?;
    if proof.replacement.previous != intent.previous
        || proof.replacement.previous_process != intent.process
        || proof.replacement.installation != intent.installation
        || proof.replacement.entry.as_ref().map(|e| &e.request)
            != frontcap_core::native::android_lifecycle::restart_entry(&intent.previous, url)
                .map_err(io::Error::other)?.as_ref()
        || proof.commands.is_empty()
    {
        return Err(io::Error::other(
            "Android replacement differs from original intent",
        ));
    }
    for (name, digest) in &proof.commands {
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() != 2
            || !parts[0]
                .strip_prefix("command-")
                .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == s))
            || !["command.json", "exchange.json"].contains(&parts[1])
        {
            return Err(io::Error::other("invalid Android restart evidence path"));
        }
        let path = root.join(name);
        if std::fs::symlink_metadata(path.parent().unwrap())?
            .file_type()
            .is_symlink()
            || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
            || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&path)?) != *digest
        {
            return Err(io::Error::other("Android restart command evidence changed"));
        }
        crate::process::validate_exchange_files(&path)?;
    }
    let capture = crate::android_collection::verify_retained_observation(
        &directory.join("initial-view"),
        &crate::android_app::capture_owner(&proof.replacement.owner),
    )
    .map_err(io::Error::other)?;
    if proof.capture.now_ns != 0
        || capture != frontcap_core::native::android_capture::Collection::Observed(proof.capture)
    {
        return Err(io::Error::other("Android replacement capture changed"));
    }
    Ok(Some(proof.replacement))
}

impl Device {
    fn inspect_view_installation(
        &mut self,
        owner: &crate::android_app::Ready,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Installation> {
        crate::android_installation::installation(
            &owner.package,
            &owner.activity,
            &owner.apk_sha256,
            |args| {
                self.adb(args, deadline, cancelled, false)?
                    .ok_or_else(|| io::Error::other("missing Android installation response"))
            },
        )
    }
    fn inspect_view_process(
        &mut self,
        owner: &crate::android_app::Ready,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Process> {
        crate::android_installation::process(&owner.package, &owner.activity, owner.pid, |args| {
            self.adb(args, deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("missing Android process response"))
        })
    }
    pub(super) fn app_process_absent(
        &mut self,
        package: &str,
        pid: u32,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<bool> {
        if let Some(pids) = self.adb(
            &["shell", "pidof", package],
            deadline,
            cancelled,
            true,
        )? {
            if pids.trim() != pid.to_string() {
                return Err(io::Error::other(
                    "Android package process changed during retirement",
                ));
            }
            return Ok(false);
        }
        // `allow_missing` is deliberately broad elsewhere; absence here requires pidof's exact
        // no-match exit, not permission denial, transport loss or a partial response.
        let exchange: crate::process::Exchange<()> = crate::process::load_exchange(
            &self
                .directory
                .join(format!("command-{}/exchange.json", self.command_number)),
        )?
        .ok_or_else(|| io::Error::other("missing Android retirement command"))?;
        if exchange.exit_code != Some(1)
            || !exchange.stdout.is_empty()
            || !exchange.stderr.is_empty()
        {
            return Err(io::Error::other("Android process absence is unconfirmed"));
        }
        let path = format!("/proc/{}", pid);
        Ok(self
            .adb(
                &["shell", "run-as", package, "test", "!", "-d", &path],
                deadline,
                cancelled,
                true,
            )?
            .is_some())
    }
    /// One stop/start pair. Any uncertainty fences further device work until owned retirement.
    pub fn restart_view(
        &mut self,
        id: &RequestId,
        generation: &str,
        url: Option<&str>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Replacement> {
        if let Some(result) = read_view_replacement(&self.directory, id, generation, url)? {
            return Ok(result);
        }
        self.require_view_foreground()?;
        let directory = self.directory.join("view-restarts").join(id.as_str());
        if directory.exists() || self.view_restart_uncertain {
            return Err(io::Error::other(
                "Android replacement unresolved; never replay",
            ));
        }
        check_budget(deadline, cancelled)?;
        self.ensure_alive()?;
        if !self.unretired_commands.is_empty() {
            return Err(io::Error::other("Android child retirement unresolved"));
        }
        let previous = self
            .view_ready()
            .ok_or_else(|| io::Error::other("Android View owner unavailable"))?
            .clone();
        if previous.generation != generation || self.has_view_entry(id) {
            return Err(io::Error::other(
                "Android restart owner or request ID differs",
            ));
        }
        if let Some(entries) = &self.entry_dispatches {
            entries.ensure_resolved()?;
        }
        let entry = frontcap_core::native::android_lifecycle::restart_entry(&previous, url)
            .map_err(io::Error::other)?;
        let first = self.command_number + 1;
        let installation = self.inspect_view_installation(&previous, deadline, cancelled)?;
        let process = self.inspect_view_process(&previous, deadline, cancelled)?;
        let raw = self
            .adb(
                &[
                    "shell",
                    "run-as",
                    &previous.package,
                    "cat",
                    "files/clock-control.json",
                ],
                deadline,
                cancelled,
                false,
            )?
            .unwrap();
        let clock: crate::android_app::InitialClock = serde_json::from_str(&raw)?;
        if clock.generation != previous.generation || clock.pid != previous.pid {
            return Err(io::Error::other("Android clock owner changed"));
        }
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        save_atomic(
            &directory.join("intent.json"),
            &Intent {
                previous: previous.clone(),
                process: process.clone(),
                installation: installation.clone(),
                url: url.map(str::to_owned),
            },
        )?;
        self.view_restart_uncertain = true;
        let result = (|| {
            self.adb(
                &["shell", "am", "force-stop", &previous.package],
                deadline,
                cancelled,
                false,
            )?;
            while !self.app_process_absent(&previous.package, previous.pid, deadline, cancelled)? {
                std::thread::sleep(Duration::from_millis(50));
            }
            save_atomic(&directory.join("terminated.json"), &process)?;
            if self.inspect_view_installation(&previous, deadline, cancelled)? != installation {
                return Err(io::Error::other("Android retirement changed installation"));
            }
            let component = format!("{}/{}", previous.package, previous.activity);
            let args = if let Some(request) = &entry {
                let mut args = crate::external_entry::android_arguments(&previous.package, &request.url);
                crate::android_app::append_launch_extras(&mut args, self.config.view_app.as_ref());
                args
            } else {
                crate::android_app::launch_arguments(&component, self.config.view_app.as_ref())
            };
            let output = self.adb(
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                deadline,
                cancelled,
                false,
            )?;
            if entry.is_some() {
                crate::external_entry::android_accepted(
                    output.as_deref().ok_or_else(|| io::Error::other("Android cold URL output unavailable"))?,
                    &previous.package, true,
                )?;
            }
            let owner = loop {
                check_budget(deadline, cancelled)?;
                if let Some(raw) = self.adb(
                    &[
                        "shell",
                        "run-as",
                        &previous.package,
                        "cat",
                        "files/clock-control.json",
                    ],
                    deadline,
                    cancelled,
                    true,
                )? {
                    let clock: crate::android_app::InitialClock = serde_json::from_str(&raw)?;
                    if clock.generation != previous.generation {
                        clock.validate()?;
                        break crate::android_app::Ready {
                            pid: clock.pid,
                            generation: clock.generation,
                            ..previous.clone()
                        };
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let current = self.inspect_view_process(&owner, deadline, cancelled)?;
            if self.inspect_view_installation(&owner, deadline, cancelled)? != installation {
                return Err(io::Error::other("Android replacement changed installation"));
            }
            let replacement = Replacement {
                previous,
                owner,
                previous_process: process,
                process: current,
                installation,
                entry: entry.map(frontcap_core::native::external_entry::Accepted::new),
            };
            replacement.validate().map_err(io::Error::other)?;
            let captured = self
                .collect_view_for(
                    &replacement.owner,
                    &directory.join("initial-view"),
                    deadline,
                    cancelled,
                )
                .map_err(io::Error::other)?;
            let frontcap_core::native::android_capture::Collection::Observed(capture) = captured
            else {
                return Err(io::Error::other("Android replacement surface unavailable"));
            };
            if capture.now_ns != 0 {
                return Err(io::Error::other("Android replacement clock is not zero"));
            }
            let mut commands = BTreeMap::new();
            for number in first..=self.command_number {
                for name in ["command.json", "exchange.json"] {
                    let path = format!("command-{number}/{name}");
                    commands.insert(
                        path.clone(),
                        frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(
                            self.directory.join(path),
                        )?),
                    );
                }
            }
            save_atomic(
                &directory.join("receipt.json"),
                &Proof {
                    replacement: replacement.clone(),
                    commands,
                    capture,
                },
            )?;
            Ok(replacement)
        })();
        let replacement = result?;
        let epoch = self
            .directory
            .join("epochs")
            .join(&replacement.owner.generation);
        std::fs::create_dir_all(epoch.parent().unwrap())?;
        std::fs::create_dir(&epoch)?;
        save_atomic(&epoch.join("view-ready.json"), &replacement.owner)?;
        if let Some(entries) = self.entry_dispatches.take() {
            self.retired_view_entries.push(entries);
        }
        self.view_epoch = Some(epoch);
        self.view_ready = Some(replacement.owner.clone());
        self.view_lifecycle_process = None;
        self.view_restart_uncertain = false;
        Ok(replacement)
    }
}
