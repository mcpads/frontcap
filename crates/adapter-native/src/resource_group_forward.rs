//! Bind cleanup authority to a live emulator and the original supervised ADB exchange.
use super::*;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRegistration {
    pub adb: PathBuf,
    /// Explicit loopback server; numeric host prevents ADB client auto-start after loss.
    /// None retains the existing shared-server route. This does not grant server ownership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    /// Original group-owned scratch; private clients may write only beneath this root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_root: Option<PathBuf>,
    pub emulator: ProcessIdentity,
    pub serial: String,
    /// Bare WebView socket name, or a canonical tcp:1..65535 guest endpoint.
    pub socket: String,
    pub port: u16,
    /// Planned original bind-command directory, relative to the frozen evidence root.
    pub bind_evidence: PathBuf,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardDisposition { Absent, ForeignMappingPreserved, RemovedBeforeSuccessor, OwnedServerRetired, Unresolved }
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRetirement {
    pub port: u16,
    pub binding_confirmed: bool,
    pub removal_confirmed: bool,
    pub passed: bool,
    pub disposition: ForwardDisposition,
    pub errors: Vec<String>,
    pub diagnostics: Vec<String>,
}
enum Mapping { Absent, Exact, Foreign }
pub(super) struct Forward {
    pub registration: ForwardRegistration,
    directory: PathBuf,
    bind_evidence: PathBuf,
    confirmed: bool,
    pub removed: bool,
    pub superseded: bool,
    removal_attempted: bool,
    command: u64,
    errors: Vec<String>,
    diagnostics: Vec<String>,
}
impl ForwardRegistration {
    fn remote_endpoint(&self) -> Option<String> {
        if let Some(port) = self.socket.strip_prefix("tcp:") {
            return port.parse::<u16>().ok().filter(|value| *value > 0 && value.to_string() == port)
                .map(|value| format!("tcp:{value}"));
        }
        self.socket.strip_prefix("webview_devtools_remote_")
            .filter(|pid| pid.parse::<u32>().is_ok_and(|pid| pid > 0))
            .map(|_| format!("localabstract:{}", self.socket))
    }
}
impl Forward {
    pub fn new(registration: ForwardRegistration, root: &Path, directory: &Path) -> io::Result<Self> {
        if !registration.adb.is_absolute() || registration.port == 0
            || registration.server_port == Some(0)
            || registration.remote_endpoint().is_none() {
            return Err(io::Error::other("invalid owned Android forwarding registration"));
        }
        let bind_evidence = capture::evidence_path(root, &registration.bind_evidence)?;
        if bind_evidence.exists() { return Err(io::Error::other("forward bind evidence already exists")); }
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(directory)?;
        save_inherited(Class::Ordinary, &directory.join("intent.json"), &registration)?;
        Ok(Self { registration, directory: directory.to_owned(), bind_evidence, confirmed: false,
            removed: false, superseded: false, removal_attempted: false, command: 0,
            errors: Vec::new(), diagnostics: Vec::new() })
    }
    fn arguments(&self, bind: bool) -> Vec<String> {
        let mut args = vec!["-s".into(), self.registration.serial.clone(), "forward".into(),
            if bind { "--no-rebind" } else { "--remove" }.into(), format!("tcp:{}", self.registration.port)];
        if bind { args.push(self.registration.remote_endpoint().expect("validated forwarding endpoint")); }
        args
    }
    fn command(&self, args: &[&str]) -> io::Result<Command> {
        let mut command = Command::new(&self.registration.adb);
        if let Some(port) = self.registration.server_port {
            command.env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{port}"));
        }
        command.args(args);
        if let Some(root) = &self.registration.write_root {
            return crate::emulator_confinement::adb_client(&command, root);
        }
        Ok(command)
    }
    fn matches(&self, command: &Command, bind: bool) -> bool {
        let arguments = self.arguments(bind);
        let args = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        let Ok(expected) = self.command(&args) else { return false; };
        command.get_program() == expected.get_program()
            && command.get_args().eq(expected.get_args())
            && expected.get_envs().all(|entry| command.get_envs().any(|actual| actual == entry))
    }
    pub fn before_capture(&self, command: &Command, evidence: &Path) -> io::Result<()> {
        if evidence == self.bind_evidence && !self.matches(command, true) {
            return Err(io::Error::other("forward bind differs from registered helper"));
        }
        Ok(())
    }
    pub fn after_capture<R>(&mut self, command: &Command, evidence: &Path, result: &crate::process::Exchange<R>) -> io::Result<()> {
        if self.superseded { return Ok(()); }
        // A captured zero exit proves ADB acceptance even if caller cancellation won the reply race.
        // It grants cleanup authority, not a successful business-operation outcome.
        let completed = result.exit_code == Some(0) && result.cleanup_failure.is_none();
        if evidence == self.bind_evidence {
            self.confirmed = completed;
            save_inherited(Class::Ordinary, &self.directory.join("bind-completion.json"), &(evidence, completed, &result.failure))?;
        } else if self.confirmed && self.matches(command, false) {
            self.removal_attempted = true;
            self.removed |= completed;
            save_inherited(Class::Ordinary, &self.directory.join("remove-completion.json"), &(evidence, completed, &result.failure))?;
        }
        Ok(())
    }
    fn adb(&mut self, args: &[&str], deadline: Instant) -> io::Result<String> {
        let _writes = crate::process::RetirementWrites::enter();
        self.command += 1;
        let arguments = ["-s", self.registration.serial.as_str()].into_iter().chain(args.iter().copied()).collect::<Vec<_>>();
        let mut command = self.command(&arguments)?;
        let result = crate::process::capture_recorded(&mut command, deadline, &AtomicBool::new(false),
            65536, &self.directory.join(format!("cleanup-{}", self.command)))?;
        if let Some(error) = result.cleanup_failure.as_ref().or(result.evidence_failure.as_ref()) {
            self.errors.push(error.clone());
        }
        if let Some(error) = result.failure.or(result.cleanup_failure).or(result.evidence_failure) {
            return Err(io::Error::other(error));
        }
        String::from_utf8(result.stdout).map_err(io::Error::other)
    }
    fn mapping(&mut self, deadline: Instant) -> io::Result<Mapping> {
        let inventory = self.adb(&["forward", "--list"], deadline)?;
        let local = format!("tcp:{}", self.registration.port);
        let remote = self.registration.remote_endpoint().expect("validated forwarding endpoint");
        let mut count = 0;
        let mut exact = false;
        for line in inventory.lines().filter(|line| !line.trim().is_empty()) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 3 { return Err(io::Error::other("malformed ADB forwarding inventory")); }
            if fields[1] == local {
                count += 1;
                exact = fields[0] == self.registration.serial && fields[2] == remote;
            }
        }
        match count {
            0 => Ok(Mapping::Absent),
            1 if exact => Ok(Mapping::Exact),
            1 => Ok(Mapping::Foreign),
            _ => Err(io::Error::other("ambiguous ADB forwarding inventory")),
        }
    }
    /// Best effort while the guest still exists. Final read-only verification follows child retirement.
    pub fn before_target_stop(&mut self, deadline: Instant) {
        if self.superseded || self.removed || self.removal_attempted || !self.confirmed { return; }
        let attempt = (|| -> io::Result<()> {
            if matches!(self.mapping(deadline)?, Mapping::Exact) {
                self.removal_attempted = true;
                save_inherited(Class::Retirement, &self.directory.join("cleanup-remove-intent.json"), &self.registration.port)?;
                self.adb(&["forward", "--remove", &format!("tcp:{}", self.registration.port)], deadline)?;
                self.removed = true;
            }
            Ok(())
        })();
        if let Err(error) = attempt { self.diagnostics.push(error.to_string()); }
    }
    fn listener_absent(&self) -> io::Result<()> {
        match TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.registration.port)), Duration::from_millis(250)) {
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(()),
            result => Err(io::Error::other(format!("forward listener absence unconfirmed: {result:?}"))),
        }
    }
    pub fn finish(&mut self, deadline: Instant, retired_server_port: Option<u16>) -> ForwardRetirement {
        let disposition = if self.superseded && self.removed {
            Ok(ForwardDisposition::RemovedBeforeSuccessor)
        } else if retired_server_port.is_some() && self.registration.server_port == retired_server_port {
            self.listener_absent().map(|()| ForwardDisposition::OwnedServerRetired)
        } else {
            self.mapping(deadline).and_then(|mapping| match mapping {
                Mapping::Foreign => Ok(ForwardDisposition::ForeignMappingPreserved),
                Mapping::Exact => Err(io::Error::other("registered forward remains; no confirmed removal")),
                Mapping::Absent => self.listener_absent().map(|()| ForwardDisposition::Absent),
            })
        };
        let disposition = match disposition { Ok(value) => value, Err(error) => {
            self.errors.push(error.to_string()); ForwardDisposition::Unresolved
        }};
        let mut result = ForwardRetirement { port: self.registration.port, binding_confirmed: self.confirmed,
            removal_confirmed: self.removed, passed: self.errors.is_empty(), disposition,
            errors: self.errors.clone(), diagnostics: self.diagnostics.clone() };
        if let Err(error) = save_inherited(Class::Retirement, &self.directory.join("retirement.json"), &result) {
            result.passed = false; result.errors.push(error.to_string());
        }
        result
    }
}
