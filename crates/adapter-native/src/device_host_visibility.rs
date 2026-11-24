//! Ordinary host visibility, with original SDK evidence and no consumer clock dependency.
use super::*;
use frontcap_core::{ids::RequestId, native::{android_host::Transition,
    android_webview::AppProcess, app_state::State}, state::load};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent { owner: AppProcess, state: State }
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Samples { before: Vec<u64>, after: Vec<u64>, focused_before: u64, focused_after: u64, dispatch: u64 }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof { transition: Transition, samples: Samples, commands: BTreeMap<String,String> }
fn arguments(owner: &AppProcess, state: State) -> Vec<String> {
    match state {
        State::Background => ["shell","input","keyevent","KEYCODE_HOME"].map(str::to_owned).to_vec(),
        State::Foreground => vec!["shell".into(),"am".into(),"start".into(),"-W".into(),
            "--activity-reorder-to-front".into(),"-n".into(),format!("{}/{}",owner.package,owner.activity)],
    }
}
fn command(root: &Path, proof: &Proof, config: &AndroidLaunch, owner: &AppProcess,
    number: u64, args: &[&str]) -> io::Result<String> {
    for file in ["command.json","exchange.json"] {
        let name = format!("command-{number}/{file}");
        let path = root.join(&name); super::host_entry::unlinked(root,&path)?;
        if proof.commands.get(&name) != Some(&frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(path)?)) {
            return Err(io::Error::other("Android visibility command evidence changed"));
        }
    }
    let (program, actual): (PathBuf,Vec<String>) = load(&root.join(format!("command-{number}/command.json")))?
        .ok_or_else(||io::Error::other("missing Android visibility command"))?;
    let expected: Vec<_> = ["-s",owner.serial.as_str()].into_iter().chain(args.iter().copied()).collect();
    if program != config.adb || actual != expected { return Err(io::Error::other("Android visibility command arguments differ")); }
    let exchange: crate::process::Exchange<()> = crate::process::load_exchange(&root.join(format!("command-{number}/exchange.json")))?
        .ok_or_else(||io::Error::other("missing Android visibility exchange"))?;
    if exchange.exit_code != Some(0) || exchange.failure.is_some() || exchange.cleanup_failure.is_some() || exchange.evidence_failure.is_some() {
        return Err(io::Error::other("Android visibility command did not complete"));
    }
    String::from_utf8(exchange.stdout).map_err(io::Error::other)
}
/// Retained bytes only: never query ADB, publish live ownership or repeat a transition.
pub fn read_host_transition(root: &Path, epoch: &Path, id: &RequestId, owner: &AppProcess,
    state: State) -> io::Result<Option<Transition>> {
    let path = epoch.join("host-visibility").join(id.as_str());
    for name in ["intent.json","receipt.json"] { super::host_entry::unlinked(root,&path.join(name))?; }
    let Some(intent): Option<Intent> = load(&path.join("intent.json"))? else { return Ok(None); };
    if intent.owner != *owner || intent.state != state { return Err(io::Error::other("Android visibility request binding differs")); }
    let Some(proof): Option<Proof> = load(&path.join("receipt.json"))? else { return Ok(None); };
    let config: AndroidLaunch = load(&root.join("launch.json"))?.ok_or_else(||io::Error::other("missing Android launch"))?;
    if config.host_app.as_ref().is_none_or(|a| a.package!=owner.package || a.activity!=owner.activity)
        || config.apk_sha256!=owner.apk_sha256 || config.source_sha256!=owner.source_sha256
        || owner.serial!=format!("emulator-{}",config.port) || proof.transition.owner!=*owner || proof.transition.requested!=state {
        return Err(io::Error::other("Android visibility owner differs from frozen launch"));
    }
    for (name,digest) in &proof.commands {
        let parts:Vec<_>=name.split('/').collect();
        if parts.len()!=2 || !parts[0].strip_prefix("command-").is_some_and(|n| n.parse::<u64>().is_ok_and(|v|v>0 && v.to_string()==n))
            || !["command.json","exchange.json"].contains(&parts[1]) {return Err(io::Error::other("invalid Android visibility evidence path"));}
        let file=root.join(name); super::host_entry::unlinked(root,&file)?;
        if frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&file)?)!=*digest {return Err(io::Error::other("Android visibility command evidence changed"));}
        crate::process::validate_exchange_files(&file)?;
    }
    proof.transition.validate()?;
    let s=&proof.samples;
    let ordered: Vec<_> = s.before.iter().copied().chain([s.focused_before,s.dispatch,s.focused_after])
        .chain(s.after.iter().copied()).collect();
    if s.before.is_empty() || s.after.is_empty() || ordered.first()==Some(&0) || ordered.windows(2).any(|p|p[0]>=p[1]) {
        return Err(io::Error::other("Android visibility samples are unordered"));
    }
    let process = |numbers: &[u64]| -> io::Result<frontcap_core::native::android_lifecycle::Process> {
        let mut numbers=numbers.iter();
        let p=crate::android_installation::process(&owner.package,&owner.activity,owner.pid,|args| {
            let n=numbers.next().ok_or_else(||io::Error::other("missing Android visibility process sample"))?;
            command(root,&proof,&config,owner,*n,args)
        })?;
        if numbers.next().is_some() { return Err(io::Error::other("extra Android visibility process sample")); }
        Ok(p)
    };
    let before=process(&s.before)?; let after=process(&s.after)?;
    let focus=|n| super::webview_lifecycle::focused_component(&command(root,&proof,&config,owner,n,&["shell","dumpsys","window"])?);
    let args=arguments(owner,state);
    command(root,&proof,&config,owner,s.dispatch,&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if before!=proof.transition.process_before || after!=proof.transition.process_after
        || focus(s.focused_before)?!=proof.transition.focused_before || focus(s.focused_after)?!=proof.transition.focused_after {
        return Err(io::Error::other("Android visibility receipt differs from SDK evidence"));
    }
    Ok(Some(proof.transition))
}
impl Device {
    pub(crate) fn require_host_foreground(&self) -> io::Result<()> {
        if self.config.host_app.is_some() && self.host_visibility!=Some(State::Foreground) {
            return Err(io::Error::other("Android host is background or visibility is unresolved"));
        }
        Ok(())
    }
    pub fn set_host_state(&mut self, id: &RequestId, owner: &AppProcess, state: State,
        deadline: Instant, cancelled: &AtomicBool) -> Result<Transition,AcquisitionError> {
        let epoch=self.host_evidence_directory().to_owned();
        if let Some(result)=read_host_transition(&self.directory,&epoch,id,owner,state)? { return Ok(result); }
        let path=epoch.join("host-visibility").join(id.as_str());
        if path.exists() || self.config.host_app.is_none() || self.retired.is_some()
            || self.app_process.as_ref()!=Some(owner) || self.host_visibility.is_none()
            || self.host_visibility==Some(state) || !self.unretired_commands.is_empty() || self.contact_poisoned {
            return Err(io::Error::other("Android host visibility unavailable, repeated or unresolved").into());
        }
        if let Some(contacts)=&self.contact_state { contacts.require_released()?; }
        self.verify_app_process(owner,deadline,cancelled)?;
        let first=self.command_number+1; let mut samples=Samples::default();
        let sample=|device: &mut Self, numbers: &mut Vec<u64>| {
            crate::android_installation::process(&owner.package,&owner.activity,owner.pid,|args| {
                let raw=device.adb(args,deadline,cancelled,false)?.ok_or_else(||io::Error::other("missing Android process response"))?;
                numbers.push(device.command_number); Ok(raw)
            })
        };
        let before=sample(self,&mut samples.before)?;
        if before.start_ticks!=owner.process_start_ticks { return Err(io::Error::other("Android host process changed before visibility").into()); }
        let focused_before=super::webview_lifecycle::focused_component(&self.adb(&["shell","dumpsys","window"],deadline,cancelled,false)?.unwrap())?;
        samples.focused_before=self.command_number;
        let component=format!("{}/{}",owner.package,owner.activity);
        let valid=match state { State::Background=>focused_before==component, State::Foreground=>focused_before.split('/').next()!=Some(owner.package.as_str()) };
        if !valid { return Err(io::Error::other("Android host focus differs before transition").into()); }
        std::fs::create_dir_all(path.parent().unwrap())?; std::fs::create_dir(&path)?;
        // The effect is never repeated after intent, including when publication or sampling fails.
        let result=(|| -> io::Result<Transition> {
            save_atomic(&path.join("intent.json"),&Intent {owner:owner.clone(),state})?;
            self.host_visibility=None; self.invalidate_contact_capture();
            let args=arguments(owner,state);
            self.adb(&args.iter().map(String::as_str).collect::<Vec<_>>(),deadline,cancelled,false)?;
            samples.dispatch=self.command_number;
            let focused_after=loop {
                self.verify_app_process(owner,deadline,cancelled)?;
                let raw=self.adb(&["shell","dumpsys","window"],deadline,cancelled,false)?.unwrap();
                if let Ok(focus)=super::webview_lifecycle::focused_component(&raw) {
                    let reached=match state { State::Background=>focus.split('/').next()!=Some(owner.package.as_str()), State::Foreground=>focus==component };
                    if reached { samples.focused_after=self.command_number; break focus; }
                }
                std::thread::sleep(Duration::from_millis(25));
            };
            let after=sample(self,&mut samples.after)?;
            let transition=Transition {owner:owner.clone(),requested:state,process_before:before,process_after:after,focused_before,focused_after};
            transition.validate()?;
            let mut commands=BTreeMap::new();
            for n in first..=self.command_number { for file in ["command.json","exchange.json"] {
                let name=format!("command-{n}/{file}");
                commands.insert(name.clone(),frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(self.directory.join(name))?));
            }}
            save_atomic(&path.join("receipt.json"),&Proof {transition,samples,commands})?;
            let verified=read_host_transition(&self.directory,&epoch,id,owner,state)?.ok_or_else(||io::Error::other("Android visibility receipt missing"))?;
            self.host_visibility=Some(state); Ok(verified)
        })();
        result.map_err(|error|AcquisitionError {cause:error.to_string(),retirement:Some(self.stop())})
    }
}
#[cfg(test)]
#[path="device_host_visibility_tests.rs"]
mod tests;
