//! External shell input producer lifetime, owned by the acquired emulator.
use super::*;

pub use frontcap_core::native::android_webview::ContactDriver;
use frontcap_core::native::android_webview::ContactConsumer;
/// Admission limits fixed for the lifetime of one producer.
pub struct ContactDriverLimits {
    pub lifetime: Duration,
    pub request_capacity: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    #[serde(default)]
    short_tap: bool,
    status: String,
    generation: String,
    pid: u32,
    uid: u32,
    request_capacity: u64,
}
impl Device {
    /// Start one external producer; acquisition failure retires this target, never adopts a journal.
    pub fn start_contact_driver(&mut self, jar: &Path, sha256: &str,
        deadline: Instant, cancelled: &AtomicBool) -> Result<ContactDriver, AcquisitionError> {
        self.start_contact_driver_for(jar, sha256, Duration::from_secs(300), deadline, cancelled)
    }
    /// Explicit bounded producer lifetime for longer journeys; does not extend request capacity.
    pub fn start_contact_driver_for(&mut self, jar:&Path, sha256:&str, lifetime:Duration,
        deadline:Instant,cancelled:&AtomicBool)->Result<ContactDriver,AcquisitionError> {
        self.start_contact_driver_bounded(jar, sha256, ContactDriverLimits { lifetime, request_capacity: 24 }, deadline, cancelled)
    }
    /// Start with explicit bounded lifetime and capacity; neither can be extended in place.
    pub fn start_contact_driver_bounded(&mut self, jar: &Path, sha256: &str, limits: ContactDriverLimits,
        deadline: Instant, cancelled: &AtomicBool) -> Result<ContactDriver, AcquisitionError> {
        let work = self.start_contact_driver_once(jar, sha256, limits, deadline, cancelled);
        work.map_err(|error| AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) })
    }
    fn start_contact_driver_once(&mut self, jar: &Path, sha256: &str, limits: ContactDriverLimits,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<ContactDriver> {
        check_budget(deadline, cancelled)?;
        let ContactDriverLimits { lifetime, request_capacity } = limits;
        if !(1..=frontcap_core::native::contacts::MAX_REQUEST_CAPACITY).contains(&request_capacity) {
            return Err(io::Error::other("contact capacity must be 1..4096 requests"));
        }
        if lifetime.is_zero() || lifetime>Duration::from_secs(900) || lifetime.as_millis()==0 {
            return Err(io::Error::other("contact lifetime must be 1..900000 milliseconds"));
        }
        let lifetime_ms=lifetime.as_millis() as u64;
        if self.retired.is_some() || self.contact_process.is_some() || !self.unretired_commands.is_empty() {
            return Err(io::Error::other("contact driver already attempted or owner unavailable"));
        }
        let process = self.app_process.clone().ok_or_else(|| io::Error::other("external consumer unavailable"))?;
        self.verify_app_process(&process, deadline, cancelled)?;
        let consumer = match self.webview_debugger.clone() {
            Some(debugger) => ContactConsumer::WebView(debugger),
            None => ContactConsumer::Host(process.clone()),
        };
        if !jar.is_absolute() || std::fs::metadata(jar)?.len() > 8 * 1024 * 1024 {
            return Err(io::Error::other("invalid contact driver artifact"));
        }
        let bytes = std::fs::read(jar)?;
        if frontcap_core::canonical::sha256_bytes_hex(&bytes) != sha256 {
            return Err(io::Error::other("contact driver artifact hash differs"));
        }
        let directory = self.host_evidence_directory().join("contact-driver");
        std::fs::create_dir(&directory)?;
        let frozen = directory.join("driver.jar");
        frontcap_core::state::save_atomic_bytes(&frozen, &bytes)?;
        let token = frontcap_core::canonical::sha256_bytes_hex(directory.canonicalize()?.as_os_str().as_encoded_bytes());
        let guest = format!("/data/local/tmp/frontcap-input-{}", &token[..24]);
        let guest_jar = format!("{guest}/driver.jar");
        let journal = format!("{guest}/journal");
        save_atomic(&directory.join("intent.json"), &(&consumer, sha256, &guest, lifetime_ms, request_capacity))?;
        self.adb(&["shell", "mkdir", &guest], deadline, cancelled, false)?;
        self.adb(&["push", frozen.to_str().ok_or_else(||io::Error::other("non-UTF8 driver path"))?, &guest_jar], deadline, cancelled, false)?;
        let digest = self.adb(&["shell", "sha256sum", &guest_jar], deadline, cancelled, false)?.unwrap();
        if digest.split_whitespace().next() != Some(sha256) {
            return Err(io::Error::other("guest contact driver hash differs"));
        }
        let mut command = Command::new(&self.config.adb);
        command.args(["-s", &process.serial, "shell", "-T", &format!(
            "CLASSPATH={guest_jar} app_process /system/bin dev.frontcap.input.TouchInput {journal} {lifetime_ms} {request_capacity}")])
            .stdin(Stdio::null())
            .process_group(0);
        if self.supervision.is_none() {
            command.stdout(crate::diagnostic_log::create_output(&directory.join("stdout.log"))?)
                .stderr(crate::diagnostic_log::create_output(&directory.join("stderr.log"))?);
        }
        self.contact_process = Some(self.spawn_owned(&mut command, "input", &directory, deadline, cancelled)?);
        let pid = self.contact_process.as_ref().unwrap().id();
        let identity = ProcessIdentity::of(pid)?.ok_or_else(||io::Error::other("contact transport exited before publication"))?;
        save_atomic(&directory.join("process.json"), &identity)?;
        let raw = loop {
            check_budget(deadline, cancelled)?;
            if self.contact_process.as_mut().unwrap().exited()? {
                return Err(io::Error::other("contact transport exited before ready"));
            }
            if let Some(raw) = self.adb(&["shell", "cat", &format!("{journal}/ready.json")], deadline, cancelled, true)? {
                break raw;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let ready: Ready = serde_json::from_str(&raw)?;
        if ready.status != "ready" || ready.uid != 2000 || ready.pid == 0 || ready.request_capacity != request_capacity
            || !frontcap_core::native::request::canonical_uuid(&ready.generation) {
            return Err(io::Error::other("invalid contact producer identity"));
        }
        let stat = self.adb(&["shell", "cat", &format!("/proc/{}/stat", ready.pid)], deadline, cancelled, false)?.unwrap();
        let owner = ContactDriver { short_tap: ready.short_tap, generation: ready.generation, lifetime_ms, request_capacity, pid: ready.pid,
            process_start_ticks: crate::android_webview::start_ticks(&stat, ready.pid)?,
            guest_directory: journal, jar_sha256: sha256.into(), consumer };
        self.verify_app_process(&owner.consumer.process(), deadline, cancelled)?;
        save_atomic(&directory.join("owner.json"), &owner)?;
        self.contact_state=Some(super::contact_input::Session::new(owner.clone()));
        Ok(owner)
    }
}
