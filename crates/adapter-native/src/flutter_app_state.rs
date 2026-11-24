//! Same-process Flutter host visibility and read-only original-result recovery.
use crate::{flutter_app::Acquired, flutter_session::Ready};
use frontcap_core::{ids::RequestId, native::{app_state::State, flutter::Owner,
    flutter_lifecycle::{Installation, Transition}, ios_ui::{UiCompletion, UiOperation, UiRequest}},
    state::{load, save_atomic, save_atomic_bytes}, write_budget::{Class, WriteBudget}};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, io, path::{Path, PathBuf}};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag="platform",rename_all="snake_case",deny_unknown_fields)]
pub enum Native {
    Ios { completion: Box<UiCompletion> },
    Android { focused_before:String, focused_after:String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changed {
    pub before:Acquired,
    pub after:Acquired,
    pub native:Native,
    pub ready:Ready,
    pub state:State,
}
impl Changed {
    pub fn validate(&self, application:&Acquired, ready:&Ready, requested:State) -> io::Result<Transition> {
        application.validate(ready)?;
        self.before.validate(ready)?; self.after.validate(ready)?;
        if self.before != *application || self.after != self.before || self.ready != *ready || self.state != requested {
            return Err(io::Error::other("Flutter host transition changed its process, installation or endpoint"));
        }
        let result=match (&self.before,&self.after,&self.native) {
            (Acquired::Ios(before),Acquired::Ios(after),Native::Ios {completion}) => Transition::Ios {
                owner:ready.owner.clone(),requested,process_before:before.process.clone(),process_after:after.process.clone(),
                installation:Installation {bundle:before.bundle.clone(),app:before.containers.app.clone(),data:before.containers.data.clone(),
                    data_device:before.containers.data_device,data_inode:before.containers.data_inode},
                runner:ready.system_ui.clone().ok_or_else(||io::Error::other("Flutter host transition has no independent runner"))?,
                completion:completion.clone(),
            },
            (Acquired::Android(before),Acquired::Android(after),Native::Android {focused_before,focused_after}) => Transition::Android {
                owner:ready.owner.clone(),requested,process_before:before.process.clone(),process_after:after.process.clone(),
                installation:before.installation.clone(),activity:before.activity.clone(),focused_before:focused_before.clone(),focused_after:focused_after.clone(),
            },
            _=>return Err(io::Error::other("Flutter host visibility platform differs")),
        };
        result.validate().map_err(io::Error::other)?;
        Ok(result)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id:RequestId,
    ready:Ready,
    application:Acquired,
    requested:State,
    command_number:u64,
    ui_request:Option<UiRequest>,
    completion:Option<Changed>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved { initial:Owner, entries:Vec<Entry> }
impl Saved {
    fn validate(&self) -> io::Result<HashMap<String,State>> {
        self.initial.validate().map_err(io::Error::other)?;
        let mut states=HashMap::new();let mut ids=HashSet::new();let mut previous=0;
        for (index,entry) in self.entries.iter().enumerate() {
            entry.application.validate(&entry.ready)?;
            if !ids.insert(&entry.id) || entry.command_number <= previous
                || entry.ready.owner.platform != self.initial.platform || entry.ready.owner.device != self.initial.device
                || entry.ready.owner.source_sha256 != self.initial.source_sha256
                || states.get(&entry.ready.owner.generation).copied().unwrap_or(State::Foreground)==entry.requested {
                return Err(io::Error::other("invalid Flutter host transition history"));
            }
            previous=entry.command_number;
            match (&entry.application,&entry.ui_request) {
                (Acquired::Ios(app),Some(request)) => {
                    let runner=entry.ready.system_ui.as_ref().ok_or_else(||io::Error::other("missing independent runner"))?;
                    request.validate(runner).map_err(io::Error::other)?;
                    if request.operation != (UiOperation::SetHostAppState {bundle:app.bundle.clone(),pid:app.process.pid,state:entry.requested}) {
                        return Err(io::Error::other("Flutter host request belongs to another app"));
                    }
                },
                (Acquired::Android(_),None)=>{},
                _=>return Err(io::Error::other("Flutter host request platform differs")),
            }
            if let Some(done)=&entry.completion {
                done.validate(&entry.application,&entry.ready,entry.requested)?;
                states.insert(entry.ready.owner.generation.clone(),entry.requested);
            } else if index+1 != self.entries.len() {return Err(io::Error::other("work follows unresolved Flutter host effect"));}
        }
        Ok(states)
    }
}
pub struct History { root:PathBuf, saved:Saved, _lock: crate::store::RequestLease, poisoned:bool, budget:Option<WriteBudget> }
impl History {
    pub fn create(root:&Path, initial:Owner)->io::Result<Self>{
        let budget=WriteBudget::from_environment()?;
        Self::create_with_budget(root,initial,budget.as_ref())
    }
    pub fn create_with_budget(root:&Path, initial:Owner, budget:Option<&WriteBudget>)->io::Result<Self>{
        let directory=root.join("app-state");std::fs::create_dir(&directory)?;
        let lock=crate::store::lock(&directory).map_err(io::Error::other)?;
        let saved=Saved {initial,entries:vec![]};saved.validate()?;
        match budget {Some(budget)=>budget.save_atomic(Class::Ordinary,&directory.join("history.json"),&saved)?,
            None=>save_atomic(&directory.join("history.json"),&saved)?}
        Ok(Self {root:root.into(),saved,_lock: lock,poisoned:false,budget:budget.cloned()})
    }
    pub fn contains(&self,id:&RequestId)->bool {self.saved.entries.iter().any(|entry|&entry.id==id)}
    pub fn ensure_resolved(&self)->io::Result<()> {
        self.saved.validate()?;
        if self.poisoned || self.saved.entries.iter().any(|entry|entry.completion.is_none()) {
            Err(io::Error::other("Flutter host transition unresolved; no new dispatch authority"))
        } else {Ok(())}
    }
    pub fn ensure_foreground(&self,owner:&Owner)->io::Result<()> {
        self.ensure_resolved()?;
        if self.saved.validate()?.get(&owner.generation).copied().unwrap_or(State::Foreground)!=State::Foreground {
            return Err(io::Error::other("Flutter app work unavailable while background"));
        }
        Ok(())
    }
    pub fn validate_request(&self,id:&RequestId,ready:&Ready,requested:State)->io::Result<()> {
        if let Some(entry)=self.saved.entries.iter().find(|entry|&entry.id==id) {
            return if entry.ready==*ready && entry.requested==requested {Ok(())} else {Err(io::Error::other("Flutter host request ID conflict"))};
        }
        self.ensure_resolved()?;
        if self.saved.validate()?.get(&ready.owner.generation).copied().unwrap_or(State::Foreground)==requested {
            return Err(io::Error::other("Flutter host state already matches request"));
        }
        Ok(())
    }
    fn publish(&mut self,saved:Saved)->io::Result<()> {
        if self.poisoned {return Err(io::Error::other("Flutter host history publication uncertain"));}
        saved.validate()?;
        let bytes=frontcap_core::canonical::canonical_of(&saved);
        if let Some(budget)=&self.budget {budget.charge(Class::Ordinary,bytes.len() as u64)?;}
        self.poisoned=true;
        save_atomic_bytes(&self.root.join("app-state/history.json"),bytes.as_bytes())?;
        self.saved=saved;self.poisoned=false;Ok(())
    }
    pub fn dispatch(&mut self,id:RequestId,ready:Ready,application:Acquired,requested:State,command_number:u64,ui_request:Option<UiRequest>)->io::Result<()> {
        self.validate_request(&id,&ready,requested)?;
        if self.contains(&id){return Err(io::Error::other("Flutter host intent cannot be resent"));}
        let mut saved=self.saved.clone();saved.entries.push(Entry {id,ready,application,requested,command_number,ui_request,completion:None});
        self.publish(saved)
    }
    pub fn complete(&mut self,id:&RequestId,changed:Changed)->io::Result<Transition>{
        let entry=self.saved.entries.iter().find(|entry|&entry.id==id).ok_or_else(||io::Error::other("unknown Flutter host request"))?;
        let result=verify_worker(&self.root,&self.saved.initial,entry)?.ok_or_else(||io::Error::other("Flutter host evidence incomplete"))?;
        if result!=changed {return Err(io::Error::other("Flutter host reply differs from retained evidence"));}
        let transition=changed.validate(&entry.application,&entry.ready,entry.requested)?;
        if let Some(previous)=&entry.completion {
            return if previous==&changed {Ok(transition)} else {Err(io::Error::other("Flutter host completion changed"))};
        }
        let mut saved=self.saved.clone();saved.entries.iter_mut().find(|entry|&entry.id==id).unwrap().completion=Some(changed);
        self.publish(saved)?;Ok(transition)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest { number:u64, timeout_ms:u64, operation:String, generation:String, state:State,
    #[serde(default)] call:Option<crate::ios::UiCall> }
fn focus(raw:&str)->io::Result<String>{
    let lines:Vec<_>=raw.lines().filter_map(|line|line.trim().strip_prefix("mCurrentFocus=")).collect();
    let value=if lines.len()==1 {lines[0].strip_prefix("Window{").and_then(|s|s.strip_suffix('}')).and_then(|s|s.split_whitespace().last())}else{None};
    let (package,activity)=value.and_then(|s|s.split_once('/')).ok_or_else(||io::Error::other("unavailable retained Android focus"))?;
    let activity=if activity.starts_with('.') {format!("{package}{activity}")}else{activity.into()};
    if !frontcap_core::native::android_capture::valid_app(package,&activity){return Err(io::Error::other("invalid retained Android focus"));}
    Ok(format!("{package}/{activity}"))
}
fn verify_worker(root:&Path,initial:&Owner,entry:&Entry)->io::Result<Option<Changed>>{
    let directory=root.join("session/worker/commands").join(entry.command_number.to_string());
    let Some(request):Option<WorkerRequest>=load(&directory.join("request.json"))? else{return Ok(None);};
    if request.number!=entry.command_number || !(1..=120_000).contains(&request.timeout_ms) || request.operation!="set_app_state"
        || request.generation!=entry.ready.owner.generation || request.state!=entry.requested {
        return Err(io::Error::other("Flutter host worker request binding differs"));
    }
    match (&request.call,&entry.ui_request) {
        (Some(crate::ios::UiCall::Execute {identity,app:None,request}),Some(expected))
            if entry.ready.system_ui.as_ref()==Some(identity) && request==expected=>{},
        (None,None)=>{},
        _=>return Err(io::Error::other("Flutter host native request binding differs")),
    }
    let evidence=directory.join("app-state");
    let Some(before)=load(&evidence.join("before.json"))? else{return Ok(None);};
    let Some(after)=load(&evidence.join("after.json"))? else{return Ok(None);};
    let Some(native)=load(&evidence.join("native.json"))? else{return Ok(None);};
    let changed=Changed {before,after,native,ready:entry.ready.clone(),state:entry.requested};
    changed.validate(&entry.application,&entry.ready,entry.requested)?;
    match &changed.native {
        Native::Ios {completion}=>{
            let request=entry.ui_request.as_ref().ok_or_else(||io::Error::other("missing system request"))?;
            let original=crate::flutter_system_ui::read_ui_completion(root,initial,entry.ready.system_ui.as_ref().unwrap(),&entry.id,&request.operation)?;
            if original.as_ref()!=Some(completion){return Err(io::Error::other("Flutter host native receipt changed"));}
        },
        Native::Android {focused_before,focused_after}=>{
            if focus(&std::fs::read_to_string(evidence.join("window-before.txt"))?)?!=*focused_before
                || focus(&std::fs::read_to_string(evidence.join("window-after.txt"))?)?!=*focused_after {
                return Err(io::Error::other("Flutter host focus differs from retained windows"));
            }
        },
    }
    if let Some(log)=load::<Changed>(&directory.join("command.log"))?
        && log!=changed {return Err(io::Error::other("Flutter host saved reply changed"));}
    Ok(Some(changed))
}
/// Read original evidence only. Never query a process, repair history or redispatch an effect.
pub fn read_completion(root:&Path,owner:&Owner,id:&RequestId,requested:State)->io::Result<Option<Transition>>{
    let Some(saved):Option<Saved>=load(&root.join("app-state/history.json"))? else{return Ok(None);};
    saved.validate()?;
    let Some(entry)=saved.entries.iter().find(|entry|&entry.id==id) else{return Ok(None);};
    if &entry.ready.owner!=owner || entry.requested!=requested {return Err(io::Error::other("Flutter host result belongs to another request"));}
    let acquired=crate::flutter_restarts::application_at(root,owner)?;
    if acquired.as_ref()!=Some(&(entry.ready.clone(),entry.application.clone())) {
        return Err(io::Error::other("Flutter host history differs from original acquisition"));
    }
    let result=verify_worker(root,&saved.initial,entry)?;
    if let Some(completion)=&entry.completion
        && result.as_ref()!=Some(completion){return Err(io::Error::other("Flutter original host evidence changed"));}
    result.map(|done|done.validate(&entry.application,&entry.ready,entry.requested)).transpose()
}

#[cfg(test)]
#[path="flutter_app_state_tests.rs"]
mod tests;
