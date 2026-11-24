//! URL-only replacement keeps the owned simulator and independent system runner alive.
//! The pending handle is in memory; retained evidence never grants a new live lease.
use super::*;
use frontcap_core::{canonical::sha256_bytes_hex, native::{contacts, external_entry::{Observation, validate_url}, ios_host::Owner as HostOwner}, state::load};

pub use frontcap_core::native::ios_host::EntryAcceptance as Begun;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    begun: Begun,
    admission_sha256: String,
    commands: BTreeMap<String, String>,
}
pub(super) struct Pending {
    directory: PathBuf,
    config: Launch,
    begun: Begun,
}
fn bad(message: &str) -> io::Error { io::Error::other(message) }
fn unlinked(root: &Path, path: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(root)?.file_type().is_symlink() {return Err(bad("linked entry evidence root"));}
    let mut current=root.to_path_buf();
    for part in path.strip_prefix(root).map_err(io::Error::other)?.components() {
        if !matches!(part,std::path::Component::Normal(_)) {return Err(bad("invalid entry evidence path"));}
        current.push(part);
        if std::fs::symlink_metadata(&current)?.file_type().is_symlink() {return Err(bad("linked entry evidence"));}
    }
    Ok(())
}
/// Original acceptance lookup is read-only and does not require live input products or processes.
pub fn read_begin(directory: &Path, simulator_evidence: &Path, previous: &HostOwner, url: &str) -> io::Result<Option<Begun>> {
    previous.validate().map_err(io::Error::other)?;validate_url(url).map_err(io::Error::other)?;
    let path=directory.join("entry-accepted.json");
    if !path.try_exists()? {return Ok(None);}
    for name in ["entry-intent.json","entry-admission.json","entry-accepted.json","terminated.json"] {unlinked(directory,&directory.join(name))?;}
    let intent:Begun=load(&directory.join("entry-intent.json"))?.ok_or_else(||bad("missing entry intent"))?;
    let proof:Proof=load(&path)?.ok_or_else(||bad("missing entry proof"))?;
    if load::<ProcessIdentity>(&directory.join("terminated.json"))?.as_ref()!=Some(&previous.process)
        || intent.previous!=*previous || intent.url!=url || proof.begun!=intent || proof.commands.is_empty()
        || intent.confirmation!=Observation::Unobserved || intent.destination!=Observation::Unobserved
        || sha256_bytes_hex(&std::fs::read(directory.join("entry-admission.json"))?)!=proof.admission_sha256 {
        return Err(bad("entry acceptance differs from original intent"));
    }
    for (name,digest) in proof.commands {
        let parts:Vec<_>=name.split('/').collect();
        if parts.len()!=2 || !parts[0].strip_prefix("command-").is_some_and(|n|n.parse::<u64>().is_ok_and(|v|v>0 && v.to_string()==n))
            || !["command.json","exchange.json"].contains(&parts[1]) {return Err(bad("invalid entry command evidence path"));}
        let path=simulator_evidence.join(name);unlinked(simulator_evidence,&path)?;
        if sha256_bytes_hex(&std::fs::read(&path)?)!=digest {return Err(bad("entry command evidence changed"));}
        crate::process::validate_exchange_files(&path)?;
    }
    Ok(Some(intent))
}
impl App {
    /// Terminate the original app once and deliver one admitted URL. OS acceptance is not readiness.
    /// System-only XCTest stays owned; app actions must wait for `poll_entry` to bind the replacement.
    pub fn begin_entry(&mut self, url: &str, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> Result<Begun, AcquisitionError> {
        validate_url(url).map_err(io::Error::other)?;
        if self.launch_config.prepared.is_none() {return Err(bad("cold URL entry requires a prepared app").into());}
        self.ensure_running(deadline,cancelled)?;
        if matches!(self.owner,Some(Owner::Xctest(_))) {
            let ready:UiReady=load(&self.directory.join("xctest-ready.json"))?.ok_or_else(||bad("missing independent runner readiness"))?;
            if !ready.system_only || !ready.system_alerts {return Err(bad("cold entry requires a system-only runner").into());}
        }
        // Refuse a reused directory before changing either target or its immutable proof.
        std::fs::create_dir(directory)?;
        let directory=directory.canonicalize()?;
        let result=(|| -> io::Result<Begun> {
            let (simulator_root,first)={let (root,n)=self.simulator_mut()?.command_cursor();(root.to_owned(),n+1)};
            let bundle=self.bundle.clone();
            let admission=crate::ios_external_entry::admit(self.simulator_mut()?,&bundle,url,deadline,cancelled)?;
            let begun=Begun {previous:HostOwner {simulator:self.simulator_id.clone(),process:self.process.clone(),attachment_sha256:self.attachment_sha256.clone()},url:url.into(),confirmation:Observation::Unobserved,destination:Observation::Unobserved};
            save_evidence(&directory.join("entry-admission.json"),&admission)?;
            save_evidence(&directory.join("entry-intent.json"),&begun)?;
            let config=self.prepare_replacement(&directory,false,deadline,cancelled)?;
            save_evidence(&directory.join("launch-intent.json"),&config)?;
            self.entry=Some(Pending {directory:directory.clone(),config,begun:begun.clone()});
            crate::ios_external_entry::open(self.simulator_mut()?,url,deadline,cancelled)?;
            self.ensure_platform_running(deadline,cancelled)?;
            let last=self.simulator_mut()?.command_cursor().1;
            let mut commands=BTreeMap::new();
            for n in first..=last {
                for name in ["command.json","exchange.json"] {
                    let relative=format!("command-{n}/{name}");
                    commands.insert(relative.clone(),sha256_bytes_hex(&std::fs::read(simulator_root.join(relative))?));
                }
            }
            save_evidence(&directory.join("entry-accepted.json"),&Proof {begun:begun.clone(),commands,
                admission_sha256:sha256_bytes_hex(&std::fs::read(directory.join("entry-admission.json"))?)})?;
            Ok(begun)
        })();
        result.map_err(|e| {let _=save_evidence(&directory.join("failure.json"),&e.to_string());AcquisitionError {cause:e.to_string(),retirement:Some(self.stop())}})
    }
    pub fn entry_pending(&self) -> bool {self.entry.is_some()}
    /// No waiting loop and no new URL dispatch. Absent endpoint means waiting, never success.
    /// An unexpected endpoint/process or changed installation fences this owned target.
    pub fn poll_entry(&mut self, deadline: Instant, cancelled: &AtomicBool) -> Result<Option<ProcessIdentity>, AcquisitionError> {
        if self.entry.is_none() {return Err(bad("no pending URL replacement").into());}
        let result=self.poll_entry_inner(deadline,cancelled);
        result.map_err(|e| {if let Some(p)=&self.entry {let _=save_evidence(&p.directory.join("failure.json"),&e.to_string());}AcquisitionError {cause:e.to_string(),retirement:Some(self.stop())}})
    }
    fn poll_entry_inner(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Option<ProcessIdentity>> {
        self.ensure_platform_running(deadline,cancelled)?;
        let pending=self.entry.as_ref().ok_or_else(||bad("no pending URL replacement"))?;
        let endpoint_path=self.container.join("Documents/frontcap-hid/endpoint.json");
        if !endpoint_path.try_exists()? {return Ok(None);}
        unlinked(&self.container,&endpoint_path)?;
        let endpoint:contacts::Endpoint=load(&endpoint_path)?.ok_or_else(||bad("missing replacement input endpoint"))?;
        endpoint.validate(endpoint.pid).map_err(io::Error::other)?;
        let process=ProcessIdentity::of(endpoint.pid)?.ok_or_else(||bad("replacement endpoint process absent"))?;
        if process==pending.begun.previous.process || process.has_exited()? {return Err(bad("old or exited replacement process"));}
        let prior=pending.directory.join("previous-native-contacts/endpoint.json");
        if let Some(old)=load::<contacts::Endpoint>(&prior)?
            && old.generation==endpoint.generation {return Err(bad("replacement reused input generation"));}
        let installed=prepared::read_installed(&pending.directory,&pending.config,&self.simulator_id)?
            .ok_or_else(||bad("missing prepared installation"))?;
        let executable=installed.container.join(&pending.config.prepared.as_ref().unwrap().executable);
        if process_executable(process.pid)?.canonicalize()?!=executable.canonicalize()? {return Err(bad("replacement executable differs from installed app"));}
        let directory=pending.directory.clone();let config=pending.config.clone();
        let previous=pending.begun.previous.process.clone();
        let old_directory=self.directory.clone();
        prepared::verify_installed(self.simulator_mut()?,&config,&directory,Some(&old_directory),deadline,cancelled)?;
        let simulator=self.simulator_id.clone();let bundle=self.bundle.clone();
        let output=self.simulator_mut()?.command_with_environment(&["get_app_container",&simulator,&bundle,"data"],&BTreeMap::new(),deadline,cancelled)?;
        let container=PathBuf::from(std::str::from_utf8(&output).map_err(io::Error::other)?.trim());
        if container.canonicalize()?!=self.container.canonicalize()? || process.has_exited()? {return Err(bad("replacement data container or process changed"));}
        if matches!(self.owner,Some(Owner::Xctest(_))) {
            let ready:UiReady=load(&old_directory.join("xctest-ready.json"))?.ok_or_else(||bad("missing retained system runner"))?;
            if !ready.system_only || !ready.system_alerts || ready.process.has_exited()? {return Err(bad("retained system runner unavailable"));}
            save_evidence(&directory.join("xctest-ready.json"),&ready)?;
        }
        save_evidence(&directory.join("process.json"),&process)?;
        save_evidence(&directory.join("container.json"),&container)?;
        save_evidence(&directory.join("restarted.json"),&serde_json::json!({"previous_process":previous,"process":process,"container":container,"simulator":simulator}))?;
        self.process=process.clone();self.container=container;self.directory=directory;self.launch_config=config;self.entry=None;
        Ok(Some(process))
    }
}
#[cfg(target_os="macos")]
fn process_executable(pid:u32) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut bytes=vec![0_u8;libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let size=unsafe {libc::proc_pidpath(pid as i32,bytes.as_mut_ptr().cast(),bytes.len() as u32)};
    if size<=0 {return Err(io::Error::last_os_error());}
    let end=bytes.iter().position(|b|*b==0).ok_or_else(||bad("unterminated process executable"))?;
    bytes.truncate(end);Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}
#[cfg(target_os="linux")]
fn process_executable(pid:u32) -> io::Result<PathBuf> {std::fs::read_link(format!("/proc/{pid}/exe"))}
#[cfg(not(any(target_os="macos",target_os="linux")))]
fn process_executable(_pid:u32) -> io::Result<PathBuf> {Err(bad("process executable inspection unavailable"))}

#[cfg(test)]
#[path="simulator_app_entry_tests.rs"]
mod tests;
