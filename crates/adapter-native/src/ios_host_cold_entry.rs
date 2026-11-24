//! Pending native-host entry retains old system authority until replacement publication.
use super::*;
use frontcap_core::native::ios_host::{Completion, Entry, Operation, Ready};

pub(super) struct Pending { entry: Entry, root: PathBuf, contacts: bool, generation: Option<String> }
impl Config {
    pub fn cold_entry(&self) -> bool { self.app.prepared.is_some() && self.contacts && self.system_ui.is_some() }
}
impl Target {
    pub(crate) fn admit_entry_operation(&self, operation: &Operation) -> io::Result<()> {
        if let Some(pending)=&self.pending_entry {
            if operation.system_operation().is_some()
                || matches!(operation,Operation::PollEntry {begin} if begin==&pending.entry.begin) {return Ok(());}
            return Err(io::Error::other("pending iOS entry permits original poll, system UI or retirement"));
        }
        if matches!(operation,Operation::PollEntry {..}) {return Err(io::Error::other("no pending iOS entry"));}
        if matches!(operation,Operation::BeginEntry {..}) && (!self.app.supports_entry() || self.system.is_none() || self.contact_endpoint().is_none()) {
            return Err(io::Error::other("cold iOS entry needs native input and an independent system runner"));
        }
        Ok(())
    }
    pub(crate) fn begin_entry(&mut self, id:&RequestId, previous:&Owner, url:&str, root:&Path,
        deadline:Instant, cancelled:&AtomicBool) -> io::Result<Entry> {
        if previous!=&self.owner || self.pending_entry.is_some() {return Err(io::Error::other("cold iOS entry owner differs"));}
        std::fs::create_dir(root)?;let root=root.canonicalize()?;
        let result=(|| -> io::Result<Entry> {
            let contacts=self.input.contacts.is_some();let generation=self.contact_endpoint().map(|e|e.generation.clone());
            save_evidence(&root.join("replacement-intent.json"),&serde_json::json!({"previous":previous,"previous_evidence":self.directory,"contacts":contacts,"pages":self.pages.is_some()}))?;
            if let Some(input)=&self.input.contacts {input.retain_pending()?;}
            if let Some(pages)=&self.pages {pages.retain(&self.app)?;}
            self.input=Input::new(&root.join("host"));self.pages=None;
            let acceptance=self.app.begin_entry(url,&root.join("app"),deadline,cancelled).map_err(|e|io::Error::other(e.cause))?;
            let entry=Entry {begin:id.clone(),acceptance};entry.validate().map_err(io::Error::other)?;
            save_evidence(&root.join("entry.json"),&entry)?;
            self.pending_entry=Some(Pending {entry:entry.clone(),root:root.clone(),contacts,generation});
            Ok(entry)
        })();
        if let Err(e)=&result {let _=save_evidence(&root.join("failure.json"),&e.to_string());self.stop();}
        result
    }
    pub(crate) fn poll_entry(&mut self, id:&RequestId, begin:&RequestId, deadline:Instant,
        cancelled:&AtomicBool) -> io::Result<Completion> {
        let pending=self.pending_entry.as_ref().ok_or_else(||io::Error::other("no pending iOS entry"))?;
        if begin!=&pending.entry.begin {return Err(io::Error::other("iOS entry poll differs"));}
        let root=pending.root.clone();
        let result=(|| -> io::Result<Completion> {
            let ready=if self.app.poll_entry(deadline,cancelled).map_err(|e|io::Error::other(e.cause))?.is_none() {None} else {
                let pending=self.pending_entry.as_ref().unwrap();
                let previous=pending.entry.acceptance.previous.clone();let contacts=pending.contacts;let generation=pending.generation.clone();
                let system=self.system_launch().ok_or_else(||io::Error::other("pending system runner unavailable"))?;
                // Original system receipts retain their store. New host authority gets a fresh runner.
                self.app.detach_xctest().map_err(|e|io::Error::other(e.cause))?;self.system=None;
                let (owner,input,pages)=Self::initialize(&mut self.app,&root.join("host"),contacts,deadline,cancelled)?;
                if owner.process==previous.process || owner.simulator!=previous.simulator || owner.attachment_sha256!=previous.attachment_sha256
                    || input.contacts.as_ref().map(|s|&s.endpoint().generation)==generation.as_ref() {
                    return Err(io::Error::other("cold replacement reused or changed host authority"));
                }
                self.visibility=None;self.owner=owner;self.input=input;self.pages=pages;self.directory=root.join("host");
                self.enable_system(system.0,&system.1,deadline,cancelled).map_err(|e|io::Error::other(e.cause))?;
                let ready=Ready {owner:self.owner.clone(),contacts:self.contact_endpoint().cloned(),pages:self.pages_available(),app_lifecycle:self.app_lifecycle(),system_ui:self.system_identity().cloned()};
                crate::ios_host_fault::point(id, "before_entry_ready")?;
                save_evidence(&root.join("entry-published-by.json"),id)?;
                save_evidence(&root.join("replacement-ready.json"),&ready)?;
                self.pending_entry=None;Some(Box::new(ready))
            };
            let result=Completion::EntryPolled {begin:begin.clone(),ready};
            std::fs::create_dir_all(root.join("polls"))?;
            save_evidence(&root.join("polls").join(format!("{id}.json")),&result)?;
            Ok(result)
        })();
        if let Err(e)=&result {let _=save_evidence(&root.join("failure.json"),&e.to_string());self.stop();}
        result
    }
}
