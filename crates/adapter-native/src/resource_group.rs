//! Live ownership of a native host lease, scratch directory and independently supervised children.
//! Control records request work; only this live group can create resources or retire its host fence.
use crate::{emulator_disks::TemporaryDisks, host::{self, HostSession}, resource_process::{Launch, ResourceProcess, ResultRecord}, worker::monotonic_ns};
use frontcap_core::{ids::RequestId, process::ProcessIdentity, state::load, write_budget::{Class, save_inherited}};
#[cfg(test)]
use frontcap_core::state::save_atomic;
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs::File, io::{self, Read, Write}, net::{Ipv4Addr, SocketAddr, TcpStream},
    os::unix::process::CommandExt, path::{Path, PathBuf}, process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub host: PathBuf,
    pub target: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_budget_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary_storage_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_volume: Option<crate::storage_volume::Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_volume_root: Option<PathBuf>,
    pub resource_executable: PathBuf,
    pub resource_arguments: Vec<String>,
    pub ports: Vec<u16>,
    pub deadline_ns: u64,
}
impl Config {
    pub fn scratch_volume_directory(&self, directory: &Path) -> PathBuf {
        self.scratch_volume_root.clone().unwrap_or_else(|| directory.join("scratch-volume"))
    }
}

pub(crate) fn validate_volume_root(root: &Path) -> io::Result<()> {
    let parent = root.parent().ok_or_else(|| io::Error::other("scratch volume parent missing"))?;
    if !root.is_absolute() || root.file_name().is_none() || parent.canonicalize()? != parent
        || root.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(io::Error::other("scratch volume root requires a canonical absolute parent"));
    }
    Ok(())
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    PrepareAvd { source: PathBuf, name: String, inventory: crate::products::BundleInventory },
    AcquireSimulator { config: crate::simulator::SimulatorLaunch, deadline_ns: u64 },
    SimulatorCommand { arguments: Vec<String>, environment: BTreeMap<String, String>, deadline_ns: u64 },
    RegisterForward { id: RequestId, registration: ForwardRegistration },
    Capture { id: RequestId, launch: Launch, evidence: PathBuf, output_limit: usize,
        #[serde(default)]
        confine_adb: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout_file: Option<String> },
    Start { id: RequestId, launch: Launch },
    StartEmulator { id: RequestId, launch: Launch },
    /// One owned server kept alive until forwarding retirement finishes.
    StartAdbServer { id: RequestId, port: u16, launch: Launch, #[serde(default)] confine: bool },
    Stop { id: RequestId },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    AvdPrepared { directory: PathBuf },
    SimulatorAcquired { udid: String, command_number: u64 },
    SimulatorCommandCompleted { command_number: u64, evidence: PathBuf },
    ForwardRegistered,
    Captured { evidence: PathBuf },
    Started { process: ProcessIdentity },
    Stopped { result: ResultRecord },
    Failed { error: String },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub target: PathBuf,
    pub evidence_root: PathBuf,
    pub temporary_disks: PathBuf,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperEvidence {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdbServer {
    pub id: RequestId,
    pub port: u16,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_server: Option<AdbServer>,
    pub operation_error: Option<String>,
    pub passed: bool,
    pub errors: Vec<String>,
    pub resources: BTreeMap<String, ResultRecord>,
    pub helper_evidence: BTreeMap<String, HelperEvidence>,
    #[serde(default)]
    pub forwarding: BTreeMap<String, ForwardRetirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulator: Option<frontcap_core::native::ownership::Retirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_volume: Option<crate::storage_volume::RetirementEvidence>,
}

pub struct Group {
    child: Option<Child>,
    authority: Option<ChildStdin>,
    directory: PathBuf,
    number: u64,
    deadline_ns: u64,
    pub ready: Ready,
}
impl Group {
    pub fn start(executable: &mut Command, config: &Config, directory: &Path, deadline: Instant) -> io::Result<Self> {
        validate(config)?;
        std::fs::create_dir(directory)?;
        let directory = directory.canonicalize()?;
        File::create_new(directory.join("group.lock"))?;
        std::fs::create_dir(directory.join("commands"))?;
        std::fs::create_dir(directory.join("replies"))?;
        std::fs::create_dir(directory.join("resources"))?;
        save_inherited(Class::Ordinary, &directory.join("config.json"), config)?;
        let mut child = executable.arg(&directory).process_group(0).stdin(Stdio::piped())
            .stdout(Stdio::null()).stderr(crate::diagnostic_log::create_output(&directory.join("group.log"))?).spawn()?;
        let mut authority = child.stdin.take().unwrap();
        let result = (|| {
            authority.write_all(b"1")?;
            loop {
                if child.try_wait()?.is_some() { return Err(io::Error::other("resource group exited before readiness")); }
                if let Some(ready) = load(&directory.join("ready.json"))? { return Ok(ready); }
                if Instant::now() >= deadline { return Err(io::Error::other("resource group readiness observation expired")); }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        match result {
            Ok(ready) => Ok(Self { child: Some(child), authority: Some(authority), directory, number: 0, deadline_ns: config.deadline_ns, ready }),
            Err(error) => { drop(authority); std::thread::spawn(move || { let _ = child.wait(); }); Err(error) }
        }
    }
    /// One serial command. An expired observation revokes authority and never resubmits work.
    pub fn execute(&mut self, operation: Operation, deadline: Instant) -> io::Result<Reply> {
        self.execute_until(operation, deadline, &AtomicBool::new(false))
    }
    pub(crate) fn execute_until(&mut self, operation: Operation, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Reply> {
        if self.authority.is_none() { return Err(io::Error::other("resource group authority revoked")); }
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            self.authority.take();
            return Err(io::Error::other("resource group command cancelled or expired before publication"));
        }
        self.number = self.number.checked_add(1).ok_or_else(|| io::Error::other("resource group sequence exhausted"))?;
        let command = self.directory.join("commands").join(format!("{}.json", self.number));
        let result = (|| {
            save_inherited(Class::Ordinary, &command, &operation)?;
            loop {
                if cancelled.load(Ordering::Acquire) { return Err(io::Error::other("resource group command cancelled; inspect original command")); }
                if let Some(reply) = load(&self.directory.join("replies").join(format!("{}.json", self.number)))? { return Ok(reply); }
                if self.child.as_mut().unwrap().try_wait()?.is_some() { return Err(io::Error::other("resource group exited without reply; inspect original command")); }
                if Instant::now() >= deadline { return Err(io::Error::other("resource group observation expired; inspect original command")); }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        if result.is_err() { self.authority.take(); }
        result
    }
    pub fn stop(mut self, deadline: Instant) -> io::Result<Retirement> {
        self.authority.take();
        loop {
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                self.child.take();
                return load(&self.directory.join("retirement.json"))?
                    .ok_or_else(|| io::Error::other("resource group retirement unresolved"));
            }
            if Instant::now() >= deadline { return Err(io::Error::other("resource group retirement observation expired")); }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.authority.take();
        if let Some(mut child) = self.child.take() { std::thread::spawn(move || { let _ = child.wait(); }); }
    }
}
fn validate(config: &Config) -> io::Result<()> {
    if let Some(volume) = &config.scratch_volume {
        volume.validate()?;
        if config.temporary_storage_root.is_some() {
            return Err(io::Error::other("owned scratch volume conflicts with external scratch root"));
        }
    }
    if let Some(root) = &config.scratch_volume_root {
        validate_volume_root(root)?;
        if config.scratch_volume.is_none() || root.starts_with(&config.target) || config.target.starts_with(root) {
            return Err(io::Error::other("scratch volume root requires a volume and must not overlap the target"));
        }
    }
    capture::artifact_budget(config.artifact_budget_bytes)?;
    if !config.host.is_absolute() || !config.target.is_absolute() || !config.resource_executable.is_absolute()
        || config.ports.contains(&0) || config.ports.len() > 16 || config.deadline_ns <= monotonic_ns()? {
        return Err(io::Error::other("invalid resource group configuration"));
    }
    if let Some(root) = &config.evidence_root {
        if !root.is_absolute() { return Err(io::Error::other("resource evidence root must be absolute")); }
        let root = root.canonicalize()?;
        let parent = config.target.parent().ok_or_else(|| io::Error::other("target parent absent"))?.canonicalize()?;
        if !parent.starts_with(&root) { return Err(io::Error::other("target is outside the evidence root")); }
    }
    Ok(())
}

/// Private process entry. No restart/adoption from a prior group directory is supported.
pub fn run(directory: &Path) -> io::Result<()> {
    let lock = File::options().read(true).write(true).open(directory.join("group.lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    if directory.join("owner.json").exists() || directory.join("retirement.json").exists() {
        return Err(io::Error::other("resource group already started"));
    }
    save_inherited(Class::Ordinary, &directory.join("owner.json"), &ProcessIdentity::of(std::process::id())?
        .ok_or_else(|| io::Error::other("resource group owner absent"))?)?;
    let config: Config = load(&directory.join("config.json"))?.ok_or_else(|| io::Error::other("resource group config absent"))?;
    validate(&config)?;
    let revoked = Arc::new(AtomicBool::new(false));
    let signal = revoked.clone();
    let (send, authorized) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let input = io::stdin(); let mut input = input.lock(); let mut byte = [0];
        let valid = input.read_exact(&mut byte).is_ok() && byte == *b"1";
        let _ = send.send(valid);
        if valid { let _ = input.read(&mut byte); }
        signal.store(true, Ordering::Release);
    });
    if authorized.recv_timeout(Duration::from_nanos(config.deadline_ns.saturating_sub(monotonic_ns()?))) != Ok(true)
        || revoked.load(Ordering::Acquire) { return Err(io::Error::other("resource group not authorized")); }
    let evidence_root = config.evidence_root.as_ref().map(|root| root.canonicalize()).transpose()?;
    // Acquire the shared fence before creating target data or any supervised process.
    let _lease = host::acquire(&config.host)?;
    std::fs::create_dir(&config.target)?;
    let target = config.target.canonicalize()?;
    let evidence_root = evidence_root.unwrap_or_else(|| target.clone());
    let host_record = config.host.join("native-session.json");
    let mut disks = None;
    let mut volume = None;
    let mut volume_attempted = false;
    let mut simulator = None;
    let mut simulator_attempted = false;
    let mut prepared_avd = None;
    let mut active = BTreeMap::<String, ResourceProcess>::new();
    let mut attempted = Vec::<String>::new();
    let mut identities = BTreeSet::new();
    let mut forwards = BTreeMap::<String, forwarding::Forward>::new();
    let mut captured_bytes = 0usize;
    let mut artifact_bytes = 0usize;
    let artifact_budget = capture::artifact_budget(config.artifact_budget_bytes)?;
    let mut command_bytes = 0u64;
    let mut retirement = Retirement { adb_server: None, operation_error: None, passed: false, errors: Vec::new(), resources: BTreeMap::new(), helper_evidence: BTreeMap::new(), forwarding: BTreeMap::new(), simulator: None, scratch_volume: None };
    let work = (|| -> io::Result<()> {
        save_inherited(Class::Ordinary, &host_record, &HostSession { directory: target.clone(), retired: false })?;
        if !target.starts_with(&evidence_root) { return Err(io::Error::other("target escaped its evidence root")); }
        if let Some(plan) = &config.scratch_volume {
            volume_attempted = true;
            let deadline = Instant::now() + Duration::from_nanos(config.deadline_ns.saturating_sub(monotonic_ns()?));
            volume = Some(crate::storage_volume::Worker::start(plan, &config.scratch_volume_directory(directory), deadline)?);
        }
        let storage_root = volume.as_ref().map(|owner| owner.mount.as_path()).or(config.temporary_storage_root.as_deref());
        disks = Some(TemporaryDisks::create(&target, storage_root)?);
        save_inherited(Class::Ordinary, &directory.join("ready.json"), &Ready { target: target.clone(), evidence_root: evidence_root.clone(), temporary_disks: disks.as_ref().unwrap().path.clone() })?;
        for number in 1..=16384 {
            let path = directory.join("commands").join(format!("{number}.json"));
            loop {
                if revoked.load(Ordering::Acquire) || monotonic_ns()? >= config.deadline_ns { return Ok(()); }
                if path.exists() { break; }
                std::thread::sleep(Duration::from_millis(5));
            }
            let bytes = std::fs::metadata(&path)?.len();
            command_bytes = command_bytes.saturating_add(bytes);
            if bytes > 65536 || command_bytes > 16 * 1024 * 1024 { return Err(io::Error::other("resource group command storage limit exceeded")); }
            let command: Operation = load(&path)?.ok_or_else(|| io::Error::other("resource group command disappeared"))?;
            let start_emulator = matches!(&command, Operation::StartEmulator { .. });
            let confine_server = matches!(&command, Operation::StartAdbServer { confine: true, .. });
            let server_port = match &command { Operation::StartAdbServer { port, .. } => Some(*port), _ => None };
            let reply = match command {
                Operation::PrepareAvd { source, name, inventory } => {
                    if !attempted.is_empty() || simulator_attempted {
                        Reply::Failed { error: "AVD preparation must precede resource acquisition".into() }
                    } else {
                        match crate::avd_copy::prepare(&source, &name, &inventory,
                            &disks.as_ref().expect("scratch acquired").path, &target, &mut || {
                                if revoked.load(Ordering::Acquire) {
                                    return Err(io::Error::new(io::ErrorKind::Interrupted, "AVD preparation authority revoked"));
                                }
                                if monotonic_ns()? >= config.deadline_ns {
                                    return Err(io::Error::new(io::ErrorKind::TimedOut, "AVD preparation deadline exceeded"));
                                }
                                Ok(())
                            }) {
                            Ok(directory) => {
                                prepared_avd = Some((name, directory.clone()));
                                Reply::AvdPrepared { directory }
                            },
                            Err(error) => Reply::Failed { error: error.to_string() },
                        }
                    }
                }
                Operation::AcquireSimulator { config: launch, deadline_ns } => {
                    if simulator_attempted || !config.ports.is_empty() || deadline_ns > config.deadline_ns {
                        Reply::Failed { error: "Simulator acquisition reused or incompatible with group".into() }
                    } else {
                        simulator_attempted = true;
                        let end = Instant::now() + Duration::from_nanos(deadline_ns.saturating_sub(monotonic_ns()?));
                        match crate::simulator::Simulator::acquire_in_group(launch, &target.join("simulator"), end, &revoked, &_lease) {
                            Ok(owned) => {
                                let udid = owned.udid().to_owned();
                                let command_number = owned.command_cursor().1;
                                simulator = Some(owned);
                                Reply::SimulatorAcquired { udid, command_number }
                            }
                            Err(error) => {
                                retirement.simulator = error.retirement;
                                return Err(io::Error::other(error.cause));
                            }
                        }
                    }
                }
                Operation::SimulatorCommand { arguments, environment, deadline_ns } => {
                    if let Some(owned) = &mut simulator {
                        let app_command = arguments.first().is_some_and(|name|
                            matches!(name.as_str(), "install" | "launch" | "terminate" | "get_app_container" | "listapps" | "openurl")
                            || (name == "io" && arguments.get(2).is_some_and(|action| action == "screenshot")))
                            && arguments.get(1).is_some_and(|id| id.eq_ignore_ascii_case(owned.udid()));
                        let inventory = arguments == ["list", "devices", "--json"];
                        if (!app_command && !inventory) || environment.keys().any(|name| !name.starts_with("SIMCTL_CHILD_"))
                            || deadline_ns > config.deadline_ns || captured_bytes > 56 * 1024 * 1024
                            || !identities.insert(format!("simulator-helper-{number}")) {
                            Reply::Failed { error: "Simulator command target, operation, environment, lifetime or output budget rejected".into() }
                        } else {
                            let before = owned.command_cursor().1;
                            let args = arguments.iter().map(String::as_str).collect::<Vec<_>>();
                            let end = Instant::now() + Duration::from_nanos(deadline_ns.saturating_sub(monotonic_ns()?));
                            let outcome = owned.command_with_environment(&args, &environment, end, &revoked);
                            let (root, command_number) = owned.command_cursor();
                            if command_number != before + 1 { return Err(outcome.err().unwrap_or_else(|| io::Error::other("Simulator command has no original exchange"))); }
                            let evidence = root.join(format!("command-{command_number}/exchange.json"));
                            let bytes = std::fs::read(&evidence)?;
                            let exchange: crate::process::Exchange = crate::process::load_exchange(&evidence)?.ok_or_else(|| io::Error::other("helper exchange missing"))?;
                            captured_bytes += exchange.stdout.len() + exchange.stderr.len();
                            if let Some(error) = exchange.cleanup_failure.or(exchange.evidence_failure) {
                                retirement.errors.push(error);
                                return Err(io::Error::other("Simulator helper cleanup or publication unresolved"));
                            }
                            retirement.helper_evidence.insert(format!("simulator-helper-{number}"),
                                HelperEvidence { path: evidence.clone(), sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes) });
                            Reply::SimulatorCommandCompleted { command_number, evidence }
                        }
                    } else { Reply::Failed { error: "Simulator is not actively owned".into() } }
                }
                Operation::RegisterForward { id, registration } => {
                    let id = id.as_str().to_owned();
                    let owned = active.iter().any(|(id, resource)| resource.process == registration.emulator
                        && retirement.adb_server.as_ref().is_none_or(|server| server.id.as_str() != id))
                        && !registration.emulator.has_exited()?;
                    let server_matches = retirement.adb_server.as_ref().is_none_or(|server|
                        registration.server_port == Some(server.port) && active.contains_key(server.id.as_str()));
                    let storage_matches = registration.write_root.as_ref().is_none_or(|root|
                        cfg!(target_os = "macos") && retirement.adb_server.is_some()
                            && disks.as_ref().is_some_and(|owned| &owned.path == root));
                    let serial = config.ports.first().map(|port| format!("emulator-{port}"));
                    let conflict = forwards.values().any(|forward|
                        (forward.registration.port == registration.port && !forward.removed)
                        || forward.registration.bind_evidence == registration.bind_evidence);
                    if identities.contains(&id) || forwards.len() >= 64 || !owned
                        || serial.as_deref() != Some(registration.serial.as_str()) || conflict || !server_matches || !storage_matches {
                        Reply::Failed { error: "forward identity, owner, serial or endpoint rejected".into() }
                    } else {
                        let forward = forwarding::Forward::new(registration, &evidence_root,
                            &directory.join("forwarding").join(&id))?;
                        for previous in forwards.values_mut().filter(|previous|
                            previous.registration.port == forward.registration.port) {
                            previous.superseded = true;
                        }
                        identities.insert(id.clone());
                        forwards.insert(id, forward);
                        Reply::ForwardRegistered
                    }
                }
                Operation::Capture { id, launch, evidence, output_limit, request, stdout_file, confine_adb } => {
                    let id = id.as_str().to_owned();
                    let limits = if stdout_file.is_some() {
                        capture::remaining_artifact_limits(output_limit, captured_bytes, artifact_bytes, artifact_budget)
                    } else {
                        capture::remaining_output_limit(output_limit, captured_bytes)
                            .map(|limit| crate::process::CaptureLimits { stdout: limit, stderr: limit })
                    };
                    if !identities.insert(id.clone()) {
                        Reply::Failed { error: "helper identity already used; recover original evidence".into() }
                    } else if launch.deadline_ns > config.deadline_ns {
                        Reply::Failed { error: "helper lifetime exceeds resource group deadline".into() }
                    } else if let Err(error) = limits {
                        Reply::Failed { error: error.to_string() }
                    } else {
                        let limits = limits.unwrap();
                        let evidence = capture::evidence_path(&evidence_root, &evidence)?;
                        let mut command = capture::command(&launch)?;
                        if confine_adb {
                            let server = retirement.adb_server.as_ref().filter(|server| active.contains_key(server.id.as_str()))
                                .ok_or_else(|| io::Error::other("confined ADB capture requires its live owned server"))?;
                            if active[server.id.as_str()].process.has_exited()? {
                                return Err(io::Error::other("confined ADB server has exited"));
                            }
                            command = capture::confined_adb(&command, &disks.as_ref().expect("scratch acquired").path, server.port)?;
                        }
                        let end = Instant::now() + Duration::from_nanos(launch.deadline_ns.saturating_sub(monotonic_ns()?));
                        for forward in forwards.values() { forward.before_capture(&command, &evidence)?; }
                        let result = if let Some(name) = stdout_file {
                            if request.is_some() { return Err(io::Error::other("binary capture cannot carry a bridge request")); }
                            capture::run_artifact(&mut command, end, &revoked, limits, &evidence, &name)?
                        } else {
                            capture::run(&mut command, request.as_deref(), end, &revoked,
                                limits.stdout, &evidence, &mut forwards)?
                        };
                        retirement.errors.extend(result.forwarding_errors);
                        captured_bytes += result.output_bytes;
                        artifact_bytes += result.artifact_bytes;
                        if let Some(proof) = result.artifact {
                            retirement.helper_evidence.insert(format!("{id}/stdout"), proof);
                        }
                        if let Some(error) = result.cleanup_failure.or(result.evidence_failure) {
                            retirement.errors.push(format!("{id}: {error}"));
                            return Err(io::Error::other("helper retirement or evidence publication unresolved"));
                        }
                        let exchange = evidence.join("exchange.json");
                        let proof = match std::fs::read(&exchange) {
                            Ok(bytes) => HelperEvidence { path: exchange.clone(), sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes) },
                            Err(error) => {
                                retirement.errors.push(format!("{id}: helper evidence unavailable: {error}"));
                                return Err(error);
                            }
                        };
                        retirement.helper_evidence.insert(id, proof);
                        if !retirement.errors.is_empty() {
                            return Err(io::Error::other("forwarding completion publication unresolved"));
                        }
                        Reply::Captured { evidence: exchange }
                    }
                }
                Operation::Start { id, mut launch } | Operation::StartEmulator { id, mut launch } | Operation::StartAdbServer { id, mut launch, .. } => {
                    let id = id.as_str().to_owned();
                    if identities.contains(&id) || attempted.len() >= 64 || launch.deadline_ns > config.deadline_ns
                        || server_port.is_some_and(|port| port == 0 || port == 5037 || config.ports.contains(&port)
                            || retirement.adb_server.is_some() || !attempted.is_empty()) {
                        Reply::Failed { error: "resource identity reused, capacity exceeded or lifetime exceeds group".into() }
                    } else {
                        let mut emulator_ports = Vec::new();
                        if start_emulator {
                            let (name, avd) = prepared_avd.as_ref().ok_or_else(|| io::Error::other("emulator requires its original prepared AVD"))?;
                            let server = retirement.adb_server.as_ref().filter(|server| active.contains_key(server.id.as_str()))
                                .ok_or_else(|| io::Error::other("emulator requires its live private ADB server"))?;
                            if volume.is_none() || active[server.id.as_str()].process.has_exited()? {
                                return Err(io::Error::other("emulator requires live mounted scratch and private server"));
                            }
                            launch = capture::confined_emulator(&launch, &disks.as_ref().expect("scratch acquired").path,
                                name, avd, &config.ports, server.port, &target)?;
                            for port in &config.ports {
                                emulator_ports.push(std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, *port))?);
                            }
                        }
                        if let Some(port) = server_port {
                            // Refuse an existing listener; no saved PID grants authority over it.
                            let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
                            if confine_server {
                                let root = &disks.as_ref().expect("scratch acquired").path;
                                let (mut keygen, server) = crate::emulator_confinement::adb_server(&capture::command(&launch)?, root, port)?;
                                let evidence = directory.join("adb-key-generation");
                                let end = Instant::now() + Duration::from_nanos(launch.deadline_ns.saturating_sub(monotonic_ns()?));
                                let result = crate::process::capture_recorded(&mut keygen, end, &revoked,
                                    capture::remaining_output_limit(65536, captured_bytes)?, &evidence)?;
                                captured_bytes += result.stdout.len() + result.stderr.len();
                                if let Some(error) = result.cleanup_failure.as_ref().or(result.evidence_failure.as_ref()) {
                                    retirement.errors.push(error.clone());
                                }
                                let path = evidence.join("exchange.json");
                                let bytes = std::fs::read(&path)?;
                                retirement.helper_evidence.insert(format!("{id}/keygen"), HelperEvidence {
                                    path, sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
                                });
                                if let Some(error) = result.failure.or(result.cleanup_failure).or(result.evidence_failure) {
                                    return Err(io::Error::other(format!("private ADB key generation failed: {error}")));
                                }
                                launch = capture::launch(&server, launch.deadline_ns)?;
                            }
                            let owner = AdbServer { id: RequestId::new(&id).map_err(io::Error::other)?, port };
                            save_inherited(Class::Ordinary, &directory.join("adb-server.json"), &owner)?;
                            retirement.adb_server = Some(owner);
                            drop(reservation);
                        }
                        identities.insert(id.clone());
                        attempted.push(id.clone());
                        let mut command = Command::new(&config.resource_executable); command.args(&config.resource_arguments);
                        // Preserve the attempt in cleanup's proof inventory even if readiness fails.
                        drop(emulator_ports);
                        let resource = ResourceProcess::start(&mut command, &launch, &directory.join("resources").join(&id), Instant::now() + Duration::from_secs(5))?;
                        let process = resource.process.clone(); active.insert(id, resource);
                        Reply::Started { process }
                    }
                }
                Operation::Stop { id } => {
                    let id = id.as_str();
                    if retirement.adb_server.as_ref().is_some_and(|server| server.id.as_str() == id) {
                        Reply::Failed { error: "owned ADB server retires with its group after forwarding".into() }
                    } else if let Some(resource) = active.remove(id) {
                        let result = resource.stop(Instant::now() + Duration::from_secs(8))?;
                        save_inherited(Class::Retirement, &directory.join("resources").join(id).join("group-retirement.json"), &result)?;
                        if let Some(error) = &result.retirement_error { return Err(io::Error::other(format!("resource stop unresolved: {error}"))); }
                        Reply::Stopped { result }
                    } else { Reply::Failed { error: "resource is not actively owned".into() } }
                }
            };
            save_inherited(Class::Ordinary, &directory.join("replies").join(format!("{number}.json")), &reply)?;
        }
        Err(io::Error::other("resource group command capacity exhausted"))
    })();
    retirement.operation_error = work.err().map(|error| error.to_string());
    // Remove confirmed host listeners while their guest transport still exists.
    let forwarding_end = Instant::now() + Duration::from_secs(10);
    for forward in forwards.values_mut() { forward.before_target_stop(forwarding_end); }
    // Keep the owned server through best-effort removal and target retirement.
    let server = retirement.adb_server.as_ref().and_then(|owner| active.remove(owner.id.as_str()));
    // Revoke target children together; the explicitly owned server is retired below.
    for resource in active.values_mut() { resource.revoke(); }
    let end = Instant::now() + Duration::from_secs(10);
    for (id, resource) in active {
        if let Err(error) = resource.stop(end) { retirement.errors.push(format!("{id}: {error}")); }
    }
    // Retire runners first; only the live creator can then delete its Simulator. Neither the
    // Simulator's local success nor an acquisition error can release the group's host fence.
    if let Some(mut simulator) = simulator { retirement.simulator = Some(simulator.stop()); }
    if let Some(result) = &retirement.simulator && !result.passed {
        retirement.errors.push(format!("Simulator retirement unresolved: {:?}", result.errors));
    }
    for (id, proof) in &retirement.helper_evidence {
        match std::fs::read(&proof.path) {
            Ok(bytes) if frontcap_core::canonical::sha256_bytes_hex(&bytes) == proof.sha256
                && crate::process::validate_exchange_files(&proof.path).is_ok() => (),
            _ => retirement.errors.push(format!("{id}: helper evidence changed before group retirement")),
        }
    }
    let end = Instant::now() + Duration::from_secs(10);
    if let Some(server) = server && let Err(error) = server.stop(end) {
        retirement.errors.push(format!("ADB server retirement: {error}"));
    }
    for id in attempted {
        let root = directory.join("resources").join(&id);
        let result = (|| -> io::Result<ResultRecord> {
            let lock = File::options().read(true).write(true).open(root.join("owner.lock"))?;
            loop {
                match lock.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => return Err(io::Error::other(error)),
                }
            }
            load(&root.join("result.json"))?.ok_or_else(|| io::Error::other("resource has no retirement proof"))
        })();
        match result {
            Ok(result) => {
                if let Some(error) = &result.retirement_error { retirement.errors.push(format!("{id}: {error}")); }
                retirement.resources.insert(id, result);
            }
            Err(error) => retirement.errors.push(format!("{id}: {error}")),
        }
    }
    // Only the original owned child's successful retirement can replace an ADB inventory query.
    let retired_server_port = retirement.adb_server.as_ref().and_then(|owner| {
        retirement.resources.get(owner.id.as_str()).filter(|proof|
            proof.process.is_some() && proof.retirement_error.is_none()).map(|_| owner.port)
    });
    let forwarding_end = Instant::now() + Duration::from_secs(10);
    for (id, mut forward) in forwards {
        let result = forward.finish(forwarding_end, retired_server_port);
        if !result.passed { retirement.errors.push(format!("{id}: forwarding retirement unresolved: {:?}", result.errors)); }
        retirement.forwarding.insert(id, result);
    }
    for port in config.ports.iter().copied().chain(retirement.adb_server.as_ref().map(|server| server.port)) {
        match TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(250)) {
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => (),
            result => retirement.errors.push(format!("resource group port {port} absence unconfirmed: {result:?}")),
        }
    }
    if disks.is_none() {
        match target.join("emulator-temporary").symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => retirement.errors.push(format!("scratch absence unconfirmed: {error}")),
            Ok(_) => retirement.errors.push("scratch acquisition unresolved".into()),
        }
    }
    if retirement.errors.is_empty() && let Some(disks) = disks && let Err(error) = disks.retire() {
        retirement.errors.push(error.to_string());
    }
    if volume_attempted && volume.is_none() {
        retirement.errors.push("scratch volume acquisition unresolved; inspect original worker evidence".into());
    }
    if retirement.errors.is_empty() && let Some(volume) = volume {
        let result = volume.retire(Instant::now() + Duration::from_secs(20)).and_then(|evidence| {
            crate::storage_volume::inspect(&config.scratch_volume_directory(directory),
                config.scratch_volume.as_ref().expect("started volume configuration"), &evidence)?;
            Ok(evidence)
        });
        match result {
            Ok(evidence) => retirement.scratch_volume = Some(evidence),
            Err(error) => retirement.errors.push(format!(
                "scratch volume retirement unresolved: {error}; original evidence: {}",
                config.scratch_volume_directory(directory).display())),
        }
    }
    // SDK bundle owners are registered before spawn. Cleanup remains authoritative even
    // after their client dies, but unresolved volumes cannot release the host fence.
    if let Err(error) = crate::xctest::bundle_volume::inspect_group(&target, Instant::now() + Duration::from_secs(5)) {
        retirement.errors.push(format!("XCTest bundle retirement unresolved: {error}"));
    }
    retirement.passed = retirement.errors.is_empty();
    save_inherited(Class::Retirement, &directory.join("cleanup.json"), &retirement)?;
    if retirement.passed
        && let Err(error) = save_inherited(Class::Retirement, &host_record, &HostSession { directory: target, retired: true }) {
        retirement.passed = false; retirement.errors.push(error.to_string());
    }
    save_inherited(Class::Retirement, &directory.join("retirement.json"), &retirement)
}

#[cfg(test)]
#[path = "resource_group_tests.rs"]
pub(crate) mod tests;

#[path = "resource_group_capture.rs"]
pub(crate) mod capture;

#[path = "resource_group_forward.rs"]
mod forwarding;
pub use forwarding::{ForwardDisposition, ForwardRegistration, ForwardRetirement};

#[path = "resource_group_inspection.rs"]
pub(crate) mod inspection;

#[cfg(test)]
#[path = "resource_group_budget_tests.rs"]
mod budget_tests;
