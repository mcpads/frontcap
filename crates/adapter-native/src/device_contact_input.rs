//! Screenshot-bound external contact requests. Native completion is not consumer admission.
use super::*;
use frontcap_core::{ids::RequestId, native::contacts::Operation};
use serde_json::{Value, json};

pub use frontcap_core::native::android_webview::ContactCapture;
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    observation: RequestId,
    operation: Operation,
    request: Value,
    reply: Option<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Session {
    owner: ContactDriver,
    next: u32,
    revision: u64,
    basis: Option<ContactCapture>,
    pending: Option<RequestId>,
    entries: std::collections::BTreeMap<RequestId, Entry>,
}
impl Session {
    fn inspect(&self, storage_uncertain: bool) -> frontcap_core::native::android_progress::Contacts {
        use frontcap_core::native::android_progress::{Contacts, Input};
        let record = |(id, entry): (&RequestId, &Entry)| Input {
            id: id.clone(), observation: entry.observation.clone(), operation: entry.operation.clone(),
            native_request_id: entry.request["request_id"].as_u64().expect("owned contact sequence"),
            reply: entry.reply.clone(),
        };
        Contacts {
            owner: self.owner.clone(), storage_uncertain, revision: self.revision,
            remaining_requests: self.owner.request_capacity.saturating_sub(
                u64::from(self.next) - 1 + u64::from(self.pending.is_some())),
            last_completed: self.entries.iter().filter(|(_, e)| e.reply.is_some())
                .max_by_key(|(_, e)| e.request["request_id"].as_u64()).map(record),
            pending: self.pending.as_ref().and_then(|id| self.entries.get_key_value(id)).map(record),
        }
    }
    pub(super) fn require_released(&self)->io::Result<()> {
        let held=self.entries.values().filter_map(|entry|entry.reply.as_ref()).any(|reply|
            reply["status"]=="injected" && reply["contact_revision"].as_u64()==Some(self.revision)
            && reply["contacts"].as_array().is_some_and(|contacts|!contacts.is_empty()));
        if self.pending.is_some() || held {return Err(io::Error::other("resolve and release contacts before host transition"));}
        Ok(())
    }

    pub(super) fn new(owner: ContactDriver) -> Self {
        Self { owner, next: 1, revision: 0, basis: None, pending: None, entries: Default::default() }
    }
}
fn validate_tap_frames(point:&Value,reply:&Value)->io::Result<()> {
    let valid=||->Option<()> {
        let frames=reply["frames"].as_array()?;
        if frames.len()!=2 {return None;}
        let down=frames[0]["down_time_ms"].as_u64()?;
        let mut previous=down;
        for (frame,action) in frames.iter().zip([0,1]) {
            let at=frame["uptime_ms"].as_u64()?;
            let contacts=frame["contacts"].as_array()?;
            if frame["action"].as_u64()!=Some(action) || frame["accepted"]!=true
                || frame["down_time_ms"].as_u64()!=Some(down) || at<previous
                || at.checked_sub(down)?>100 || contacts.len()!=1
                || contacts[0]["id"]!=1 || contacts[0]["x"].as_f64()!=point["x"].as_f64()
                || contacts[0]["y"].as_f64()!=point["y"].as_f64() {return None;}
            previous=at;
        }
        Some(())
    };
    valid().ok_or_else(||io::Error::other("short tap lacks an accepted bounded native DOWN/UP pair"))
}
fn dimensions(bytes: &[u8]) -> io::Result<(u32,u32)> {
    if bytes.len()<24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16]!=b"IHDR" {
        return Err(io::Error::other("invalid contact capture PNG"));
    }
    let w=u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h=u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if w==0 || h==0 || w>16384 || h>16384 {return Err(io::Error::other("unsupported contact capture dimensions"));}
    Ok((w,h))
}
fn focus(raw:&str, owner:&ContactDriver)->io::Result<String> {
    let values:Vec<_>=raw.lines().filter_map(|s|s.trim().strip_prefix("mCurrentFocus=")).collect();
    if values.len()!=1 {return Err(io::Error::other("ambiguous focused window"));}
    let value=values[0];
    let component=value.strip_prefix("Window{").and_then(|s|s.strip_suffix('}'))
        .and_then(|s|s.split_whitespace().last()).ok_or_else(||io::Error::other("unavailable focused window"))?;
    let (package,activity)=component.split_once('/').ok_or_else(||io::Error::other("invalid focused component"))?;
    let activity=if activity.starts_with('.') {format!("{package}{activity}")} else {activity.into()};
    let process=owner.consumer.process();
    if package!=process.package || activity!=process.activity {
        return Err(io::Error::other("contact consumer is not focused"));
    }
    Ok(value.into())
}
impl Device {
    pub(crate) fn inspect_contact_progress(&self) -> Option<frontcap_core::native::android_progress::Contacts> {
        self.contact_state.as_ref().map(|state| state.inspect(self.contact_poisoned))
    }
    /// Read verified historical input evidence without adopting or contacting a device.
    pub(crate) fn read_contact_result(
        directory: &Path, owner: &ContactDriver, id: &RequestId,
        observation: &RequestId, operation: &Operation,
    ) -> io::Result<Option<Value>> {
        let Some(state) = frontcap_core::state::load::<Session>(&directory.join("contact-driver/state.json"))? else {
            return Ok(None);
        };
        if state.owner != *owner {
            return Err(io::Error::other("retained contact producer differs from session owner"));
        }
        let Some(entry) = state.entries.get(id) else { return Ok(None); };
        if entry.observation != *observation || entry.operation != *operation {
            return Err(io::Error::other("retained contact request differs from original input"));
        }
        // Only the locally verified completion is authoritative. Raw guest reply files may still
        // represent unknown delivery or an unverified publication; never promote those here.
        Ok(entry.reply.clone())
    }
    pub(crate) fn invalidate_contact_capture(&mut self) {
        if let Some(session)=&mut self.contact_state {session.basis=None;}
    }
    fn contact_live(&mut self, deadline:Instant,cancelled:&AtomicBool)->io::Result<ContactDriver> {
        self.check_webview_lifecycle()?;
        if self.retired.is_some() || self.contact_poisoned || !self.unretired_commands.is_empty() {return Err(io::Error::other("contact owner unavailable"));}
        let owner=self.contact_state.as_ref().ok_or_else(||io::Error::other("contact driver unavailable"))?.owner.clone();
        if self.contact_process.as_mut().ok_or_else(||io::Error::other("missing contact transport"))?.exited()? {
            return Err(io::Error::other("contact transport exited"));
        }
        self.verify_app_process(&owner.consumer.process(),deadline,cancelled)?;
        let stat=self.adb(&["shell","cat",&format!("/proc/{}/stat",owner.pid)],deadline,cancelled,false)?.unwrap();
        if crate::android_webview::start_ticks(&stat,owner.pid)?!=owner.process_start_ticks {
            return Err(io::Error::other("contact producer changed"));
        }
        Ok(owner)
    }
    fn contact_focus(&mut self,owner:&ContactDriver,deadline:Instant,cancelled:&AtomicBool)->io::Result<String> {
        let result=self.adb(&["shell","dumpsys","window"],deadline,cancelled,false)
            .and_then(|raw|focus(&raw.unwrap(),owner));
        if result.is_err() {self.invalidate_contact_capture();}
        result
    }
    fn save_contacts(&mut self, state:Session)->io::Result<()> {
        self.contact_poisoned=true;
        save_atomic(&self.host_evidence_directory().join("contact-driver/state.json"),&state)?;
        self.contact_state=Some(state);self.contact_poisoned=false;Ok(())
    }
    pub fn capture_contacts(&mut self,id:&RequestId,deadline:Instant,cancelled:&AtomicBool)->io::Result<ContactCapture> {
        self.invalidate_contact_capture();
        if self.contact_state.as_ref().is_some_and(|s|s.pending.is_some()) {
            return Err(io::Error::other("pending contact effect cannot grant a new input basis"));
        }
        let owner=self.contact_live(deadline,cancelled)?;
        let before=self.contact_focus(&owner,deadline,cancelled)?;
        let directory=self.host_evidence_directory().join("contact-driver").join(format!("capture-{}",id.as_str()));
        let path=self.capture_app_png(&directory,deadline,cancelled)?;
        let bytes=std::fs::read(&path)?;let (width,height)=dimensions(&bytes)?;
        if before!=self.contact_focus(&owner,deadline,cancelled)? {return Err(io::Error::other("contact window changed during capture"));}
        self.contact_live(deadline,cancelled)?;
        let capture=ContactCapture{id:id.clone(),path,sha256:frontcap_core::canonical::sha256_bytes_hex(&bytes),width,height,focus:before};
        save_atomic(&directory.join("basis.json"),&capture)?;
        let mut state=self.contact_state.clone().unwrap();state.basis=Some(capture.clone());self.save_contacts(state)?;
        Ok(capture)
    }
    /// Same ID only reconciles the original attempt. No uncertain publication is retried.
    pub fn input_contacts(&mut self,id:&RequestId,observation:&RequestId,operation:&Operation,
        deadline:Instant,cancelled:&AtomicBool)->io::Result<Value> {
        operation.validate().map_err(io::Error::other)?;
        let state=self.contact_state.as_ref().ok_or_else(||io::Error::other("contact driver unavailable"))?;
        if let Some(entry)=state.entries.get(id) {
            if entry.observation!=*observation || entry.operation!=*operation {return Err(io::Error::other("contact request identity conflict"));}
            return self.reconcile_contacts(id,deadline,cancelled);
        }
        if self.contact_poisoned || state.pending.is_some() || u64::from(state.next)>state.owner.request_capacity || operation.revision()!=state.revision {
            return Err(io::Error::other("pending input, exhausted producer or stale contact revision"));
        }
        if matches!(operation, Operation::Tap { .. }) {
            if !state.owner.short_tap {return Err(io::Error::other("short tap is unavailable on this contact producer"));}
            state.require_released()?;
        }
        self.check_webview_lifecycle()?;
        let basis=state.basis.as_ref().filter(|b|b.id==*observation).cloned()
            .ok_or_else(||io::Error::other("input requires latest unconsumed contact capture"))?;
        let bytes=std::fs::read(&basis.path)?;
        if frontcap_core::canonical::sha256_bytes_hex(&bytes)!=basis.sha256 {return Err(io::Error::other("contact capture bytes changed"));}
        let owner=self.contact_live(deadline,cancelled)?;
        if self.contact_focus(&owner,deadline,cancelled)?!=basis.focus {
            self.invalidate_contact_capture();return Err(io::Error::other("contact focus changed"));
        }
        let mut state=self.contact_state.clone().unwrap();
        let directory=self.host_evidence_directory().join("contact-driver").join(format!("input-{}",id.as_str()));
        std::fs::create_dir(&directory)?;
        // A fresh native raster checks display dimensions without granting a new targeting basis.
        let current=self.capture_app_png(&directory.join("geometry"),deadline,cancelled)?;
        if dimensions(&std::fs::read(current)?)? != (basis.width,basis.height) {
            self.invalidate_contact_capture();return Err(io::Error::other("contact display geometry changed"));
        }
        let mut request=json!({"generation":owner.generation,"request_id":state.next,"expected_revision":state.revision});
        match operation {
            Operation::Cancel{..}=>request["cancel"]=json!(true),
            Operation::Tap{x,y,..}=>request["tap"]=json!({
                "x":(x*f64::from(basis.width-1)) as f32 as f64,
                "y":(y*f64::from(basis.height-1)) as f32 as f64}),
            Operation::Set{contacts,..}=>request["contacts"]=json!(contacts.iter().map(|p|json!({"id":p.id,
                "x":(p.x*f64::from(basis.width-1)) as f32 as f64,"y":(p.y*f64::from(basis.height-1)) as f32 as f64})).collect::<Vec<_>>()),
        }
        save_atomic(&directory.join("request.json"),&request)?;
        state.pending=Some(id.clone());state.basis=None;
        state.entries.insert(id.clone(),Entry{observation:observation.clone(),operation:operation.clone(),request,reply:None});
        self.save_contacts(state.clone())?;
        // From this point every failure leaves the attempt pending, including lost push/move replies.
        let temporary=format!("{}/request-{}.pending",owner.guest_directory,state.next);
        let destination=format!("{}/request-{}.json",owner.guest_directory,state.next);
        self.adb(&["push",directory.join("request.json").to_str().unwrap(),&temporary],deadline,cancelled,false)?;
        self.contact_live(deadline,cancelled)?;
        if self.contact_focus(&owner,deadline,cancelled)?!=basis.focus {return Err(io::Error::other("contact focus changed before publication"));}
        self.adb(&["shell","mv",&temporary,&destination],deadline,cancelled,false)?;
        self.reconcile_contacts(id,deadline,cancelled)
    }
    pub fn reconcile_contacts(&mut self,id:&RequestId,deadline:Instant,cancelled:&AtomicBool)->io::Result<Value> {
        let mut state=self.contact_state.clone().ok_or_else(||io::Error::other("contact driver unavailable"))?;
        let entry=state.entries.get(id).ok_or_else(||io::Error::other("unknown contact request"))?.clone();
        if let Some(reply)=entry.reply {return Ok(reply);}
        let owner=state.owner.clone();
        if self.retired.is_some() || self.contact_poisoned {return Err(io::Error::other("retired or poisoned target has no retained result"));}
        self.verify_app_process(&owner.consumer.process(),deadline,cancelled)?;
        let sequence=entry.request["request_id"].as_u64().unwrap();
        let raw=loop {
            check_budget(deadline,cancelled)?;
            if let Some(raw)=self.adb(&["shell","cat",&format!("{}/reply-{sequence}.json",owner.guest_directory)],deadline,cancelled,true)? {break raw;}
            if self.contact_process.as_mut().unwrap().exited()? {return Err(io::Error::other("contact producer exited without result"));}
            std::thread::sleep(Duration::from_millis(25));
        };
        let accepted=self.adb(&["shell","cat",&format!("{}/accepted-{sequence}.json",owner.guest_directory)],deadline,cancelled,false)?.unwrap();
        let request:Value=serde_json::from_str(&accepted)?;
        if request!=entry.request {return Err(io::Error::other("producer accepted a different contact request"));}
        let reply:Value=serde_json::from_str(&raw)?;
        let status=reply["status"].as_str().unwrap_or("");
        if reply["generation"]!=owner.generation || reply["request_id"].as_u64()!=Some(sequence)
            || !matches!(status,"injected"|"rejected"|"unknown") {
            return Err(io::Error::other("invalid contact producer result"));
        }
        let directory=self.host_evidence_directory().join("contact-driver").join(format!("input-{}",id.as_str()));
        save_atomic(&directory.join("producer-reply.json"),&reply)?;
        if status=="unknown" {return Err(io::Error::other("native contact effect remains unknown"));}
        let expected=state.revision+u64::from(status=="injected");
        if reply["contact_revision"].as_u64()!=Some(expected) {return Err(io::Error::other("contact producer revision differs"));}
        if status=="injected" {
            if let Some(point)=entry.request.get("tap") {validate_tap_frames(point,&reply)?;}
            let expected_contacts=entry.request.get("contacts").cloned().unwrap_or_else(||json!([]));
            let actual=reply["contacts"].as_array().ok_or_else(||io::Error::other("missing contact result set"))?;
            let wanted=expected_contacts.as_array().unwrap();
            if actual.len()!=wanted.len() || wanted.iter().any(|p| !actual.iter().any(|q|
                q["id"]==p["id"] && q["x"].as_f64()==p["x"].as_f64() && q["y"].as_f64()==p["y"].as_f64()))
                || !reply["frames"].as_array().is_some_and(|frames|frames.len()<=16 && frames.iter().all(|f|f["accepted"]==true)) {
                return Err(io::Error::other("contact result differs from requested native state"));
            }
        }
        state.revision=expected;state.next+=1;state.pending=None;
        state.entries.get_mut(id).unwrap().reply=Some(reply.clone());self.save_contacts(state)?;
        Ok(reply)
    }
}

#[cfg(test)]
#[path = "device_contact_input_tests.rs"]
mod tests;
