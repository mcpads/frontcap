//! Native host operations on the shared owned Android worker transport.
use super::*;
use frontcap_core::write_budget::{Class, WriteBudget};
use frontcap_core::native::{android_host as host, android_webview::ContactConsumer};

impl Config {
    pub fn validate_host_ready(&self, ready: &host::Ready) -> io::Result<()> {
        self.validate()?;
        let profile = self.device.host_app.as_ref().ok_or_else(|| io::Error::other("host readiness requires host_app"))?;
        let owner = &ready.owner;
        if owner.package != profile.package || owner.activity != profile.activity
            || owner.serial != format!("emulator-{}", self.device.port)
            || owner.apk_sha256 != self.device.apk_sha256 || owner.source_sha256 != self.device.source_sha256
            || owner.pid == 0 || owner.process_start_ticks == 0 {
            return Err(io::Error::other("native host readiness differs from frozen launch inputs"));
        }
        let matches = match (&self.input, &ready.contacts) {
            (None, None) => true,
            (Some(input), Some(driver)) => driver.consumer == ContactConsumer::Host(owner.clone())
                && driver.jar_sha256 == input.sha256 && driver.request_capacity == input.request_capacity
                && driver.lifetime_ms == self.lifetime_ms,
            _ => false,
        };
        if !matches { return Err(io::Error::other("native host input producer differs from frozen launch inputs")); }
        Ok(())
    }
}
pub(super) fn read_result(directory: &Path, id: &RequestId, operation: &host::Operation) -> io::Result<Option<Reply>> {
    let launch: Launch = load(&directory.join("launch.json"))?.ok_or_else(|| io::Error::other("host launch unavailable"))?;
    let ready: host::Ready = load(&directory.join("host-ready.json"))?.ok_or_else(|| io::Error::other("host readiness unavailable"))?;
    launch.config.validate_host_ready(&ready)?;
    let initial = ready;
    let binding = epochs::binding(directory,id,&initial)?;
    let Some(epoch) = epochs::resolve(directory,&launch.config,&initial,binding.epoch.as_ref())? else { return Ok(None); };
    let ready = epoch.ready;
    if binding.owner != ready.owner { return Err(io::Error::other("host request belongs to another process epoch")); }
    let mut reply = load::<Reply>(&directory.join("requests").join(id.as_str()).join("result.json"))?;
    if matches!(reply, Some(Reply::Failed { .. })) { return Ok(reply); }
    if matches!(operation, host::Operation::AttachSpeech { .. } | host::Operation::ReadSpeech { .. }) {
        let original = speech::completion(directory,id,operation,&ready.owner)?
            .map(|completion| Reply::Completed { completion: Box::new(Completion::Host { completion }) });
        if reply.is_some() && !matches!((&reply,&original),
            (Some(Reply::Completed { completion:a }),Some(Reply::Completed { completion:b })) if a==b) {
            return Err(io::Error::other("speech result differs from original receipt"));
        }
        return Ok(original);
    }
    if let host::Operation::SetAppState { owner, state } = operation {
        if owner.as_ref()!=&ready.owner { return Err(io::Error::other("host visibility differs from request owner")); }
        let original=crate::device::read_host_transition(&directory.join("target"),&epoch.root,id,owner,*state)?;
        let recovered=original.map(|transition|Completion::Host {completion:host::Completion::StateChanged {transition:Box::new(transition)}});
        if let Some(Reply::Completed {completion})=&reply {
            if recovered.as_ref()!=Some(completion.as_ref()) { return Err(io::Error::other("host visibility lacks original SDK proof")); }
        } else if reply.is_none() { reply=recovered.map(|completion|Reply::Completed {completion:Box::new(completion)}); }
        return Ok(reply);
    }
    if let host::Operation::DispatchUrl { owner, url } = operation {
        if owner.as_ref() != &ready.owner { return Err(io::Error::other("host URL differs from request owner")); }
        let accepted = crate::device::read_host_url(&directory.join("target"), &epoch.root, id, owner, url)?;
        let recovered = accepted.map(|acceptance| Completion::Host {
            completion: host::Completion::UrlAccepted { acceptance: Box::new(acceptance) } });
        if let Some(Reply::Completed { completion }) = &reply {
            if recovered.as_ref() != Some(completion.as_ref()) {
                return Err(io::Error::other("host URL completion lacks original acceptance proof"));
            }
        } else if let Some(completion) = recovered { reply = Some(Reply::Completed { completion: Box::new(completion) }); }
        return Ok(reply);
    }
    if let host::Operation::Restart { previous, url } = operation {
        if previous.as_ref() != &ready.owner { return Err(io::Error::other("host restart differs from request owner")); }
        let Some(next) = epochs::resolve(directory,&launch.config,&initial,Some(id))? else {
            return if matches!(reply, Some(Reply::Completed { .. })) {
                Err(io::Error::other("host restart completion lacks replacement proof"))
            } else { Ok(reply) };
        };
        let replacement = crate::device::read_host_replacement(&directory.join("target"),id,previous,url.as_deref())?
            .ok_or_else(||io::Error::other("host replacement proof unavailable"))?;
        if replacement.ready != next.ready { return Err(io::Error::other("host replacement readiness differs")); }
        let recovered = Completion::Host { completion: host::Completion::Restarted { replacement:Box::new(replacement) } };
        if let Some(Reply::Completed { completion }) = &reply {
            if completion.as_ref() != &recovered {
                return Err(io::Error::other("host replacement completion differs from original proof"));
            }
        } else if reply.is_none() { reply = Some(Reply::Completed { completion:Box::new(recovered) }); }
        return Ok(reply);
    }
    if reply.is_none() {
        reply = pages::recover(directory,id,operation)?.map(|completion|Reply::Completed { completion:Box::new(Completion::Host { completion }) });
    }
    if let host::Operation::Contacts { observation, input } = operation {
        let owner = ready.contacts.as_ref().ok_or_else(|| io::Error::other("host contacts unavailable"))?;
        let original = Device::read_contact_result(&epoch.root, owner, &child_id(id,"input"), observation, input)?;
        if let Some(receipt) = original {
            let completion = host::Completion::Contacts { receipt };
            if let Some(Reply::Completed { completion: saved }) = &reply {
                if saved.as_ref() != &(Completion::Host { completion: completion.clone() }) {
                    return Err(io::Error::other("host completion differs from original contact receipt"));
                }
            } else if reply.is_none() {
                reply = Some(Reply::Completed { completion: Box::new(Completion::Host { completion }) });
            }
        } else if matches!(reply, Some(Reply::Completed { .. })) {
            return Err(io::Error::other("host completion lacks original contact receipt"));
        }
    }
    if let host::Operation::ReconcileContacts { request_id } = operation {
        let (observation,input) = contact_request(directory,request_id,&initial,&binding)?;
        let owner = ready.contacts.as_ref().ok_or_else(||io::Error::other("host contacts unavailable"))?;
        let original = Device::read_contact_result(&epoch.root,owner,&child_id(request_id,"input"),&observation,&input)?;
        if let Some(Reply::Completed { completion }) = &reply
            && !matches!(completion.as_ref(), Completion::Host { completion:host::Completion::Contacts { receipt } } if original.as_ref() == Some(receipt)) {
            return Err(io::Error::other("host reconciliation lacks original verified input"));
        }
    }
    match &reply {
        Some(Reply::Completed { completion }) => {
            let valid = match (operation, completion.as_ref()) {
                (host::Operation::Observe { input_basis }, Completion::Host { completion: host::Completion::Observed { owner, screen, bytes, sha256, basis } }) => {
                    let expected = if *input_basis {
                        epoch.root.join("contact-driver").join(format!("capture-{}",child_id(id,"basis"))).join("screen.png")
                    } else { directory.join("requests").join(id.as_str()).join("capture/screen.png") };
                    owner.as_ref() == &ready.owner && *input_basis == basis.is_some() && *bytes > 0
                        && screen == &expected && screen.is_absolute() && sha256.len() == 64
                        && basis.as_ref().is_none_or(|basis| basis.path == *screen && basis.sha256 == *sha256 && basis.id == child_id(id,"basis"))
                }
                (host::Operation::Contacts { .. } | host::Operation::ReconcileContacts { .. }, Completion::Host { completion: host::Completion::Contacts { .. } }) => true,
                (_, Completion::Host { completion }) => pages::valid_result(directory,id,operation,completion,&ready.owner)?,
                _ => false,
            };
            if !valid { return Err(io::Error::other("native host result differs from original operation or owner")); }
        }
        Some(Reply::HomeReturnFailed { .. }) => return Err(io::Error::other("unexpected page lifecycle result in host session")),
        _ => (),
    }
    Ok(reply)
}
struct Dispatch {
    id: RequestId,
    operation: Operation,
    deadline: Instant,
    request: PathBuf,
    attempt: PathBuf,
}
struct Driver<'a> {
    budget: Option<WriteBudget>,
    directory: &'a Path,
    commands: &'a mpsc::Receiver<Message>,
    cancelled: &'a AtomicBool,
    stopping: &'a AtomicBool,
    end: Instant,
    expected: u64,
    ready: host::Ready,
    epoch: Option<RequestId>,
    depth: usize,
    // Keep released page policies alive until successful process replacement or worker retirement.
    released_connections: Vec<(RequestId, cdp::Client)>,
}
impl Driver<'_> {
    fn save<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        match &self.budget {
            Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
            None => save_atomic(path, value),
        }
    }
    fn next(&mut self) -> io::Result<Option<Dispatch>> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return if self.stopping.load(Ordering::Acquire) { Ok(None) }
                    else { Err(io::Error::other("controller disconnected or sent invalid commands")) };
            }
            check(self.end, self.cancelled)?;
            let message = match self.commands.recv_timeout(Duration::from_millis(20)) { Ok(message) => message, Err(_) => continue };
            let Message::Execute { number, id, operation, deadline_ns } = &message else { return Err(io::Error::other("unexpected host session start")); };
            if *number != self.expected { return Err(io::Error::other("out-of-order native host command")); }
            self.expected += 1;
            let attempt = self.directory.join("commands").join(number.to_string());
            std::fs::create_dir(&attempt)?;
            self.save(&attempt.join("command.json"), &message)?;
            let request = self.directory.join("requests").join(id.as_str());
            if request.exists() {
                let reply = match super::read_result(self.directory, id, operation) {
                    Ok(Some(reply)) => reply,
                    Ok(None) => Reply::Failed { error: "original request unresolved; never replay".into() },
                    Err(error) => Reply::Failed { error: error.to_string() },
                };
                self.save(&attempt.join("result.json"), &reply)?;
                continue;
            }
            std::fs::create_dir(&request)?;
            self.save(&request.join("operation.json"), operation)?;
            self.save(&request.join("host-binding.json"), &epochs::Binding { owner:self.ready.owner.clone(), epoch:self.epoch.clone() })?;
            return Ok(Some(Dispatch { id: id.clone(), operation: operation.clone(),
                deadline: instant(*deadline_ns)?.min(self.end), request, attempt }));
        }
    }
    fn admit_restart(&self, previous: &frontcap_core::native::android_webview::AppProcess) -> io::Result<()> {
        if previous != &self.ready.owner || self.depth == 64 {
            return Err(io::Error::other("Android restart owner differs or replacement limit reached"));
        }
        Ok(())
    }
    fn restart(&mut self, device: &mut Device, dispatch: &Dispatch, previous: &frontcap_core::native::android_webview::AppProcess, url: Option<&str>) -> io::Result<()> {
        let work = device.restart_host(&dispatch.id,previous,url,dispatch.deadline,self.cancelled);
        match work {
            Ok(replacement) => {
                self.released_connections.clear();
                self.ready = replacement.ready.clone(); self.epoch = Some(dispatch.id.clone()); self.depth += 1;
                self.finish(dispatch,Ok(host::Completion::Restarted { replacement:Box::new(replacement) }))
            }
            Err(error) => {
                self.finish(dispatch,Err(io::Error::other(error.to_string())))?;
                Err(io::Error::other(error))
            }
        }
    }
    fn finish(&self, dispatch: &Dispatch, work: io::Result<host::Completion>) -> io::Result<()> {
        let reply = match work {
            Ok(completion) => Reply::Completed { completion: Box::new(Completion::Host { completion }) },
            Err(error) => Reply::Failed { error: error.to_string() },
        };
        #[cfg(feature = "fault-injection")]
        if std::env::var("FRONTCAP_ANDROID_WEBVIEW_FAULT").ok().as_deref() == Some(format!("{}@before_result",dispatch.id).as_str()) {
            return Err(io::Error::other("injected native host result publication failure"));
        }
        self.save(&dispatch.request.join("result.json"), &reply)?;
        self.save(&dispatch.attempt.join("result.json"), &reply)
    }
    fn visibility(&self, device: &mut Device, dispatch: &Dispatch,
        owner: &frontcap_core::native::android_webview::AppProcess, state: frontcap_core::native::app_state::State) -> io::Result<()> {
        if owner!=&self.ready.owner { return self.finish(dispatch,Err(io::Error::other("Android host visibility owner differs"))); }
        match device.set_host_state(&dispatch.id,owner,state,dispatch.deadline,self.cancelled) {
            Ok(transition)=>self.finish(dispatch,Ok(host::Completion::StateChanged {transition:Box::new(transition)})),
            Err(error)=>{
                self.finish(dispatch,Err(io::Error::other(error.to_string())))?;
                if error.retirement.is_some() {Err(io::Error::other(error))} else {Ok(())}
            }
        }
    }
    fn dispatch_url(&self, device: &mut Device, dispatch: &Dispatch,
        owner: &frontcap_core::native::android_webview::AppProcess, url: &str) -> io::Result<()> {
        if owner != &self.ready.owner {
            return self.finish(dispatch, Err(io::Error::other("Android host URL owner differs")));
        }
        match device.dispatch_host_url(&dispatch.id, owner, url, dispatch.deadline, self.cancelled) {
            Ok(acceptance) => self.finish(dispatch, Ok(host::Completion::UrlAccepted { acceptance: Box::new(acceptance) })),
            Err(error) => {
                self.finish(dispatch, Err(io::Error::other(error.to_string())))?;
                if error.retirement.is_some() { Err(io::Error::other(error)) } else { Ok(()) }
            }
        }
    }
}
impl Dispatch {
    fn operation(&self, cancelled: &AtomicBool) -> io::Result<&host::Operation> {
        check(self.deadline,cancelled)?;
        let Operation::Host { request } = &self.operation else { return Err(io::Error::other("expected native host operation")); };
        request.validate()?;
        Ok(request)
    }
}
pub(super) async fn serve(directory: &Path, config: &Config, device: &mut Device,
    commands: &mpsc::Receiver<Message>, cancelled: &AtomicBool, stopping: &AtomicBool) -> io::Result<()> {
    let end = Instant::now() + Duration::from_millis(config.lifetime_ms);
    let contacts = config.input.as_ref().map(|input| device.start_contact_driver_bounded(&input.jar, &input.sha256,
        ContactDriverLimits { lifetime: Duration::from_millis(config.lifetime_ms), request_capacity: input.request_capacity },
        end, cancelled).map_err(io::Error::other)).transpose()?;
    let ready = host::Ready { owner: device.app_process().ok_or_else(||io::Error::other("native host owner unavailable"))?.clone(), contacts };
    config.validate_host_ready(&ready)?;
    std::fs::create_dir(directory.join("commands"))?;
    std::fs::create_dir(directory.join("requests"))?;
    std::fs::create_dir(directory.join("pages"))?;
    let budget = WriteBudget::from_environment()?;
    match &budget {
        Some(budget) => budget.save_atomic(Class::Ordinary, &directory.join("host-ready.json"), &ready)?,
        None => save_atomic(&directory.join("host-ready.json"), &ready)?,
    }
    let mut driver = Driver { budget, directory, commands, cancelled, stopping, end, expected: 1, ready, epoch: None, depth: 0, released_connections: Vec::new() };
    while let Some(dispatch) = driver.next()? {
        let operation = match dispatch.operation(cancelled) {
            Ok(operation) => operation,
            Err(error) => { driver.finish(&dispatch,Err(error))?; continue; },
        };
        if let host::Operation::SetAppState {owner,state}=operation {
            driver.visibility(device,&dispatch,owner,*state)?; continue;
        }
        if let Err(error)=device.require_host_foreground() {driver.finish(&dispatch,Err(error))?;continue;}
        if matches!(operation, host::Operation::AttachSpeech { .. }) {
            driver.attach_speech(device,&dispatch,operation)?; continue;
        }
        if let host::Operation::Restart { previous, url } = operation {
            if let Err(error) = driver.admit_restart(previous) { driver.finish(&dispatch,Err(error))?; continue; }
            driver.restart(device,&dispatch,previous,url.as_deref())?;
        } else if let host::Operation::DispatchUrl { owner, url } = operation {
            driver.dispatch_url(device,&dispatch,owner,url)?;
        } else if matches!(operation, host::Operation::AttachPage { .. } | host::Operation::AttachDocument { .. }) {
            pages::attach_and_serve(&mut driver,device,&dispatch,operation).await?;
        } else {
            let work = driver.execute(device,&dispatch,operation);
            driver.finish(&dispatch,work)?;
        }
    }
    Ok(())
}

fn contact_request(directory: &Path, id: &RequestId, initial: &host::Ready, binding: &epochs::Binding)
    -> io::Result<(RequestId,frontcap_core::native::contacts::Operation)> {
    let original = epochs::binding(directory,id,initial)?;
    if original.owner != binding.owner || original.epoch != binding.epoch {
        return Err(io::Error::other("contact request belongs to another host process; read its original result"));
    }
    let operation: Operation = load(&directory.join("requests").join(id.as_str()).join("operation.json"))?
        .ok_or_else(||io::Error::other("original host input unavailable"))?;
    let Operation::Host { request:host::Operation::Contacts { observation,input } } = operation else {
        return Err(io::Error::other("reconciliation requires an original contact request"));
    };
    Ok((observation,input))
}
impl Driver<'_> {
    fn attach_speech(&self, device: &mut Device, dispatch: &Dispatch, operation: &host::Operation) -> io::Result<()> {
        let host::Operation::AttachSpeech { owner, library, sha256 } = operation else { unreachable!() };
        if owner.as_ref() != &self.ready.owner {
            return self.finish(dispatch,Err(io::Error::other("speech attachment owner differs")));
        }
        let marker = device.host_evidence_directory().join("speech-attachment-intent.json");
        if marker.exists() { return self.finish(dispatch,Err(io::Error::other("speech attachment already attempted; use the original request"))); }
        match device.attach_speech(&dispatch.id,owner,(library,sha256),&dispatch.request,dispatch.deadline,self.cancelled) {
            Ok((attachment,history)) => self.finish(dispatch,Ok(host::Completion::SpeechAttached { attachment:Box::new(attachment),history:Box::new(history) })),
            Err(error) => {
                self.finish(dispatch,Err(io::Error::other(error.to_string())))?;
                // Admission failures without guest mutation do not retire a healthy target.
                if marker.exists() { Err(error) } else { Ok(()) }
            }
        }
    }
    fn execute(&self, device: &mut Device, dispatch: &Dispatch, operation: &host::Operation) -> io::Result<host::Completion> {
        if let host::Operation::ReadSpeech { attachment } = operation {
            let original = speech::attachment(self.directory,attachment,&self.ready.owner)?;
            let history = device.read_speech(&dispatch.id,&original,&dispatch.request,dispatch.deadline,self.cancelled)?;
            return Ok(host::Completion::SpeechRead { attachment:attachment.clone(),history:Box::new(history) });
        }
        if let host::Operation::ReconcileContacts { request_id } = operation {
            let initial: host::Ready = load(&self.directory.join("host-ready.json"))?
                .ok_or_else(||io::Error::other("initial host readiness unavailable"))?;
            contact_request(self.directory,request_id,&initial,&epochs::Binding { owner:self.ready.owner.clone(),epoch:self.epoch.clone() })?;
        }
        execute(device,&dispatch.request.join("capture"),&dispatch.id,operation,dispatch.deadline,self.cancelled)
    }
}

fn execute(device: &mut Device, capture_directory: &Path, id: &RequestId, operation: &host::Operation, deadline: Instant, cancelled: &AtomicBool) -> io::Result<host::Completion> {
    match operation {
        host::Operation::AttachSpeech { .. } | host::Operation::ReadSpeech { .. } => Err(io::Error::other("speech operation requires host request binding")),
        host::Operation::DiscoverPages => pages::discover(device,capture_directory,deadline,cancelled),
        host::Operation::SetAppState { .. } | host::Operation::Restart { .. } | host::Operation::DispatchUrl { .. } | host::Operation::AttachPage { .. } | host::Operation::AttachDocument { .. } | host::Operation::ReleasePage { .. } | host::Operation::RetirePage { .. } | host::Operation::Page { .. } =>
            Err(io::Error::other("operation requires the matching active page attachment")),
        host::Operation::Observe { input_basis } => {
            let basis = if *input_basis { Some(device.capture_contacts(&child_id(id,"basis"),deadline,cancelled)?) } else { None };
            let screen = match &basis {
                Some(basis) => basis.path.clone(),
                None => device.capture_app_png(capture_directory,deadline,cancelled)?,
            };
            let bytes = std::fs::read(&screen)?;
            Ok(host::Completion::Observed { owner: Box::new(device.app_process().unwrap().clone()), screen,
                bytes: bytes.len() as u64, sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes), basis })
        }
        host::Operation::Contacts { observation, input } => Ok(host::Completion::Contacts {
            receipt: device.input_contacts(&child_id(id,"input"),observation,input,deadline,cancelled)? }),
        host::Operation::ReconcileContacts { request_id } => Ok(host::Completion::Contacts {
            receipt: device.reconcile_contacts(&child_id(request_id,"input"),deadline,cancelled)? }),
    }
}

#[path = "android_host_pages.rs"]
mod pages;

#[path = "android_host_epochs.rs"]
mod epochs;

#[path = "android_host_speech.rs"]
mod speech;

#[cfg(test)]
#[path = "android_host_session_tests.rs"]
mod budget_tests;
