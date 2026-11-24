//! Same-process visibility through the existing independent runner; saved reads never reactivate.
use super::*;
use frontcap_core::{native::{app_state::State, ios_host::{Operation, Ready, StateTransition}, ios_ui::UiOperation}, process::ProcessIdentity};

impl Target {
    pub fn app_lifecycle(&self) -> bool {
        self.system.as_ref().is_some_and(|s| s.ready.host_app_state)
    }
    pub(crate) fn admit_visibility(&self, operation: &Operation) -> io::Result<()> {
        if let Operation::SetAppState {owner, state} = operation {
            if owner.as_ref() != &self.owner || !self.app_lifecycle() || self.pending_entry.is_some()
                || self.visibility == Some(*state) {
                return Err(io::Error::other("host visibility unavailable, repeated or owner differs"));
            }
        } else if self.visibility == Some(State::Background) && operation.system_operation().is_none() {
            return Err(io::Error::other("background host permits foreground, system UI or retirement"));
        }
        Ok(())
    }
    pub(crate) fn set_visibility(&mut self, id: &RequestId, owner: &Owner, state: State,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<StateTransition> {
        self.admit_visibility(&Operation::SetAppState {owner:Box::new(owner.clone()),state})?;
        let directory = self.directory.join("app-state").join(id.as_str());
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        let work = (|| -> io::Result<StateTransition> {
            self.app.ensure_running(deadline, cancelled)?;
            let before = sample(owner)?;
            save_evidence(&directory.join("before.json"), &before)?;
            // Home may cancel native touches independently. Require explicit cancellation before
            // subsequent contact input, and discard any targeting basis from before the transition.
            if let Some(contacts) = &mut self.input.contacts { contacts.interrupt()?; }
            self.input.capture_basis = None;
            let command = UiOperation::SetHostAppState {bundle:self.app.bundle().into(),pid:owner.process.pid,state};
            let completion = self.execute_system(id, &command, deadline, cancelled).map_err(|e| io::Error::other(e.cause))?;
            self.app.ensure_running(deadline, cancelled)?;
            let after = sample(owner)?;
            save_evidence(&directory.join("after.json"), &after)?;
            let transition = StateTransition {owner:owner.clone(),before,after,completion,directory:self.directory.join("system-ui/client")};
            if transition.state().map_err(io::Error::other)? != state { return Err(io::Error::other("visibility outcome differs")); }
            save_evidence(&directory.join("receipt.json"), &transition)?;
            self.visibility = Some(state);
            Ok(transition)
        })();
        if let Err(error) = &work {
            let _ = save_evidence(&directory.join("failure.json"), &error.to_string());
            self.stop();
        }
        work
    }
}
fn sample(owner: &Owner) -> io::Result<ProcessIdentity> {
    let process = ProcessIdentity::of(owner.process.pid)?.ok_or_else(|| io::Error::other("host process absent"))?;
    if process != owner.process || process.has_exited()? { return Err(io::Error::other("host process changed")); }
    Ok(process)
}
/// Both original process samples and the source-bound runner receipt are necessary. Incomplete
/// evidence remains pending even when the system runner itself reports a completed effect.
pub(crate) fn read(host: &Path, config: &Config, ready: &Ready, id: &RequestId, state: State) -> io::Result<Option<StateTransition>> {
    let runner = ready.system_ui.as_ref().ok_or_else(|| io::Error::other("missing system runner"))?;
    let system = config.system_ui.as_ref().ok_or_else(|| io::Error::other("missing system launch"))?;
    if !ready.app_lifecycle { return Err(io::Error::other("host visibility unavailable")); }
    let directory = host.join("app-state").join(id.as_str());
    for name in ["before.json", "after.json", "receipt.json"] {
        let mut cursor=host.to_owned();
        for part in directory.join(name).strip_prefix(host).map_err(io::Error::other)?.components() {
            cursor.push(part);
            match std::fs::symlink_metadata(&cursor) {
                Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("linked host visibility evidence")),
                Ok(_) => (), Err(e) if e.kind()==io::ErrorKind::NotFound => break, Err(e) => return Err(e),
            }
        }
    }
    let Some(before): Option<ProcessIdentity> = load(&directory.join("before.json"))? else { return Ok(None); };
    let Some(after): Option<ProcessIdentity> = load(&directory.join("after.json"))? else { return Ok(None); };
    let saved: crate::xctest::UiReady = load(&host.join("system-ui/ready.json"))?.ok_or_else(||io::Error::other("missing runner readiness"))?;
    if !saved.host_app_state { return Err(io::Error::other("original runner lacks host visibility")); }
    let command = UiOperation::SetHostAppState {bundle:config.app.bundle.clone(),pid:ready.owner.process.pid,state};
    let Some(completion) = super::system::read_completion(host, system, &config.simulator.source_sha256,
        &ready.owner, runner, id, &command)? else { return Ok(None); };
    let transition = StateTransition {owner:ready.owner.clone(),before,after,completion,directory:host.join("system-ui/client")};
    if transition.state().map_err(io::Error::other)? != state { return Err(io::Error::other("saved visibility differs")); }
    if let Some(saved) = load::<StateTransition>(&directory.join("receipt.json"))? && saved != transition {
        return Err(io::Error::other("saved host visibility receipt changed"));
    }
    Ok(Some(transition))
}
