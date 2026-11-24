//! Private volume-worker authority held by a live resource group, never reconstructed from files.
use frontcap_core::state::load;
use serde::{Deserialize, Serialize};
use std::{io::{self, Write}, path::{Path, PathBuf}, process::{Child, ChildStdin, Command, Stdio}, time::{Duration, Instant}};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub python: PathBuf,
    pub worker: PathBuf,
    pub size_mib: u32,
    pub reserve_mib: u32,
}
impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if !self.python.is_absolute() || !self.worker.is_absolute()
            || !(256..=65536).contains(&self.size_mib)
            || !(1..=64.min(self.size_mib / 4)).contains(&self.reserve_mib) {
            return Err(io::Error::other("invalid owned volume worker configuration"));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    mount: PathBuf,
    image: PathBuf,
    size_mib: u32,
    reserve_mib: u32,
    owner_pid: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retirement {
    passed: bool,
    detached: bool,
    removed: bool,
    #[serde(default)]
    retained_image: Option<RetainedImage>,
}

/// Content identity of a detached evidence image; never a device or cleanup capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedImage {
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gzip: Option<CompressedImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressedImage { pub bytes: u64, pub sha256: String }

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetirementEvidence {
    worker: frontcap_core::process::ProcessIdentity,
    ready_sha256: String,
    retirement_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_image: Option<RetainedImage>,
}

pub(crate) fn inspect(directory: &Path, config: &Config, evidence: &RetirementEvidence) -> io::Result<()> {
    inspect_retirement(directory, config, evidence, true, None)
}

/// Read-only verification of retained-image bytes and original worker receipts. This never mounts,
/// adopts or removes the image. Hashing is explicit and proportional to the declared image capacity.
pub fn inspect_retained(directory: &Path, config: &Config, evidence: &RetirementEvidence) -> io::Result<()> {
    inspect_retirement(directory, config, evidence, false, None)
}

/// Derive read-only content evidence from an original retained receipt. No cleanup/Stop authority.
pub fn read_retained_receipt(directory: &Path, config: &Config) -> io::Result<RetirementEvidence> {
    let worker = load(&directory.join("worker-process.json"))?
        .ok_or_else(|| io::Error::other("original volume worker identity missing"))?;
    let ready = std::fs::read(directory.join("worker/ready.json"))?;
    let bytes = std::fs::read(directory.join("worker/retirement.json"))?;
    let result: Retirement = serde_json::from_slice(&bytes)?;
    let evidence = RetirementEvidence { worker, ready_sha256: frontcap_core::canonical::sha256_bytes_hex(&ready),
        retirement_sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes), retained_image: result.retained_image };
    inspect_retained(directory, config, &evidence)?;
    Ok(evidence)
}

pub(crate) fn inspect_retained_until(directory: &Path, config: &Config, evidence: &RetirementEvidence, deadline: Instant) -> io::Result<()> {
    inspect_retirement(directory, config, evidence, false, Some(deadline))
}

fn inspect_retirement(directory: &Path, config: &Config, evidence: &RetirementEvidence, remove: bool, deadline: Option<Instant>) -> io::Result<()> {
    config.validate()?;
    let process: frontcap_core::process::ProcessIdentity = load(&directory.join("worker-process.json"))?
        .ok_or_else(|| io::Error::other("original volume worker identity missing"))?;
    let ready_bytes = std::fs::read(directory.join("worker/ready.json"))?;
    let retirement_bytes = std::fs::read(directory.join("worker/retirement.json"))?;
    if process != evidence.worker
        || frontcap_core::canonical::sha256_bytes_hex(&ready_bytes) != evidence.ready_sha256
        || frontcap_core::canonical::sha256_bytes_hex(&retirement_bytes) != evidence.retirement_sha256 {
        return Err(io::Error::other("original volume retirement evidence changed"));
    }
    let ready: Ready = serde_json::from_slice(&ready_bytes)?;
    let result: Retirement = serde_json::from_slice(&retirement_bytes)?;
    if ready.owner_pid != process.pid || ready.mount != directory.join("worker/volume/mount")
        || ready.image != directory.join("worker/volume/bounded.dmg")
        || ready.size_mib != config.size_mib || ready.reserve_mib != config.reserve_mib
        || !result.passed || !result.detached || result.removed != remove
        || result.retained_image != evidence.retained_image {
        return Err(io::Error::other("volume retirement differs from original invocation"));
    }
    verify_image(&directory.join("worker/volume/bounded.dmg"),
        u64::from(config.size_mib) * 1024 * 1024, remove, result.retained_image.as_ref(), deadline)?;
    // Historical evidence only: do not probe/detach a device or signal a saved PID.
    Ok(())
}

fn verify_image(path: &Path, capacity: u64, remove: bool, expected: Option<&RetainedImage>, deadline: Option<Instant>) -> io::Result<()> {
    if remove {
        return if expected.is_none() { Ok(()) } else { Err(io::Error::other("removed volume has retained image proof")) };
    }
    let expected = expected.ok_or_else(|| io::Error::other("retained volume image proof missing"))?;
    let digest_valid = |s: &str| s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    if expected.bytes != capacity || !digest_valid(&expected.sha256) {
        return Err(io::Error::other("retained volume image proof differs from capacity"));
    }
    let (stored_path, stored_bytes) = if let Some(gzip) = &expected.gzip {
        if gzip.bytes == 0 || gzip.bytes > capacity + capacity / 100 + 1024 * 1024 || !digest_valid(&gzip.sha256) {
            return Err(io::Error::other("compressed image exceeds storage bound or has invalid digest"));
        }
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => return Err(io::Error::other("raw image retirement unconfirmed")),
        }
        (path.with_extension("dmg.gz"), gzip.bytes)
    } else { (path.to_owned(), capacity) };
    use std::{io::{Read, Seek, BufReader}, os::unix::fs::{MetadataExt, OpenOptionsExt}};
    use sha2::{Digest, Sha256};
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&stored_path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() != stored_bytes || before.nlink() != 1 {
        return Err(io::Error::other("retained volume image type or size differs"));
    }
    let actual = if let Some(gzip) = &expected.gzip {
        // Our encoder emits no optional header fields. Reject them before the decoder can
        // accumulate an unbounded filename/comment from a damaged archive.
        let mut header = [0_u8; 10]; file.read_exact(&mut header)?;
        if header[..4] != [0x1f, 0x8b, 8, 0] { return Err(io::Error::other("unexpected compressed image header")); }
        file.rewind()?;
        let mut encoded = HashingReader { input: &mut file, hash: Sha256::new() };
        let mut buffered = BufReader::new(&mut encoded);
        let decoded = {
            let mut decoder = flate2::bufread::GzDecoder::new(&mut buffered);
            image_digest(&mut decoder, capacity, deadline)?
        };
        // One gzip member, with no trailing payload or unverified concatenated members.
        if buffered.read(&mut [0_u8; 1])? != 0 {
            return Err(io::Error::other("compressed image has trailing bytes"));
        }
        let encoded_digest: String = encoded.hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if encoded_digest != gzip.sha256 { return Err(io::Error::other("compressed image digest differs")); }
        decoded
    } else { image_digest(&mut file, capacity, deadline)? };
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(&stored_path)?;
    let stamp = |m: &std::fs::Metadata| (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec());
    if stamp(&before) != stamp(&after) || !current.is_file() || stamp(&after) != stamp(&current)
        || actual != expected.sha256 {
        return Err(io::Error::other("retained volume image changed or digest differs"));
    }
    Ok(())
}

struct HashingReader<R> { input: R, hash: sha2::Sha256 }
impl<R: std::io::Read> std::io::Read for HashingReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        use sha2::Digest;
        let count = self.input.read(bytes)?;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
}
fn image_digest(input: &mut impl std::io::Read, capacity: u64, deadline: Option<Instant>) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new(); let mut remaining = capacity; let mut buffer = [0_u8; 65536];
    while remaining > 0 {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "retained image verification deadline exceeded"));
        }
        let count = input.read(&mut buffer[..remaining.min(65536) as usize])?;
        if count == 0 { return Err(io::Error::other("retained volume image shortened")); }
        digest.update(&buffer[..count]); remaining -= count as u64;
    }
    if input.read(&mut buffer[..1])? != 0 { return Err(io::Error::other("retained image exceeds declared capacity")); }
    Ok(digest.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Live authority for one fresh volume worker. Dropping the handle preserves uncertain storage.
/// The caller must prove all writers retired before calling either retirement method.
pub struct Worker {
    child: Option<Child>,
    authority: Option<ChildStdin>,
    directory: PathBuf,
    process: Option<frontcap_core::process::ProcessIdentity>,
    ready_sha256: String,
    capacity_bytes: u64,
    pub mount: PathBuf,
}
impl Worker {
    pub fn start(config: &Config, directory: &Path, deadline: Instant) -> io::Result<Self> {
        config.validate()?;
        std::fs::create_dir(directory)?;
        let directory = directory.canonicalize()?;
        let mut child = Command::new(&config.python).arg(&config.worker).arg(directory.join("worker"))
            .args(["--size-mib", &config.size_mib.to_string(), "--reserve-mib", &config.reserve_mib.to_string()])
            .env("PYTHONDONTWRITEBYTECODE", "1").stdin(Stdio::piped()).stdout(Stdio::null())
            .stderr(crate::diagnostic_log::create_output(&directory.join("worker.log"))?).spawn()?;
        let authority = child.stdin.take();
        let mut owner = Self { child: Some(child), authority, mount: directory.join("worker/volume/mount"), directory,
            process: None, ready_sha256: String::new(), capacity_bytes: u64::from(config.size_mib) * 1024 * 1024 };
        let process = frontcap_core::process::ProcessIdentity::of(owner.child.as_ref().unwrap().id())?
            .ok_or_else(|| io::Error::other("volume worker identity absent before authorization"))?;
        frontcap_core::state::save_atomic(&owner.directory.join("worker-process.json"), &process)?;
        owner.process = Some(process);
        owner.authority.as_mut().unwrap().write_all(b"1")?;
        loop {
            let child = owner.child.as_mut().unwrap();
            if child.try_wait()?.is_some() { return Err(io::Error::other("volume worker exited before readiness")); }
            if let Some(ready) = load::<Ready>(&owner.directory.join("worker/ready.json"))? {
                if ready.owner_pid != child.id() || ready.mount != owner.mount
                    || ready.image != owner.directory.join("worker/volume/bounded.dmg")
                    || ready.size_mib != config.size_mib || ready.reserve_mib != config.reserve_mib {
                    return Err(io::Error::other("volume worker readiness differs from its live invocation"));
                }
                owner.ready_sha256 = frontcap_core::canonical::sha256_bytes_hex(
                    &std::fs::read(owner.directory.join("worker/ready.json"))?);
                return Ok(owner);
            }
            if Instant::now() >= deadline { return Err(io::Error::other("volume readiness observation expired; retain original owner evidence")); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Register original live UIKit writers before recorder loss. EOF alone remains insufficient.
    pub fn watch_uikit(&mut self, session: &Path, worker: &frontcap_core::process::ProcessIdentity, retirement_reader: Option<&Path>, deadline: Instant) -> io::Result<()> {
        #[derive(Serialize)] struct Watch<'a> { operation: &'static str, session: &'a Path,
            worker: &'a frontcap_core::process::ProcessIdentity, ready_sha256: String,
            #[serde(skip_serializing_if = "Option::is_none")] retirement_reader: Option<&'a Path> }
        let ready_sha256 = frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(session.join("ready.json"))?);
        let request = serde_json::to_vec(&Watch { operation: "watch_uikit", session, worker, ready_sha256, retirement_reader })?;
        self.watch(request, "watch", deadline)
    }
    pub fn watch_uikit_acquisition(&mut self, session: &Path, worker: &frontcap_core::process::ProcessIdentity, retirement_reader: Option<&Path>, deadline: Instant) -> io::Result<()> {
        #[derive(Serialize)] struct Watch<'a> { operation: &'static str, session: &'a Path,
            worker: &'a frontcap_core::process::ProcessIdentity,
            #[serde(skip_serializing_if = "Option::is_none")] retirement_reader: Option<&'a Path> }
        self.watch(serde_json::to_vec(&Watch { operation: "watch_uikit_acquisition", session, worker, retirement_reader })?,
            "watch-acquisition", deadline)
    }
    fn watch(&mut self, mut request: Vec<u8>, name: &str, deadline: Instant) -> io::Result<()> {
        #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Acknowledged { request_sha256: String }
        #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Rejected { request_sha256: String, error: String }
        request.push(b'\n');
        if request.len() > 4096 { return Err(io::Error::other("UIKit volume watch exceeds message bound")); }
        self.authority.as_mut().ok_or_else(|| io::Error::other("volume authority absent"))?.write_all(&request)?;
        loop {
            if self.child.as_mut().unwrap().try_wait()?.is_some() { return Err(io::Error::other("volume worker exited before UIKit watch acknowledgement")); }
            if let Some(rejected) = load::<Rejected>(&self.directory.join(format!("worker/{name}-rejected.json")))? {
                if rejected.request_sha256 != frontcap_core::canonical::sha256_bytes_hex(&request) {
                    return Err(io::Error::other("UIKit volume watch rejection differs"));
                }
                return Err(io::Error::other(format!("UIKit volume watch rejected: {}", rejected.error)));
            }
            if let Some(ack) = load::<Acknowledged>(&self.directory.join(format!("worker/{name}-ready.json")))? {
                if ack.request_sha256 != frontcap_core::canonical::sha256_bytes_hex(&request) {
                    return Err(io::Error::other("UIKit volume watch acknowledgement differs"));
                }
                return Ok(());
            }
            if Instant::now() >= deadline { return Err(io::Error::other("UIKit volume watch acknowledgement expired")); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Called only after this group's process/port/scratch retirement proofs have passed.
    pub fn retire(self, deadline: Instant) -> io::Result<RetirementEvidence> {
        self.finish(deadline, true, false)
    }
    /// Detach and retain the original image after the caller proves every writer has retired.
    /// The caller must keep the returned evidence and worker directory; no image is adopted later.
    pub fn retire_retaining(self, deadline: Instant) -> io::Result<RetirementEvidence> {
        self.finish(deadline, false, false)
    }
    /// Preserve a verified gzip representation, removing raw storage only after a lossless check.
    pub fn retire_compressed(self, deadline: Instant) -> io::Result<RetirementEvidence> {
        self.finish(deadline, false, true)
    }
    fn finish(mut self, deadline: Instant, remove: bool, compress: bool) -> io::Result<RetirementEvidence> {
        let recorded = load::<frontcap_core::process::ProcessIdentity>(&self.directory.join("worker-process.json"))?;
        if recorded != self.process || frontcap_core::canonical::sha256_bytes_hex(
            &std::fs::read(self.directory.join("worker/ready.json"))?) != self.ready_sha256 {
            return Err(io::Error::other("volume authority evidence changed before retirement"));
        }
        let request = if remove {
            b"{\"operation\":\"retire\",\"writers_retired\":true,\"remove\":true}\n".as_slice()
        } else if compress {
            b"{\"operation\":\"retire\",\"writers_retired\":true,\"remove\":false,\"compression\":\"gzip\"}\n".as_slice()
        } else {
            b"{\"operation\":\"retire\",\"writers_retired\":true,\"remove\":false}\n".as_slice()
        };
        self.authority.as_mut().ok_or_else(|| io::Error::other("volume authority absent"))?
            .write_all(request)?;
        self.authority.take();
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!("volume worker exited unsuccessfully ({status}); inspect {}",
                        self.directory.join("worker/state.json").display())));
                }
                let bytes = std::fs::read(self.directory.join("worker/retirement.json"))?;
                let result: Retirement = serde_json::from_slice(&bytes)?;
                if !status.success() || !result.passed || !result.detached || result.removed != remove
                    || result.retained_image.as_ref().is_some_and(|image| image.gzip.is_some()) != compress {
                    return Err(io::Error::other("volume retirement lacks successful worker exit and proof"));
                }
                verify_image(&self.directory.join("worker/volume/bounded.dmg"), self.capacity_bytes,
                    remove, result.retained_image.as_ref(), Some(deadline))?;
                return Ok(RetirementEvidence { worker: self.process.clone().unwrap(),
                    ready_sha256: self.ready_sha256.clone(),
                    retirement_sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
                    retained_image: result.retained_image });
            }
            if Instant::now() >= deadline { return Err(io::Error::other("volume retirement observation expired; inspect original worker")); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // EOF alone never attests retirement; a registered live UIKit binding must independently
        // prove all writers retired before this original worker can detach.
        self.authority.take();
        if let Some(mut child) = self.child.take() { std::thread::spawn(move || { let _ = child.wait(); }); }
    }
}

#[cfg(test)]
#[path = "storage_volume_tests.rs"]
mod tests;
