//! Preserve existing native command/exchange evidence while the group owns the helper process.
use super::*;
use std::path::Component;

/// Reserve both bounded streams against the remaining raw-output budget. A caller's maximum
/// is a ceiling, not a required reservation: small results may still fit near the group limit.
pub(super) fn remaining_output_limit(requested: usize, retained: usize) -> io::Result<usize> {
    let remaining = (64 * 1024 * 1024usize).saturating_sub(retained);
    if requested == 0 || requested > 16 * 1024 * 1024 || remaining < 2 {
        return Err(io::Error::other(format!(
            "helper output budget unavailable: requested {requested} bytes per stream, retained {retained} bytes, remaining {remaining} bytes")));
    }
    Ok(requested.min(remaining / 2))
}

pub(crate) const DEFAULT_ARTIFACT_BUDGET: usize = 64 * 1024 * 1024;

pub(crate) fn artifact_budget(configured: Option<u64>) -> io::Result<usize> {
    let limit = configured.unwrap_or(DEFAULT_ARTIFACT_BUDGET as u64);
    if limit == 0 || limit > 1024 * 1024 * 1024 {
        return Err(io::Error::other("artifact_budget_bytes must be 1..1073741824"));
    }
    Ok(limit as usize)
}

pub(crate) fn remaining_artifact_limits(requested: usize, diagnostics: usize,
    retained: usize, budget: usize) -> io::Result<crate::process::CaptureLimits> {
    let remaining_diagnostics = (64 * 1024 * 1024usize).saturating_sub(diagnostics);
    let remaining_artifacts = budget.saturating_sub(retained);
    if requested == 0 || requested > 16 * 1024 * 1024 || remaining_diagnostics == 0 || remaining_artifacts == 0 {
        return Err(io::Error::other(format!(
            "binary helper budget unavailable: diagnostics remaining {remaining_diagnostics} bytes; artifacts retained {retained} bytes, remaining {remaining_artifacts} bytes")));
    }
    Ok(crate::process::CaptureLimits {
        stdout: requested.min(remaining_artifacts), stderr: requested.min(remaining_diagnostics),
    })
}

pub(super) fn evidence_path(target: &Path, relative: &Path) -> io::Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(io::Error::other("helper evidence must be a normal relative target path"));
    }
    let evidence = target.join(relative);
    if !evidence.parent().unwrap().canonicalize()?.starts_with(target) {
        return Err(io::Error::other("helper evidence parent escaped the owned target"));
    }
    Ok(evidence)
}
pub(super) fn command(launch: &Launch) -> io::Result<Command> {
    if !launch.program.is_absolute() || !launch.directory.is_absolute() {
        return Err(io::Error::other("helper program and directory must be absolute"));
    }
    let mut command = Command::new(&launch.program);
    command.args(&launch.arguments);
    for name in &launch.remove_environment { command.env_remove(name); }
    command.envs(&launch.environment).current_dir(&launch.directory);
    Ok(command)
}
pub(super) fn confined_emulator(original: &Launch, root: &Path, name: &str, avd: &Path,
    ports: &[u16], server_port: u16, evidence: &Path) -> io::Result<Launch> {
    if !cfg!(target_os = "macos") || ports.len() != 2 || ports[0] < 1024 || !ports[0].is_multiple_of(2) || ports[1] != ports[0] + 1 {
        return Err(io::Error::other("emulator requires its declared console and transport port pair"));
    }
    let mut seen = BTreeSet::new();
    let mut args = original.arguments.iter();
    while let Some(flag) = args.next() {
        if !seen.insert(flag.as_str()) { return Err(io::Error::other("duplicate emulator launch option")); }
        let valid = match flag.as_str() {
            "-avd" => args.next().is_some_and(|value| value == name),
            "-port" => args.next().is_some_and(|value| value == &ports[0].to_string()),
            "-cores" => args.next().is_some_and(|value| value.parse::<u8>().is_ok_and(|value| (1..=32).contains(&value))),
            "-memory" => args.next().is_some_and(|value| value.parse::<u32>().is_ok_and(|value| (256..=16384).contains(&value))),
            "-no-window" | "-no-audio" | "-no-snapshot" | "-read-only" | "-no-boot-anim" => true,
            _ => false,
        };
        if !valid { return Err(io::Error::other("emulator launch differs from its owned configuration")); }
    }
    if !["-avd", "-port", "-no-window", "-no-audio", "-no-snapshot", "-read-only"].iter().all(|flag| seen.contains(flag)) {
        return Err(io::Error::other("emulator launch lacks owned isolation options"));
    }
    let mut command = command(original)?;
    command.env("ANDROID_AVD_HOME", avd).env("ANDROID_TMP", root)
        .env("ANDROID_ADB_SERVER_PORT", server_port.to_string());
    let confined = crate::emulator_confinement::prepare(&command, root, avd, evidence)?;
    launch(&confined, original.deadline_ns)
}

pub(super) fn confined_adb(command: &Command, root: &Path, port: u16) -> io::Result<Command> {
    let endpoint = format!("tcp:127.0.0.1:{port}");
    if !cfg!(target_os = "macos") || !command.get_envs().any(|(key, value)|
        key == "ADB_SERVER_SOCKET" && value == Some(std::ffi::OsStr::new(&endpoint))) {
        return Err(io::Error::other("confined ADB capture requires macOS and the original private endpoint"));
    }
    for name in ["adb-home", "adb-temporary"] {
        let path = root.join(name);
        std::fs::create_dir_all(&path)?;
        if path.canonicalize()? != path { return Err(io::Error::other("ADB scratch directory changed")); }
    }
    crate::emulator_confinement::adb_client(command, root)
}

impl Group {
    /// Start the group's sole ADB server before target resources. It outlives best-effort forwarding removal.
    pub fn start_adb_server(&mut self, id: RequestId, port: u16, command: &Command, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<ProcessIdentity> {
        let launch = launch(command, self.deadline_ns)?;
        match self.execute_until(Operation::StartAdbServer { id, port, launch, confine: false }, deadline, cancelled)? {
            Reply::Started { process } => Ok(process),
            reply => Err(io::Error::other(format!("ADB server did not start: {reply:?}"))),
        }
    }
    /// Caller must use the unchanged original helper evidence for subsequent recovery; no redispatch.
    pub fn capture(&mut self, command: &Command, evidence: &Path, deadline: Instant,
        cancelled: &AtomicBool, output_limit: usize) -> io::Result<crate::process::Exchange> {
        let launch = launch(command, monotonic_ns()?.saturating_add(
            u64::try_from(deadline.saturating_duration_since(Instant::now()).as_nanos()).map_err(io::Error::other)?).min(self.deadline_ns))?;
        let relative = evidence.strip_prefix(&self.ready.evidence_root).map_err(io::Error::other)?.to_owned();
        let expected = evidence_path(&self.ready.evidence_root, &relative)?.join("exchange.json");
        let id = RequestId::new(format!("helper-{}", self.number + 1)).map_err(io::Error::other)?;
        match self.execute_until(Operation::Capture { id, launch, evidence: relative, output_limit, confine_adb: false, request: None, stdout_file: None }, deadline, cancelled)? {
            Reply::Captured { evidence } if evidence == expected => crate::process::load_exchange(&evidence)?
                .ok_or_else(|| io::Error::other("supervised helper exchange absent")),
            reply => Err(io::Error::other(format!("supervised helper did not return its original exchange: {reply:?}"))),
        }
    }
}

pub(super) fn launch(command: &Command, deadline_ns: u64) -> io::Result<Launch> {
        fn text(value: &std::ffi::OsStr) -> io::Result<String> {
            value.to_str().map(str::to_owned).ok_or_else(|| io::Error::other("non-UTF8 supervised helper command"))
        }
        let mut environment = BTreeMap::new();
        let mut remove_environment = Vec::new();
        for (name, value) in command.get_envs() {
            let name = text(name)?;
            if let Some(value) = value { environment.insert(name, text(value)?); }
            else { remove_environment.push(name); }
        }
        let current = std::env::current_dir()?;
        let directory = command.get_current_dir().map(|path| if path.is_absolute() { path.to_owned() } else { current.join(path) }).unwrap_or(current);
        Ok(Launch { program: PathBuf::from(command.get_program()), arguments: command.get_args().map(text).collect::<io::Result<_>>()?,
            environment, remove_environment, directory, deadline_ns })
}

impl Group {
    /// Ownership is published by the leaf before returning its identity; caller never owns its PID.
    pub(crate) fn start_resource(&mut self, id: RequestId, command: &Command, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<ProcessIdentity> {
        let launch = launch(command, self.deadline_ns)?;
        match self.execute_until(Operation::Start { id, launch }, deadline, cancelled)? {
            Reply::Started { process } => Ok(process),
            reply => Err(io::Error::other(format!("resource did not start: {reply:?}"))),
        }
    }
    pub(crate) fn register_forward(&mut self, registration: ForwardRegistration, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<()> {
        let id = RequestId::new(format!("forward-{}", self.number + 1)).map_err(io::Error::other)?;
        match self.execute_until(Operation::RegisterForward { id, registration }, deadline, cancelled)? {
            Reply::ForwardRegistered => Ok(()),
            reply => Err(io::Error::other(format!("forward registration rejected: {reply:?}"))),
        }
    }
}

/// Transport opaque JSON without interpreting the consumer protocol in the resource owner.
/// The caller still decodes its typed reply and validates original completion receipts.
impl Group {
    pub fn exchange<C: Serialize, R: serde::de::DeserializeOwned + Serialize>(&mut self,
        command: &Command, call: &C, evidence: &Path, deadline: Instant,
        cancelled: &AtomicBool, output_limit: usize) -> io::Result<crate::process::Exchange<R>> {
        let launch = launch(command, monotonic_ns()?.saturating_add(
            u64::try_from(deadline.saturating_duration_since(Instant::now()).as_nanos()).map_err(io::Error::other)?).min(self.deadline_ns))?;
        let relative = evidence.strip_prefix(&self.ready.evidence_root).map_err(io::Error::other)?.to_owned();
        let expected = evidence_path(&self.ready.evidence_root, &relative)?.join("exchange.json");
        let id = RequestId::new(format!("helper-{}", self.number + 1)).map_err(io::Error::other)?;
        let request = Some(serde_json::to_string(call)?);
        match self.execute_until(Operation::Capture { id, launch, evidence: relative, output_limit, confine_adb: false, request, stdout_file: None }, deadline, cancelled)? {
            Reply::Captured { evidence } if evidence == expected => crate::process::load_exchange(&evidence)?
                .ok_or_else(|| io::Error::other("supervised bridge exchange absent")),
            reply => Err(io::Error::other(format!("supervised bridge did not return its original exchange: {reply:?}"))),
        }
    }
}

pub(super) struct Captured {
    pub output_bytes: usize,
    pub artifact_bytes: usize,
    pub artifact: Option<HelperEvidence>,
    pub cleanup_failure: Option<String>,
    pub evidence_failure: Option<String>,
    pub forwarding_errors: Vec<String>,
}
#[allow(clippy::too_many_arguments)]
pub(super) fn run(command: &mut Command, request: Option<&str>,
    deadline: Instant, cancelled: &AtomicBool, output_limit: usize, evidence: &Path,
    forwards: &mut BTreeMap<String, forwarding::Forward>) -> io::Result<Captured> {
    fn summarize<R>(result: crate::process::Exchange<R>, command: &Command, evidence: &Path,
        forwards: &mut BTreeMap<String, forwarding::Forward>) -> Captured {
        let forwarding_errors = forwards.values_mut().filter_map(|forward|
            forward.after_capture(command, evidence, &result).err().map(|error|
                format!("forwarding completion publication failed: {error}"))).collect();
        Captured { output_bytes: result.stdout.len() + result.stderr.len(), artifact_bytes: 0, artifact: None,
            cleanup_failure: result.cleanup_failure, evidence_failure: result.evidence_failure,
            forwarding_errors }
    }
    if let Some(request) = request {
        // The tagged command carries opaque JSON text; RawValue cannot be decoded through
        // serde's internally tagged enum buffer. Decode only at the dedicated bridge boundary.
        let request: Box<serde_json::value::RawValue> = serde_json::from_str(request)?;
        let result = crate::process::exchange_typed_recorded::<_, Box<serde_json::value::RawValue>>(
            command, &request, deadline, cancelled, output_limit, evidence)?;
        Ok(summarize(result, command, evidence, forwards))
    } else {
        let result = crate::process::capture_recorded(command, deadline, cancelled, output_limit, evidence)?;
        Ok(summarize(result, command, evidence, forwards))
    }
}


impl Group {
    pub(crate) fn capture_artifact(&mut self, command: &Command, evidence: &Path, name: &str,
        deadline: Instant, cancelled: &AtomicBool, output_limit: usize) -> io::Result<crate::process::ArtifactExchange> {
        let launch = launch(command, monotonic_ns()?.saturating_add(
            u64::try_from(deadline.saturating_duration_since(Instant::now()).as_nanos()).map_err(io::Error::other)?).min(self.deadline_ns))?;
        let relative = evidence.strip_prefix(&self.ready.evidence_root).map_err(io::Error::other)?.to_owned();
        let expected = evidence_path(&self.ready.evidence_root, &relative)?.join("exchange.json");
        let id = RequestId::new(format!("helper-{}", self.number + 1)).map_err(io::Error::other)?;
        match self.execute_until(Operation::Capture { id, launch, evidence: relative, output_limit, confine_adb: false,
            request: None, stdout_file: Some(name.into()) }, deadline, cancelled)? {
            Reply::Captured { evidence } if evidence == expected => load(&evidence)?
                .ok_or_else(|| io::Error::other("supervised binary helper exchange absent")),
            reply => Err(io::Error::other(format!("supervised binary helper did not return original evidence: {reply:?}"))),
        }
    }
}

pub(super) fn run_artifact(command: &mut Command, deadline: Instant, cancelled: &AtomicBool,
    limits: crate::process::CaptureLimits, evidence: &Path, name: &str) -> io::Result<Captured> {
    let result = crate::process::capture_artifact_recorded(command, deadline, cancelled, limits, evidence, name)?;
    let artifact_bytes = result.stdout_file.as_ref().map_or(0, |a| a.bytes as usize);
    Ok(Captured {
        output_bytes: result.exchange.stderr.len(),
        artifact_bytes,
        artifact: result.stdout_file.map(|a| HelperEvidence { path: a.path, sha256: a.sha256 }),
        cleanup_failure: result.exchange.cleanup_failure,
        evidence_failure: result.exchange.evidence_failure,
        forwarding_errors: Vec::new(),
    })
}


#[cfg(test)]
#[path = "resource_group_capture_tests.rs"]
mod tests;
