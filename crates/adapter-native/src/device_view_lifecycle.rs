//! View host transitions retain SDK command evidence for read-only result recovery.
use super::*;
use frontcap_core::{
    ids::RequestId,
    native::{
        android_app::Owner,
        android_lifecycle::{Process, Transition},
        app_state::State,
    },
    state::load,
};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    owner: Owner,
    state: State,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Samples {
    before: Vec<u64>,
    after: Vec<u64>,
    focused_before: u64,
    focused_after: u64,
    dispatch: u64,
    clock_before: u64,
    clock_after: Option<u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    adb: PathBuf,
    transition: Transition,
    samples: Samples,
    commands: BTreeMap<String, String>,
}
fn attempt(root: &Path, id: &RequestId) -> PathBuf {
    root.join("view-app-state").join(id.as_str())
}
fn arguments(owner: &Owner, state: State) -> Vec<String> {
    match state {
        State::Background => ["shell", "input", "keyevent", "KEYCODE_HOME"]
            .map(str::to_owned)
            .to_vec(),
        State::Foreground => vec![
            "shell".into(),
            "am".into(),
            "start".into(),
            "-W".into(),
            "--activity-reorder-to-front".into(),
            "-n".into(),
            format!("{}/{}", owner.package, owner.activity),
        ],
    }
}
fn command(
    root: &Path,
    proof: &Proof,
    owner: &Owner,
    number: u64,
    args: &[&str],
) -> io::Result<String> {
    for name in ["command.json", "exchange.json"] {
        let relative = format!("command-{number}/{name}");
        let expected = proof
            .commands
            .get(&relative)
            .ok_or_else(|| io::Error::other("unbound Android host command"))?;
        let path = root.join(relative);
        if std::fs::symlink_metadata(path.parent().unwrap())?
            .file_type()
            .is_symlink()
            || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
            || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(path)?) != *expected
        {
            return Err(io::Error::other("Android host command evidence changed"));
        }
    }
    let (program, actual): (String, Vec<String>) =
        load(&root.join(format!("command-{number}/command.json")))?
            .ok_or_else(|| io::Error::other("missing Android host command"))?;
    let expected: Vec<_> = ["-s", &owner.serial]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    if Path::new(&program) != proof.adb || !proof.adb.is_absolute() || actual != expected {
        return Err(io::Error::other("Android host command arguments differ"));
    }
    let exchange: crate::process::Exchange<()> =
        crate::process::load_exchange(&root.join(format!("command-{number}/exchange.json")))?
            .ok_or_else(|| io::Error::other("missing Android host exchange"))?;
    if exchange.exit_code != Some(0)
        || exchange.failure.is_some()
        || exchange.cleanup_failure.is_some()
        || exchange.evidence_failure.is_some()
    {
        return Err(io::Error::other("Android host command did not complete"));
    }
    String::from_utf8(exchange.stdout).map_err(io::Error::other)
}
fn process(root: &Path, proof: &Proof, owner: &Owner, numbers: &[u64]) -> io::Result<Process> {
    let mut samples = numbers.iter();
    let process =
        crate::android_installation::process(&owner.package, &owner.activity, owner.pid, |args| {
            let number = samples
                .next()
                .ok_or_else(|| io::Error::other("missing Android process sample"))?;
            command(root, proof, owner, *number, args)
        })?;
    if samples.next().is_some() {
        return Err(io::Error::other("extra Android process sample"));
    }
    Ok(process)
}
fn clock(owner: &Owner, raw: &str) -> io::Result<u64> {
    let clock: crate::android_app::InitialClock = serde_json::from_str(raw)?;
    if clock.pid != owner.pid
        || clock.generation != owner.generation
        || !matches!(clock.status.as_str(), "ready" | "ok")
        || clock.now_ns > i64::MAX as u64
    {
        return Err(io::Error::other(
            "Android host transition changed its clock owner",
        ));
    }
    Ok(clock.now_ns)
}
/// No ADB or process queries: reconstruct only from the original command/exchange bytes.
pub fn read_view_transition(
    root: &Path,
    id: &RequestId,
    owner: &Owner,
    state: State,
) -> io::Result<Option<Transition>> {
    let path = attempt(root, id);
    let Some(intent): Option<Intent> = load(&path.join("intent.json"))? else {
        return Ok(None);
    };
    if intent.owner != *owner || intent.state != state {
        return Err(io::Error::other("Android host transition binding differs"));
    }
    let Some(proof): Option<Proof> = load(&path.join("receipt.json"))? else {
        return Ok(None);
    };
    let config: AndroidLaunch = load(&root.join("launch.json"))?
        .ok_or_else(|| io::Error::other("missing Android host launch evidence"))?;
    if proof.adb != config.adb
        || owner.serial != format!("emulator-{}", config.port)
        || owner.source_sha256 != config.source_sha256
        || owner.apk_sha256 != config.apk_sha256
        || config
            .view_app
            .as_ref()
            .is_none_or(|app| app.package != owner.package || app.activity != owner.activity)
    {
        return Err(io::Error::other(
            "Android visibility source differs from frozen launch",
        ));
    }
    let sampled = &proof.samples;
    let ordered: Vec<_> = sampled
        .before
        .iter()
        .copied()
        .chain([
            sampled.clock_before,
            sampled.focused_before,
            sampled.dispatch,
            sampled.focused_after,
        ])
        .chain(sampled.clock_after)
        .chain(sampled.after.iter().copied())
        .collect();
    if sampled.before.is_empty()
        || sampled.after.is_empty()
        || ordered.first() == Some(&0)
        || ordered.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(io::Error::other(
            "Android host samples are not in effect order",
        ));
    }
    let before = process(root, &proof, owner, &sampled.before)?;
    let after = process(root, &proof, owner, &sampled.after)?;
    let focus = |number| {
        super::webview_lifecycle::focused_component(&command(
            root,
            &proof,
            owner,
            number,
            &["shell", "dumpsys", "window"],
        )?)
    };
    let focused_before = focus(sampled.focused_before)?;
    let focused_after = focus(sampled.focused_after)?;
    let args = arguments(owner, state);
    command(
        root,
        &proof,
        owner,
        sampled.dispatch,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    let clock_args = [
        "shell",
        "run-as",
        &owner.package,
        "cat",
        "files/clock-control.json",
    ];
    clock(
        owner,
        &command(root, &proof, owner, sampled.clock_before, &clock_args)?,
    )?;
    let now_ns = sampled
        .clock_after
        .map(|number| {
            command(root, &proof, owner, number, &clock_args).and_then(|raw| clock(owner, &raw))
        })
        .transpose()?;
    let transition = Transition {
        owner: owner.clone(),
        requested: state,
        process_before: before,
        process_after: after,
        focused_before,
        focused_after,
        now_ns,
    };
    transition.validate().map_err(io::Error::other)?;
    if transition != proof.transition {
        return Err(io::Error::other(
            "Android host receipt differs from source commands",
        ));
    }
    Ok(Some(transition))
}

impl Device {
    pub(crate) fn require_view_foreground(&self) -> io::Result<()> {
        if self.view_lifecycle_uncertain || self.view_lifecycle_state != State::Foreground {
            return Err(io::Error::other(
                "Android View app is background or transition is unresolved",
            ));
        }
        Ok(())
    }
    fn sample_view_process(
        &mut self,
        owner: &Owner,
        samples: &mut Vec<u64>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Process> {
        crate::android_installation::process(&owner.package, &owner.activity, owner.pid, |args| {
            let result = self
                .adb(args, deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("missing Android process response"))?;
            samples.push(self.command_number);
            Ok(result)
        })
    }
    pub(crate) fn set_view_state(
        &mut self,
        id: &RequestId,
        owner: &Owner,
        state: State,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Transition> {
        if let Some(result) = read_view_transition(&self.directory, id, owner, state)? {
            return Ok(result);
        }
        let path = attempt(&self.directory, id);
        if path.exists()
            || self.view_restart_uncertain
            || self.view_lifecycle_uncertain
            || self.view_lifecycle_state == state
            || self.view_ready() != Some(owner)
        {
            return Err(io::Error::other(
                "Android host transition unresolved or owner differs",
            ));
        }
        if let Some(entries) = &self.entry_dispatches {
            entries.ensure_resolved()?;
        }
        if !self.unretired_commands.is_empty() {
            return Err(io::Error::other("Android child retirement unresolved"));
        }
        check_budget(deadline, cancelled)?;
        let first = self.command_number + 1;
        let mut samples = Samples::default();
        let before = self.sample_view_process(owner, &mut samples.before, deadline, cancelled)?;
        if self
            .view_lifecycle_process
            .as_ref()
            .is_some_and(|original| original != &before)
        {
            return Err(io::Error::other(
                "Android app process changed since its last host transition",
            ));
        }
        let clock_args = [
            "shell",
            "run-as",
            &owner.package,
            "cat",
            "files/clock-control.json",
        ];
        clock(
            owner,
            &self.adb(&clock_args, deadline, cancelled, false)?.unwrap(),
        )?;
        samples.clock_before = self.command_number;
        let focused_before = super::webview_lifecycle::focused_component(
            &self
                .adb(&["shell", "dumpsys", "window"], deadline, cancelled, false)?
                .unwrap(),
        )?;
        samples.focused_before = self.command_number;
        let component = format!("{}/{}", owner.package, owner.activity);
        let valid_before = match state {
            State::Background => focused_before == component,
            State::Foreground => focused_before.split('/').next() != Some(owner.package.as_str()),
        };
        if !valid_before {
            return Err(io::Error::other(
                "Android host visibility differs before transition",
            ));
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::create_dir(&path)?;
        save_atomic(
            &path.join("intent.json"),
            &Intent {
                owner: owner.clone(),
                state,
            },
        )?;
        self.view_lifecycle_uncertain = true;
        let args = arguments(owner, state);
        self.adb(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            deadline,
            cancelled,
            false,
        )?;
        samples.dispatch = self.command_number;
        let focused_after = loop {
            let raw = self
                .adb(&["shell", "dumpsys", "window"], deadline, cancelled, false)?
                .unwrap();
            if let Ok(focus) = super::webview_lifecycle::focused_component(&raw) {
                let reached = match state {
                    State::Background => focus.split('/').next() != Some(owner.package.as_str()),
                    State::Foreground => focus == component,
                };
                if reached {
                    samples.focused_after = self.command_number;
                    break focus;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let now_ns = if state == State::Foreground {
            let now = clock(
                owner,
                &self.adb(&clock_args, deadline, cancelled, false)?.unwrap(),
            )?;
            samples.clock_after = Some(self.command_number);
            Some(now)
        } else {
            None
        };
        let after = self.sample_view_process(owner, &mut samples.after, deadline, cancelled)?;
        let transition = Transition {
            owner: owner.clone(),
            requested: state,
            process_before: before,
            process_after: after,
            focused_before,
            focused_after,
            now_ns,
        };
        transition.validate().map_err(io::Error::other)?;
        let mut commands = BTreeMap::new();
        for number in first..=self.command_number {
            for name in ["command.json", "exchange.json"] {
                let relative = format!("command-{number}/{name}");
                commands.insert(
                    relative.clone(),
                    frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(
                        self.directory.join(relative),
                    )?),
                );
            }
        }
        save_atomic(
            &path.join("receipt.json"),
            &Proof {
                adb: self.config.adb.clone(),
                transition: transition.clone(),
                samples,
                commands,
            },
        )?;
        // Qualify the just-written receipt through the same path used after controller loss.
        let verified = read_view_transition(&self.directory, id, owner, state)?
            .ok_or_else(|| io::Error::other("Android host transition receipt disappeared"))?;
        self.view_lifecycle_state = state;
        self.view_lifecycle_process = Some(verified.process_after.clone());
        self.view_lifecycle_uncertain = false;
        Ok(verified)
    }
}
