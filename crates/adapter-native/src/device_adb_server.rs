//! One foreground ADB server under the group's original scratch and retirement authority.
use super::*;
use frontcap_core::{ids::RequestId, write_budget::{Class, save_inherited}};
use std::os::unix::fs::MetadataExt;

impl Device {
    pub(super) fn apply_adb_endpoint(&self, command: &mut Command) {
        if let Some(port) = self.config.adb_server_port {
            // Numeric loopback selects ADB's no-auto-start route after server loss.
            command.env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{port}"));
        }
    }

    pub(super) fn confined_adb_command(&self, command: &Command) -> io::Result<Option<Command>> {
        if command.get_program() == self.config.adb.as_os_str()
            && let Some(root) = &self.adb_write_root {
            return crate::emulator_confinement::adb_client(command, root).map(Some);
        }
        Ok(None)
    }

    pub(super) fn start_adb_server(&mut self, scratch: &Path, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<()> {
        let port = self.config.adb_server_port.expect("private server selected");
        if self.adb_server.is_some() { return Err(io::Error::other("ADB server already acquired")); }
        let root = scratch.canonicalize()?;
        let metadata = std::fs::metadata(&root)?;
        if !metadata.is_dir() || metadata.dev() == std::fs::metadata(&self.directory)?.dev() {
            return Err(io::Error::other("ADB server requires independent owned scratch filesystem"));
        }
        let home = root.join("adb-home"); let temporary = root.join("adb-temporary");
        std::fs::create_dir(&home)?; std::fs::create_dir(&temporary)?;
        let policy = crate::emulator_confinement::policy(&root)?;
        let mut command = Command::new(&self.config.adb);
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| key.starts_with("ADB_") || key.starts_with("ANDROID_")) {
                command.env_remove(key);
            }
        }
        for key in ["HOME", "ANDROID_USER_HOME"] { command.env(key, &home); }
        for key in ["TMPDIR", "TMP", "TEMP"] { command.env(key, &temporary); }
        for key in ["ADB_USB", "ADB_MDNS", "ADB_LOCAL_TRANSPORT_MAX_PORT"] { command.env(key, "0"); }
        #[derive(Serialize)]
        struct Manifest<'a> {
            root: &'a Path, device: u64, inode: u64, port: u16,
            home: &'a Path, temporary: &'a Path, policy: &'a str,
            scope: &'static str,
        }
        save_inherited(Class::Ordinary, &self.directory.join("adb-write-root.json"), &Manifest {
            root: &root, device: metadata.dev(), inode: metadata.ino(), port,
            home: &home, temporary: &temporary, policy: &policy,
            scope: "owned ADB server, key preparation and device client direct writes; system-service and caller roots excluded",
        })?;
        self.adb_write_root = Some(root.clone());
        // ADB can listen and return an empty inventory even when implicit key generation
        // fails with ENOSPC. Require the explicit keygen command to complete before serving.
        let keys = home.join(".android");
        std::fs::create_dir(&keys)?;
        let mut keygen = crate::emulator_confinement::confine_command(&command, &policy);
        keygen.arg("keygen").arg(keys.join("adbkey"));
        let exchange = self.capture_owned(&mut keygen, deadline, cancelled, 65536,
            &self.directory.join("adb-key-generation"))?;
        if let Some(error) = exchange.failure.or(exchange.cleanup_failure).or(exchange.evidence_failure) {
            return Err(io::Error::other(format!("private ADB key generation failed: {error}")));
        }
        command.args(["-L", &format!("tcp:{port}"), "server", "nodaemon"]);
        let command = crate::emulator_confinement::confine_command(&command, &policy);
        let shared = self.supervision.as_ref().expect("validated supervised server");
        let process = supervision::with_group(shared, |group| group.start_adb_server(
            RequestId::new("adb-server").map_err(io::Error::other)?, port, &command, deadline, cancelled))?;
        self.adb_server = Some(process.clone());
        let ready_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
        for attempt in 0..100 {
            check_budget(ready_deadline, cancelled)?;
            if process.has_exited()? { return Err(io::Error::other("owned ADB server exited before readiness")); }
            let mut query = Command::new(&self.config.adb); query.args(["devices", "-l"]);
            let exchange = self.capture_owned(&mut query, ready_deadline, cancelled, 65536,
                &self.directory.join(format!("adb-server-readiness-{attempt}")))?;
            if let Some(error) = exchange.cleanup_failure.or(exchange.evidence_failure) {
                return Err(io::Error::other(error));
            }
            if exchange.failure.is_none() {
                let output = String::from_utf8(exchange.stdout).map_err(io::Error::other)?;
                if output.trim() != "List of devices attached" || process.has_exited()? {
                    return Err(io::Error::other("private ADB readiness has unexpected transports or lost owner"));
                }
                save_inherited(Class::Ordinary, &self.directory.join("adb-server-ready.json"), &(port, process))?;
                return Ok(());
            }
            if exchange.exit_code.is_none() { return Err(io::Error::other("ADB server readiness command unresolved")); }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(io::Error::other("owned ADB server readiness budget exhausted"))
    }
}
