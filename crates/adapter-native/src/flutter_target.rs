//! Compose the live Flutter worker, durable requests and typed HTTP transport.
use crate::{
    flutter_bridge,
    flutter_session::{self, Session},
    flutter_store::{Owner, Recovery, RequestStore},
};
use frontcap_core::{
    encode::{base64, unbase64},
    ids::RequestId,
    native::{
        flutter::{Capture, Completion, Operation, Response},
        ownership::Retirement,
    },
    state::{load, save_atomic},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Instant,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub worker: flutter_session::Launch,
    pub http_script: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url_define: Option<String>,
}
pub struct Target {
    session: Option<Session>,
    store: RequestStore,
    ready: flutter_session::Ready,
    config: Launch,
    directory: PathBuf,
    entries: Option<crate::external_entry::Dispatches>,
    system: Option<crate::flutter_system_ui::Store>,
    initial_owner: Owner,
    restarts: Option<crate::flutter_restarts::History>,
    app_states: Option<crate::flutter_app_state::History>,
    budget: Option<frontcap_core::write_budget::WriteBudget>,
    retired: Vec<(RequestStore, Option<crate::external_entry::Dispatches>)>,
}
impl Target {
    pub fn acquire(
        config: Launch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<(Self, Owner)> {
        Self::acquire_with_budget(config, directory, deadline, cancelled, None)
    }
    pub fn acquire_with_budget(config: Launch, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
        budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<(Self, Owner)> {
        std::fs::create_dir(directory)?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("launch.json"), &config)?,
            None => save_atomic(&directory.join("launch.json"), &config)?,
        }
        let mut session =
            Session::start_with_budget(config.worker.clone(), &directory.join("session"), deadline, budget)?;
        let ready = session.wait_ready(deadline, cancelled)?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("ready.json"), &ready.owner)?,
            None => save_atomic(&directory.join("ready.json"), &ready.owner)?,
        }
        let store = RequestStore::create_with_budget(&directory.join("requests"), ready.owner.clone(), budget)
            .map_err(io::Error::other)?;
        std::fs::create_dir(directory.join("exchanges"))?;
        std::fs::create_dir(directory.join("captures"))?;
        let owner = ready.owner.clone();
        let entries = ready.external_url_target.clone().map(|target|
            crate::external_entry::Dispatches::create_with_budget(&directory.join("external-entry"), target, owner.generation.clone(), budget)).transpose()?;
        let system = ready.system_ui.clone().map(|runner| crate::flutter_system_ui::Store::create_with_budget(
            &directory.join("system-ui"), owner.clone(), runner, budget)).transpose()?;
        let restarts = if let Some(product) = &config.worker.config.installed_app {
            let app = load(&directory.join("session/worker/application.json"))?
                .ok_or_else(||io::Error::other("missing installed Flutter app evidence"))?;
            product.validate_acquired(&app, &ready)?;
            Some(crate::flutter_restarts::History::create_with_budget(directory, ready.clone(), app, budget)?)
        } else { None };
        let app_states = if restarts.is_some() && (owner.platform == frontcap_core::native::flutter::Platform::Android
            || load::<bool>(&directory.join("session/worker/system-ui/host-app-state.json"))? == Some(true)) {
            Some(crate::flutter_app_state::History::create_with_budget(directory,owner.clone(),budget)?)
        } else { None };
        Ok((
            Self {
                session: Some(session),
                store,
                ready,
                config,
                directory: directory.into(),
                entries,
                system,
                initial_owner: owner.clone(),
                restarts,
                app_states,
                retired: vec![],
                budget: budget.cloned(),
            },
            owner,
        ))
    }
    pub fn validate_operation(&self, id: &RequestId, operation: &Operation) -> io::Result<()> {
        self.validate_current_id(id)?;
        if let Some(system) = &self.system {
            system.ensure_resolved()?;
            if system.contains(id) { return Err(io::Error::other("Flutter request ID belongs to system UI")); }
        }
        if let Some(entries) = &self.entries {
            entries.ensure_resolved()?;
            if entries.contains(id) { return Err(io::Error::other("Flutter request ID belongs to external entry")); }
        }
        if matches!(operation, Operation::Stop {}) {
            return Err(io::Error::other("use target stop for retirement"));
        }
        self.store
            .validate_operation(id, operation)
            .map_err(io::Error::other)
    }
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Completion> {
        self.validate_operation(&id, &operation)?;
        if matches!(operation, Operation::Stop {}) {
            return Err(io::Error::other("use target stop for owned retirement"));
        }
        let changes_basis = !matches!(operation, Operation::Read {});
        match self
            .store
            .prepare(id.clone(), operation)
            .map_err(io::Error::other)?
        {
            Recovery::Completed(receipt) => {
                verify_capture(&self.directory, &id, &receipt)?;
                return Ok(*receipt);
            }
            Recovery::Reconcile(_) => {
                return Err(io::Error::other(
                    "Flutter request remains dispatched; no redispatch authority",
                ));
            }
            Recovery::Prepared(_) => {}
        }
        if changes_basis && let Some(system) = &mut self.system { system.invalidate(&id)?; }
        let mut command = Command::new(&self.config.worker.python);
        command.arg(&self.config.http_script);
        let exchange = flutter_bridge::dispatch(
            &mut self.store,
            &id,
            &mut command,
            self.ready.endpoint.clone(),
            deadline,
            cancelled,
            &self.directory.join("exchanges").join(id.as_str()),
        )
        .map_err(io::Error::other)?;
        if let Some(error) = exchange
            .failure
            .or(exchange.cleanup_failure)
            .or(exchange.evidence_failure)
        {
            return Err(io::Error::other(error));
        }
        let Some(reply) = exchange.reply else {
            return Err(io::Error::other("Flutter exchange has no typed response"));
        };
        let Response::Completed(receipt) = reply.response else {
            return Err(io::Error::other(
                "Flutter request did not return terminal completion",
            ));
        };
        let receipt = *receipt;
        if let Some(Capture::Complete { png_base64, .. }) = &receipt.capture {
            let data = decode_png(png_base64)?;
            save_capture(&self.directory.join("captures").join(format!("{id}.png")), &data, self.budget.as_ref())?;
        }
        self.store
            .complete(&id, receipt.clone())
            .map_err(io::Error::other)?;
        Ok(receipt)
    }
    pub fn external_url_target(&self) -> Option<&frontcap_core::native::external_entry::Target> {
        self.ready.external_url_target.as_ref()
    }
    pub fn validate_external_entry(&self, id: &RequestId, request: &frontcap_core::native::external_entry::Request) -> io::Result<()> {
        self.validate_current_id(id)?;
        self.ready.owner.validate_external_target(&request.target).map_err(io::Error::other)?;
        request.validate().map_err(io::Error::other)?;
        if self.ready.external_url_target.as_ref() != Some(&request.target) || request.generation != self.ready.owner.generation {
            return Err(io::Error::other("external entry differs from acquired Flutter application"));
        }
        let entries = self.entries.as_ref().ok_or_else(|| io::Error::other("Flutter URL dispatch unavailable"))?;
        if !entries.contains(id) {
            if let Some(system) = &self.system {
                system.ensure_resolved()?;
                if system.contains(id) { return Err(io::Error::other("URL ID belongs to system UI")); }
            }
            entries.ensure_resolved()?;
            self.store.validate_external_entry(id).map_err(io::Error::other)?;
        }
        Ok(())
    }
    pub fn dispatch_url(&mut self, id: RequestId, request: frontcap_core::native::external_entry::Request,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<frontcap_core::native::external_entry::Accepted> {
        self.validate_external_entry(&id, &request)?;
        let entries = self.entries.as_mut().unwrap();
        let admission = entries.directory().join(format!("admission-{id}.json"));
        entries.dispatch(&id, request, |request| {
            let session = self.session.as_mut().ok_or_else(|| io::Error::other("Flutter owner retired"))?;
            if let frontcap_core::native::external_entry::Target::Ios { bundle, .. } = &request.target {
                let inventory = session.installed_apps(deadline, cancelled)?;
                let declarations = crate::ios_external_entry::inspect(&inventory, bundle, &request.url, || {
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline {
                        Err(io::Error::other("Flutter URL admission cancelled or expired"))
                    } else { Ok(()) }
                })?;
                save_atomic(&admission, &declarations)?;
            }
            if let Some(system) = &mut self.system { system.invalidate(&id)?; }
            self.store.invalidate_for_external_entry(&id).map_err(io::Error::other)?;
            session.open_url(request, deadline, cancelled)
        })
    }
    pub fn system_ui(&self) -> Option<&frontcap_core::native::ios_ui::UiIdentity> { self.ready.system_ui.as_ref() }
    pub fn validate_system_operation(&self, id: &RequestId, operation: &frontcap_core::native::ios_request::Operation) -> io::Result<()> {
        let system = self.system.as_ref().ok_or_else(|| io::Error::other("Flutter system UI unavailable"))?;
        system.validate(id, operation)?;
        if !system.contains(id) {
            if let Some(history) = &self.restarts { history.ensure_system_available()?; }
            self.validate_lifecycle_id(id)?;
            if let Some(entries) = &self.entries {
                entries.ensure_resolved()?;
                if entries.contains(id) { return Err(io::Error::other("system UI ID belongs to a URL")); }
            }
            if self.pending_entry()?.is_some() {
                if self.store.contains(id) { return Err(io::Error::other("system request ID belongs to the retired app")); }
            } else { self.store.validate_external_entry(id).map_err(io::Error::other)?; }
        }
        Ok(())
    }
    pub fn execute_system(&mut self, id: RequestId, operation: frontcap_core::native::ios_request::Operation,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<frontcap_core::native::ios_ui::UiCompletion> {
        self.validate_system_operation(&id, &operation)?;
        let runner = self.ready.system_ui.as_ref().unwrap().clone();
        if self.system.as_ref().unwrap().contains(&id) {
            return crate::flutter_system_ui::read_completion(&self.directory, &self.initial_owner, &runner, &id, &operation)?
                .ok_or_else(|| io::Error::other("system UI effect unresolved; no redispatch authority"));
        }
        let request = self.system.as_mut().unwrap().dispatch(id.clone(), &operation)?;
        if self.pending_entry()?.is_none() { self.store.invalidate_for_system_operation(&id).map_err(io::Error::other)?; }
        let call = crate::ios::UiCall::Execute { identity: runner.clone(), app: None, request };
        let session = self.session.as_mut().ok_or_else(|| io::Error::other("Flutter worker retired"))?;
        let crate::ios::UiReply::Completed { completion } = session.system_ui(&call, deadline, cancelled)? else {
            return Err(io::Error::other("system UI result unresolved"));
        };
        crate::ios_receipt::verify_system(&crate::flutter_system_ui::client_directory(&self.directory), &runner, &completion)?;
        self.system.as_mut().unwrap().complete(&id, completion.as_ref().clone())?;
        Ok(*completion)
    }
    fn validate_current_id(&self, id: &RequestId) -> io::Result<()> {
        if let Some(history)=&self.app_states { history.ensure_foreground(&self.ready.owner)?; }
        if let Some(history) = &self.restarts { history.ensure_resolved()?; }
        self.validate_lifecycle_id(id)
    }
    fn validate_lifecycle_id(&self, id: &RequestId) -> io::Result<()> {
        if let Some(history)=&self.app_states {
            history.ensure_resolved()?;
            if history.contains(id) { return Err(io::Error::other("request ID belongs to Flutter host transition")); }
        }
        if let Some(history) = &self.restarts
            && history.contains(id) { return Err(io::Error::other("request ID belongs to Flutter restart")); }
        if self.retired.iter().any(|(store, entries)| store.contains(id) || entries.as_ref().is_some_and(|e| e.contains(id))) {
            return Err(io::Error::other("request ID belongs to an earlier Flutter generation"));
        }
        Ok(())
    }
    pub fn app_lifecycle(&self) -> bool { self.app_states.is_some() }
    pub fn validate_set_app_state(&self,id:&RequestId,generation:&str,state:frontcap_core::native::app_state::State)->io::Result<()> {
        if generation != self.ready.owner.generation {return Err(io::Error::other("Flutter host generation differs"));}
        let history=self.app_states.as_ref().ok_or_else(||io::Error::other("Flutter host lifecycle unavailable"))?;
        history.validate_request(id,&self.ready,state)?;
        if history.contains(id) {return Ok(());}
        self.validate_lifecycle_id(id)?;
        self.restarts.as_ref().ok_or_else(||io::Error::other("Flutter standalone app unavailable"))?.ensure_resolved()?;
        self.store.validate_external_entry(id).map_err(io::Error::other)?;
        if let Some(entries)=&self.entries {
            entries.ensure_resolved()?;
            if entries.contains(id){return Err(io::Error::other("host request ID belongs to URL work"));}
        }
        if let Some(system)=&self.system {
            system.ensure_resolved()?;
            if system.contains(id){return Err(io::Error::other("host request ID belongs to system work"));}
        }
        Ok(())
    }
    pub fn set_app_state(&mut self,id:RequestId,generation:String,state:frontcap_core::native::app_state::State,
        deadline:Instant,cancelled:&AtomicBool)->io::Result<frontcap_core::native::flutter_lifecycle::Transition> {
        self.validate_set_app_state(&id,&generation,state)?;
        if self.app_states.as_ref().unwrap().contains(&id) {
            return crate::flutter_app_state::read_completion(&self.directory,&self.ready.owner,&id,state)?
                .ok_or_else(||io::Error::other("Flutter host transition unresolved; no replay authority"));
        }
        let application=self.restarts.as_ref().unwrap().current_application().clone();
        let request=if let crate::flutter_app::Acquired::Ios(app)=&application {
            let operation=frontcap_core::native::ios_ui::UiOperation::SetHostAppState {
                bundle:app.bundle.clone(),pid:app.process.pid,state};
            Some(self.system.as_mut().ok_or_else(||io::Error::other("independent runner unavailable"))?.dispatch_ui(id.clone(),&operation)?)
        } else {None};
        let session=self.session.as_mut().ok_or_else(||io::Error::other("Flutter worker retired"))?;
        self.app_states.as_mut().unwrap().dispatch(id.clone(),self.ready.clone(),application.clone(),state,session.next_host_number()?,request.clone())?;
        self.store.invalidate_for_system_operation(&id).map_err(io::Error::other)?;
        let call=request.map(|request|crate::ios::UiCall::Execute {
            identity:self.ready.system_ui.as_ref().unwrap().clone(),app:None,request});
        let changed=session.set_app_state(&generation,state,call.as_ref(),deadline,cancelled)?;
        changed.validate(&application,&self.ready,state)?;
        if let crate::flutter_app_state::Native::Ios {completion}=&changed.native {
            crate::ios_receipt::verify_system(&crate::flutter_system_ui::client_directory(&self.directory),
                self.ready.system_ui.as_ref().unwrap(),completion)?;
            self.system.as_mut().unwrap().complete(&id,completion.as_ref().clone())?;
        }
        self.app_states.as_mut().unwrap().complete(&id,changed)
    }
    pub fn pending_entry(&self) -> io::Result<Option<frontcap_core::native::flutter_entry::Pending>> {
        self.restarts.as_ref().map(|h| h.pending_entry()).transpose().map(Option::flatten)
    }
    pub fn validate_begin_entry(&self, id: &RequestId, generation: &str, url: &str) -> io::Result<()> {
        self.validate_restart(id, generation, None)?;
        crate::flutter_ios_entry::request(&self.ready, url).map(|_| ())
    }
    pub fn begin_entry(&mut self, id: RequestId, generation: String, url: String, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<frontcap_core::native::flutter_entry::Pending> {
        self.validate_begin_entry(&id, &generation, &url)?;
        let session = self.session.as_mut().ok_or_else(|| io::Error::other("Flutter worker retired"))?;
        let admission = session.prepare_entry(&self.ready, &url, deadline, cancelled)?;
        let number = admission.command_number();
        let digest = frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(self.directory
            .join("session/worker/entry-admissions").join(format!("{number}.json")))?);
        let history = self.restarts.as_mut().unwrap();
        let previous = history.current_application().clone();
        history.dispatch_entry(id.clone(), generation, url, number, digest)?;
        self.system.as_mut().unwrap().invalidate(&id)?;
        let started = session.begin_entry(admission, &previous, &self.ready, deadline, cancelled)?;
        history.accept_entry(&id, started)
    }
    pub fn validate_poll_entry(&self, id: &RequestId, begin: &RequestId, generation: &str) -> io::Result<()> {
        self.restarts.as_ref().ok_or_else(|| io::Error::other("Flutter lifecycle unavailable"))?.validate_poll(id, begin, generation)?;
        self.validate_lifecycle_id(id)?;
        if self.store.contains(id) { return Err(io::Error::other("poll ID belongs to app work")); }
        if let Some(entries) = &self.entries {
            entries.ensure_resolved()?;
            if entries.contains(id) { return Err(io::Error::other("poll ID belongs to URL work")); }
        }
        let system = self.system.as_ref().ok_or_else(|| io::Error::other("system runner unavailable"))?;
        system.ensure_resolved()?;
        if system.contains(id) { return Err(io::Error::other("poll ID belongs to system work")); }
        Ok(())
    }
    pub fn poll_entry(&mut self, id: RequestId, begin: RequestId, generation: String, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<frontcap_core::native::flutter_entry::Progress> {
        self.validate_poll_entry(&id, &begin, &generation)?;
        let session = self.session.as_mut().ok_or_else(|| io::Error::other("Flutter worker retired"))?;
        let (started, previous) = self.restarts.as_mut().unwrap().dispatch_poll(id.clone(), &begin, &generation, session.next_host_number()?)?;
        let result = session.poll_entry(&started, &self.ready, previous.as_ref(), deadline, cancelled)?;
        self.restarts.as_ref().unwrap().validate_polled(&id, &result)?;
        let next = if let crate::flutter_ios_entry::Polled::Ready { replacement, .. } = &result {
            self.system.as_mut().unwrap().invalidate(&id)?;
            Some((replacement.ready.clone(), self.prepare_epoch(replacement)?))
        } else { None };
        let progress = self.restarts.as_mut().unwrap().complete_poll(&id, result)?;
        if let Some((ready, (store, entries))) = next { self.publish_epoch(ready, store, entries); }
        Ok(progress)
    }
    pub fn restart_available(&self) -> bool { self.restarts.is_some() }
    pub fn validate_restart(&self, id: &RequestId, generation: &str, url: Option<&str>) -> io::Result<()> {
        if !self.restart_available() || self.ready.owner.generation != generation {
            return Err(io::Error::other("Flutter restart generation or capability differs"));
        }
        self.validate_current_id(id)?;
        frontcap_core::native::flutter_lifecycle::restart_entry(&self.ready.owner, self.ready.external_url_target.as_ref(), url)
            .map_err(io::Error::other)?;
        self.store.validate_replacement(id).map_err(io::Error::other)?;
        if let Some(entries) = &self.entries {
            entries.ensure_resolved()?;
            if entries.contains(id) { return Err(io::Error::other("restart ID belongs to a URL")); }
        }
        if let Some(system) = &self.system {
            system.ensure_resolved()?;
            if system.contains(id) { return Err(io::Error::other("restart ID belongs to system UI")); }
        }
        Ok(())
    }
    pub fn restart(&mut self, id: RequestId, generation: String, url: Option<String>, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<frontcap_core::native::flutter_lifecycle::Replacement> {
        self.validate_restart(&id, &generation, url.as_deref())?;
        let session = self.session.as_mut().ok_or_else(|| io::Error::other("Flutter owner retired"))?;
        self.restarts.as_mut().unwrap().dispatch(id.clone(), generation.clone(), session.next_host_number()?, url.clone())?;
        if let Some(system) = &mut self.system { system.invalidate(&id)?; }
        let result = session.restart_app(&generation, url.as_deref(), deadline, cancelled)?;
        self.restarts.as_ref().unwrap().validate_completion(&result)?;
        let (store, entries) = self.prepare_epoch(&result)?;
        let replacement = self.restarts.as_mut().unwrap().complete(&id, result.clone())?;
        self.publish_epoch(result.ready, store, entries);
        Ok(replacement)
    }
    fn prepare_epoch(&self, result: &crate::flutter_app::Restarted)
        -> io::Result<(RequestStore, Option<crate::external_entry::Dispatches>)> {
        let directory = crate::flutter_restarts::epoch_directory(&self.directory, &result.ready.owner.generation);
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        match &self.budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("ready.json"), &result.ready.owner)?,
            None => save_atomic(&directory.join("ready.json"), &result.ready.owner)?,
        }
        let store = RequestStore::create_with_budget(&directory.join("requests"), result.ready.owner.clone(), self.budget.as_ref()).map_err(io::Error::other)?;
        let entries = result.ready.external_url_target.clone().map(|target| crate::external_entry::Dispatches::create_with_budget(
            &directory.join("external-entry"), target, result.ready.owner.generation.clone(), self.budget.as_ref())).transpose()?;
        Ok((store, entries))
    }
    fn publish_epoch(&mut self, ready: flutter_session::Ready, store: RequestStore, entries: Option<crate::external_entry::Dispatches>) {
        self.retired.push((std::mem::replace(&mut self.store, store), std::mem::replace(&mut self.entries, entries)));
        self.ready = ready;
    }
    pub fn stop(mut self, deadline: Instant) -> io::Result<Retirement> {
        let session = self
            .session
            .take()
            .ok_or_else(|| io::Error::other("Flutter owner already stopped"))?;
        match session.stop(deadline)? {
            flutter_session::Recovery::Finished(result) if result.retirement.passed => {
                Ok(result.retirement)
            }
            _ => Err(io::Error::other("Flutter owned retirement unresolved")),
        }
    }
}
pub fn decode_png(value: &str) -> io::Result<Vec<u8>> {
    if value.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("Flutter raster exceeds transport bound"));
    }
    let bytes = unbase64(value).ok_or_else(|| io::Error::other("invalid Flutter base64"))?;
    if base64(&bytes) != value {
        return Err(io::Error::other("noncanonical Flutter base64"));
    }
    let mut decoder = png::Decoder::new(io::Cursor::new(&bytes));
    decoder.set_limits(png::Limits {
        bytes: 64 * 1024 * 1024,
    });
    let mut reader = decoder.read_info().map_err(io::Error::other)?;
    let info = reader.info();
    if info.width == 0
        || info.height == 0
        || info.width > 8192
        || info.height > 8192
        || u64::from(info.width) * u64::from(info.height) > 16_000_000
        || info.animation_control.is_some()
    {
        return Err(io::Error::other(
            "invalid Flutter raster dimensions or animation",
        ));
    }
    let size = reader
        .output_buffer_size()
        .filter(|n| *n <= 64 * 1024 * 1024)
        .ok_or_else(|| io::Error::other("Flutter raster decode bound exceeded"))?;
    let mut pixels = vec![0; size];
    reader.next_frame(&mut pixels).map_err(io::Error::other)?;
    reader.finish().map_err(io::Error::other)?;
    Ok(bytes)
}
pub fn verify_capture(directory: &Path, id: &RequestId, receipt: &Completion) -> io::Result<()> {
    if let Some(Capture::Complete { png_base64, .. }) = &receipt.capture {
        let expected = decode_png(png_base64)?;
        let path = directory.join("captures").join(format!("{id}.png"));
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.is_file()
            || meta.len() != expected.len() as u64
            || std::fs::read(path)? != expected
        {
            return Err(io::Error::other(
                "Flutter capture artifact differs from completion",
            ));
        }
    }
    Ok(())
}
pub fn read_completion(
    directory: &Path,
    owner: &Owner,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<Completion>> {
    let epoch = crate::flutter_restarts::epochs(directory)?.into_iter().find(|(ready, _)| &ready.owner == owner)
        .ok_or_else(||io::Error::other("Flutter owner differs from acquired generations"))?.1;
    let receipt = RequestStore::read_completion(&epoch.join("requests"), owner, id, operation)
        .map_err(io::Error::other)?;
    if let Some(receipt) = &receipt {
        verify_capture(directory, id, receipt)?;
    }
    Ok(receipt)
}

fn save_capture(path: &Path, data: &[u8], budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<()> {
    if let Some(budget) = budget {
        budget.charge(frontcap_core::write_budget::Class::Ordinary, data.len() as u64)?;
    }
    let mut file = File::create_new(path)?;
    file.write_all(data)?;
    file.sync_all()
}


#[cfg(test)]
#[path = "flutter_target_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "flutter_capture_budget_tests.rs"]
mod capture_budget_tests;
