//! Durable native-host operations within the shared iOS session worker.
use crate::{
    ios_host::{Config, Target},
    webview_session::{Completion as PageCompletion, Reply},
};
use frontcap_core::{
    canonical,
    ids::RequestId,
    native::ios_host::{Completion, Inventory, Operation, Owner, PageOperation, Ready},
    state::load,
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use std::{io, path::Path, sync::atomic::AtomicBool, time::Instant};
#[path = "ios_host_epochs.rs"]
mod epochs;
#[path = "ios_host_entry_results.rs"]
mod entry_results;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    owner: Owner,
    operation: Operation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<RequestId>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum Saved {
    Completed { completion: Completion },
    Failed { error: String },
}
fn bad(s: impl ToString) -> io::Error {
    io::Error::other(s.to_string())
}
fn required<T: serde::de::DeserializeOwned>(p: &Path) -> io::Result<T> {
    load(p)?.ok_or_else(|| bad(format!("missing native host evidence: {}", p.display())))
}
impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if self.lifetime_ms == 0 || self.lifetime_ms > 900_000 { return Err(bad("iOS resource lifetime must be 1..900000 milliseconds")); }
        if self.app.page_registry != self.app.script.is_some() {
            return Err(bad(
                "native host script requires explicit page registry mode",
            ));
        }
        if let Some(system) = &self.system_ui { system.validate()?; }
        if let Some(prepared) = &self.app.prepared { prepared.validate(&self.app)?; }
        Ok(())
    }
    pub fn validate_ready(&self, ready: &Ready) -> io::Result<()> {
        self.validate()?;
        ready.owner.validate().map_err(bad)?;
        if ready.owner.attachment_sha256 != self.app.attachment_sha256()
            || self.contacts != ready.contacts.is_some()
            || self.app.page_registry != ready.pages
        {
            return Err(bad("native host readiness differs from launch"));
        }
        if ready.app_lifecycle && ready.system_ui.is_none() { return Err(bad("host lifecycle requires system runner")); }
        if self.system_ui.is_some() != ready.system_ui.is_some() { return Err(bad("system runner availability differs")); }
        if let Some(runner) = &ready.system_ui {
            crate::ios_system_store::Consumer::validate_runner(&ready.owner, runner)?;
        }
        if let Some(endpoint) = &ready.contacts {
            endpoint.validate(ready.owner.process.pid).map_err(bad)?;
        }
        Ok(())
    }
}
pub(crate) fn execute(
    target: &mut Target,
    directory: &Path,
    id: &RequestId,
    operation: &Operation,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Completion> {
    let budget = WriteBudget::from_environment()?;
    let directory = &directory.canonicalize()?;
    operation.validate().map_err(bad)?;
    let requests = directory.join("host-requests");
    std::fs::create_dir_all(&requests)?;
    let root = requests.join(id.as_str());
    if root.exists() {
        let binding: Binding = required(&root.join("request.json"))?;
        if &binding.operation != operation {
            return Err(bad("native host request identity conflict"));
        }
        let launch: crate::webview_session::Launch = required(&directory.join("launch.json"))?;
        let config = Config { lifetime_ms: launch.config.lifetime_ms, simulator: launch.config.simulator, app: launch.config.app, contacts: launch.config.contacts, system_ui: launch.config.system_ui };
        return match read_result(directory, &config, id, operation)? {
            Some(Reply::Completed { completion }) => match *completion {
                PageCompletion::Host { completion } => Ok(completion),
                _ => Err(bad("saved host completion family differs")),
            },
            Some(Reply::Failed { error }) => Err(bad(error)),
            None => Err(bad("original native host request remains unresolved")),
        };
    }
    target.admit_entry_operation(operation)?;
    target.admit_visibility(operation)?;
    if operation.requires_contacts() && target.contact_endpoint().is_none() {
        return Err(bad("native host input unavailable"));
    }
    if operation.requires_pages() && !target.pages_available() {
        return Err(bad("native host page registry unavailable"));
    }
    if matches!(operation, Operation::DispatchUrl { owner, .. } if owner.as_ref() != target.owner()) {
        return Err(bad("native host URL owner differs"));
    }
    let epoch = epochs::current(directory, target.evidence_directory())?;
    if let Operation::Restart { previous } | Operation::BeginEntry { previous, .. } = operation {
        if previous != target.owner() { return Err(bad("native host restart owner differs")); }
        let launch: crate::webview_session::Launch = required(&directory.join("launch.json"))?;
        let config = Config { lifetime_ms: launch.config.lifetime_ms, simulator: launch.config.simulator, app: launch.config.app, contacts: launch.config.contacts, system_ui: launch.config.system_ui };
        let initial = required(&directory.join("host-ready.json"))?;
        config.validate_ready(&initial)?;
        let state = epochs::resolve(directory, &config, &initial, epoch.as_ref())?
            .ok_or_else(|| bad("current host epoch is unpublished"))?;
        if &state.ready.owner != target.owner() { return Err(bad("current host epoch owner differs")); }
        if state.depth >= 64 { return Err(bad("native host restart capacity exhausted")); }
    }
    target.prepare_system(id, operation)?;
    std::fs::create_dir(&root)?;
    save_with_budget(
        budget.as_ref(), &root.join("request.json"),
        &Binding {
            owner: target.owner().clone(),
            operation: operation.clone(),
            epoch,
        },
    )?;
    let result = (|| match operation {
        Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. } =>
            target.execute_system(id, &operation.system_operation().unwrap(), deadline, cancelled)
                .map(|completion| Completion::SystemUi { completion: Box::new(completion), directory:target.evidence_directory().join("system-ui/client") })
                .map_err(|e| bad(e.cause)),
        Operation::SetAppState { owner, state } => target.set_visibility(id, owner, *state, deadline, cancelled)
            .map(|transition| Completion::StateChanged { transition: Box::new(transition) }),
        Operation::DispatchUrl { owner, url } => target.dispatch_url(id, owner, url, deadline, cancelled)
            .map(|acceptance| Completion::UrlAccepted { acceptance: Box::new(acceptance) })
            .map_err(|e| bad(e.cause)),
        Operation::BeginEntry { previous, url } => {
            let epochs=directory.join("host-epochs");std::fs::create_dir_all(&epochs)?;
            target.begin_entry(id,previous,url,&epochs.join(id.as_str()),deadline,cancelled)
                .map(|entry|Completion::EntryBegun {entry:Box::new(entry)})
        }
        Operation::PollEntry { begin } => target.poll_entry(id,begin,deadline,cancelled),
        Operation::Restart { previous } => {
            let epochs = directory.join("host-epochs");
            std::fs::create_dir_all(&epochs)?;
            target.restart(previous, &epochs.join(id.as_str()), deadline, cancelled)
                .map(|ready| Completion::Restarted { previous: Box::new(previous.clone()), ready: Box::new(ready) })
                .map_err(|e| bad(e.cause))
        }
        Operation::DiscoverPages => {
            let inventory = target.discover_pages(deadline, cancelled)?;
            save_with_budget(budget.as_ref(), &root.join("discovery.json"), &inventory)?;
            Ok(Completion::Pages {
                owner: Box::new(target.owner().clone()),
                inventory,
            })
        }
        Operation::AttachPage { discovery, page } => {
            let candidate = selected(directory, target.owner(), discovery, page)?;
            target
                .attach_page(id, &candidate, deadline, cancelled)
                .map(|attachment| Completion::Attached {
                    attachment: Box::new(attachment),
                })
        }
        Operation::ReleasePage { attachment } => target
            .release_page(attachment, &root)
            .map(|attachment| Completion::Released { attachment: Box::new(attachment) }),
        Operation::RetirePage { attachment } => target
            .retire_page(attachment, &root, deadline, cancelled)
            .map(|attachment| Completion::Retired {
                attachment: Box::new(attachment),
            }),
        Operation::Page {
            attachment,
            command,
        } => {
            if let PageOperation::Reconcile { request_id } = command {
                original_clock(directory, target.owner(), attachment, request_id)?;
            }
            target
                .page(attachment, id, command, deadline, cancelled)
                .map(|reply| Completion::Page {
                    attachment: attachment.clone(),
                    reply: Box::new(reply),
                })
        }
        Operation::Observe { input_basis } => {
            let mut capture = target.capture(id.clone(), deadline, cancelled)?;
            if !input_basis {
                capture.input = None;
            }
            if capture.bytes > 16 * 1024 * 1024 {
                return Err(bad("native host capture exceeds limit"));
            }
            Ok(Completion::Observed { capture })
        }
        Operation::Contacts { observation, input } => target
            .input(
                id.clone(),
                observation.clone(),
                input.clone(),
                deadline,
                cancelled,
            )
            .map(|receipt| Completion::Contacts { receipt }),
        Operation::ReconcileContacts { request_id } => target
            .reconcile_input(request_id, deadline, cancelled)
            .map(|receipt| Completion::ContactsReconciled { receipt }),
    })();
    if result.is_ok() { crate::ios_host_fault::point(id, "before_result")?; }
    let result = match result {
        Err(error) if crate::ios_host_fault::is_injected(&error) => return Err(error),
        other => other,
    };
    let saved = match result {
        Ok(completion) => Saved::Completed { completion },
        Err(error) => Saved::Failed {
            error: error.to_string(),
        },
    };
    save_with_budget(budget.as_ref(), &root.join("result.json"), &saved)?;
    saved_completion(saved)
}
fn save_with_budget<T: Serialize>(budget: Option<&WriteBudget>, path: &Path, value: &T) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
        None => frontcap_core::state::save_atomic(path, value),
    }
}
fn saved_completion(saved: Saved) -> io::Result<Completion> {
    match saved {
        Saved::Completed { completion } => Ok(completion),
        Saved::Failed { error } => Err(bad(error)),
    }
}
/// Read original completed evidence only; never reacquire, repair, signal or redispatch.
pub fn read_result(
    directory: &Path,
    config: &Config,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<Reply>> {
    let directory = &directory.canonicalize()?;
    operation.validate().map_err(bad)?;
    let Some(ready): Option<Ready> = load(&directory.join("host-ready.json"))? else {
        return Ok(None);
    };
    config.validate_ready(&ready)?;
    crate::simulator_app::prepared::read_installed(&directory.join("target/app"), &config.app, &ready.owner.simulator)?;
    if let (Some(system), Some(runner)) = (&config.system_ui, &ready.system_ui) {
        crate::ios_host::system::verify_ready(&directory.join("target/host"), system, &config.simulator.source_sha256, &ready.owner, runner)?;
    }
    let launch: crate::webview_session::Launch = required(&directory.join("launch.json"))?;
    let expected = crate::webview_session::Config {
        lifetime_ms: config.lifetime_ms,
        simulator: config.simulator.clone(),
        app: config.app.clone(),
        contacts: config.contacts,
        system_ui: config.system_ui.clone(),
        lifecycle: None,
        service_url_environment: None,
    };
    if !launch.host || serde_json::to_value(launch.config)? != serde_json::to_value(expected)? {
        return Err(bad("native host frozen launch differs"));
    }
    let simulator: String = required(&directory.join("target/simulator/simulator.json"))?;
    let process: frontcap_core::process::ProcessIdentity =
        required(&directory.join("target/app/process.json"))?;
    if ready.owner.simulator != simulator || ready.owner.process != process {
        return Err(bad("native host original process differs"));
    }
    let root = directory.join("host-requests").join(id.as_str());
    let Some(binding): Option<Binding> = load(&root.join("request.json"))? else {
        return Ok(None);
    };
    let Some(epoch) = epochs::resolve(directory, config, &ready, binding.epoch.as_ref())? else { return Ok(None); };
    let ready = epoch.ready;
    let host = epoch.root.join("host");
    if binding.owner != ready.owner || binding.operation != *operation {
        return Err(bad("native host saved request differs"));
    }
    let system_result = || -> io::Result<Option<Completion>> {
        let command = operation.system_operation().ok_or_else(|| bad("not system UI"))?;
        let runner = ready.system_ui.as_ref().ok_or_else(|| bad("missing original system runner"))?;
        let launch = config.system_ui.as_ref().ok_or_else(|| bad("missing system configuration"))?;
        Ok(crate::ios_host::system::read_completion(&host, launch, &config.simulator.source_sha256,
            &ready.owner, runner, id, &command)?.map(|completion| Completion::SystemUi { completion: Box::new(completion), directory:host.join("system-ui/client") }))
    };
    let saved: Option<Saved> = load(&root.join("result.json"))?;
    let saved = match saved {
        Some(saved) => saved,
        None => match operation {
            Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. } => match system_result()? {
                Some(completion) => Saved::Completed { completion }, None => return Ok(None),
            },
            Operation::SetAppState { owner, state } => {
                if owner.as_ref() != &ready.owner { return Err(bad("host visibility owner differs")); }
                match crate::ios_host::visibility::read(&host, config, &ready, id, *state)? {
                    Some(transition) => Saved::Completed {completion:Completion::StateChanged {transition:Box::new(transition)}},
                    None => return Ok(None),
                }
            }
            Operation::DispatchUrl { owner, url } => {
                if owner.as_ref() != &ready.owner { return Err(bad("native host URL owner differs")); }
                match crate::ios_host::entry::read_url(directory, &host, id, owner, url)? {
                    Some(acceptance) => Saved::Completed { completion: Completion::UrlAccepted { acceptance: Box::new(acceptance) } },
                    None => return Ok(None),
                }
            }
            Operation::BeginEntry { .. } => match entry_results::begun(directory,config,id,&binding)? {
                Some(entry)=>Saved::Completed {completion:Completion::EntryBegun {entry:Box::new(entry)}},None=>return Ok(None),
            },
            Operation::PollEntry { begin } => match entry_results::polled(directory,config,id,&binding,begin)? {
                Some(completion)=>Saved::Completed {completion},None=>return Ok(None),
            },
            Operation::Restart { .. } => match epochs::resolve(directory, config, &required(&directory.join("host-ready.json"))?, Some(id))? {
                Some(next) => Saved::Completed { completion: Completion::Restarted { previous: Box::new(ready.owner.clone()), ready: Box::new(next.ready) } },
                None => return Ok(None),
            },
            _ => match recover_page(directory, &host, &ready.owner, id, operation)? {
            Some(completion) => Saved::Completed { completion },
            None => return Ok(None),
            },
        },
    };
    let completion = match saved {
        Saved::Failed { error } => return Ok(Some(Reply::Failed { error })),
        Saved::Completed { completion } => completion,
    };
    match (&completion, operation) {
        (Completion::SystemUi { .. }, Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. }) => {
            if system_result()?.as_ref() != Some(&completion) { return Err(bad("system UI saved result differs")); }
        }
        (Completion::StateChanged { transition }, Operation::SetAppState { owner, state }) => {
            if owner.as_ref() != &ready.owner || crate::ios_host::visibility::read(&host, config, &ready, id, *state)?.as_ref() != Some(transition.as_ref()) {
                return Err(bad("host visibility evidence differs"));
            }
        }
        (Completion::UrlAccepted { acceptance }, Operation::DispatchUrl { owner, url }) => {
            if owner.as_ref() != &ready.owner || crate::ios_host::entry::read_url(directory, &host, id, owner, url)?.as_ref() != Some(acceptance.as_ref()) {
                return Err(bad("native host URL acceptance differs"));
            }
        }
        (Completion::EntryBegun {entry},Operation::BeginEntry {..}) => {
            if entry_results::begun(directory,config,id,&binding)?.as_ref()!=Some(entry.as_ref()) {return Err(bad("cold entry acceptance differs"));}
        }
        (Completion::EntryPolled {..},Operation::PollEntry {begin}) => {
            if entry_results::polled(directory,config,id,&binding,begin)?.as_ref()!=Some(&completion) {return Err(bad("cold entry poll proof differs"));}
        }
        (Completion::Restarted { previous, ready: replacement }, Operation::Restart { previous: requested }) => {
            let Some(next) = epochs::resolve(directory, config, &required(&directory.join("host-ready.json"))?, Some(id))? else { return Ok(None); };
            if **previous != ready.owner || requested != previous.as_ref() || **replacement != next.ready {
                return Err(bad("native host replacement completion differs"));
            }
        }
        (Completion::Observed { capture }, Operation::Observe { input_basis }) => {
            let expected = host
                .join(format!("capture-{id}"))
                .join("screen.png");
            if capture.owner != ready.owner
                || capture.path != expected
                || (!input_basis && capture.input.is_some())
            {
                return Err(bad("native host capture binding differs"));
            }
            let mut current = directory.to_path_buf();
            for part in expected.strip_prefix(directory).map_err(bad)?.components() {
                current.push(part);
                if std::fs::symlink_metadata(&current)?
                    .file_type()
                    .is_symlink()
                {
                    return Err(bad("linked native host capture"));
                }
            }
            let metadata = std::fs::metadata(&expected)?;
            if !metadata.is_file()
                || metadata.len() != capture.bytes
                || capture.bytes > 16 * 1024 * 1024
            {
                return Err(bad("native host capture size differs"));
            }
            let data = std::fs::read(expected)?;
            if !data.starts_with(b"\x89PNG\r\n\x1a\n")
                || canonical::sha256_bytes_hex(&data) != capture.sha256
            {
                return Err(bad("native host capture bytes differ"));
            }
        }
        (Completion::Contacts { receipt }, Operation::Contacts { input, .. }) => {
            if crate::contact_session::read_host_completion(
                &host.join("contacts"),
                &ready.owner,
                id,
                Some(input),
            )?
            .as_ref()
                != Some(receipt)
            {
                return Err(bad("native host input receipt differs"));
            }
        }
        (
            Completion::ContactsReconciled { receipt },
            Operation::ReconcileContacts { request_id },
        ) => {
            if crate::contact_session::read_host_completion(
                &host.join("contacts"),
                &ready.owner,
                request_id,
                None,
            )?
            .as_ref()
                != Some(receipt)
            {
                return Err(bad("native host reconciled receipt differs"));
            }
        }
        _ if operation.requires_pages() && ready.pages => {
            if recover_page(directory, &host, &ready.owner, id, operation)?.as_ref() != Some(&completion) {
                return Err(bad(
                    "native host page completion differs from original evidence",
                ));
            }
        }
        _ => return Err(bad("native host completion family differs")),
    }
    Ok(Some(Reply::Completed {
        completion: Box::new(PageCompletion::Host { completion }),
    }))
}

fn selected(
    directory: &Path,
    owner: &Owner,
    discovery: &RequestId,
    page: &str,
) -> io::Result<frontcap_core::native::ios_host::Page> {
    let root = directory.join("host-requests").join(discovery.as_str());
    let binding: Binding = required(&root.join("request.json"))?;
    if binding.owner != *owner || binding.operation != Operation::DiscoverPages {
        return Err(bad("page selection requires original discovery"));
    }
    let completion = saved_completion(required(&root.join("result.json"))?)?;
    let Completion::Pages {
        owner: original,
        inventory,
    } = completion
    else {
        return Err(bad("page selection is not a discovery result"));
    };
    inventory.validate(owner).map_err(bad)?;
    if *original != *owner
        || inventory != required::<Inventory>(&root.join("discovery.json"))?
        || inventory.capacity_exceeded
    {
        return Err(bad("page discovery identity differs or capacity exhausted"));
    }
    inventory
        .pages
        .into_iter()
        .find(|p| p.id == page && p.alive && !p.control_stopped)
        .ok_or_else(|| bad("selected page absent from original discovery"))
}
fn original_clock(
    directory: &Path,
    owner: &Owner,
    attachment: &RequestId,
    request_id: &RequestId,
) -> io::Result<PageOperation> {
    let original: Binding = required(
        &directory
            .join("host-requests")
            .join(request_id.as_str())
            .join("request.json"),
    )?;
    if original.owner != *owner {
        return Err(bad("original page request owner differs"));
    }
    match original.operation {
        Operation::Page {
            attachment: original,
            command,
        } if original == *attachment && command.clock().is_some() => Ok(command),
        _ => Err(bad(
            "reconciliation requires original clock request in the same attachment",
        )),
    }
}
fn recover_page(
    directory: &Path,
    host: &Path,
    owner: &Owner,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<Completion>> {
    use crate::ios_host_pages as pages;
    let root = directory.join("host-requests").join(id.as_str());
    let page_root = host.join("pages");
    Ok(match operation {
        Operation::DiscoverPages => {
            let Some(inventory): Option<Inventory> = load(&root.join("discovery.json"))? else {
                return Ok(None);
            };
            inventory.validate(owner).map_err(bad)?;
            Some(Completion::Pages {
                owner: Box::new(owner.clone()),
                inventory,
            })
        }
        Operation::AttachPage { discovery, page } => {
            if !page_root.join(id.as_str()).join("attachment.json").exists() {
                return Ok(None);
            }
            let original = pages::attachment(&page_root, id, owner)?;
            if original.page != selected(directory, owner, discovery, page)? {
                return Err(bad("attachment differs from selected discovery"));
            }
            Some(Completion::Attached {
                attachment: Box::new(original),
            })
        }
        Operation::ReleasePage { attachment } => pages::released(&page_root, owner, attachment, &root)?
            .map(|attachment| Completion::Released { attachment: Box::new(attachment) }),
        Operation::RetirePage { attachment } => {
            let Some(inventory): Option<Inventory> = load(&root.join("page-retirement.json"))?
            else {
                return Ok(None);
            };
            inventory.validate(owner).map_err(bad)?;
            let original = pages::attachment(&page_root, attachment, owner)?;
            if !inventory.pages.iter().any(|p| {
                p.id == original.page.id
                    && p.endpoint == original.page.endpoint
                    && !p.alive
                    && p.control_stopped
            }) {
                return Err(bad("page retirement lacks original view/channel absence"));
            }
            Some(Completion::Retired {
                attachment: Box::new(original),
            })
        }
        Operation::Page {
            attachment,
            command,
        } => {
            let (request, clock) = match command {
                PageOperation::Reconcile { request_id } => (
                    request_id,
                    original_clock(directory, owner, attachment, request_id)?,
                ),
                _ => (id, command.clone()),
            };
            pages::completed(&page_root, owner, attachment, request, &clock)?.map(|reply| {
                Completion::Page {
                    attachment: attachment.clone(),
                    reply: Box::new(reply),
                }
            })
        }
        _ => None,
    })
}
