//! Explicit page attachment within an independently owned native host.
use super::*;
use crate::android_webview_page::Connection;

fn descriptor(page: &crate::android_webview::Page) -> host::Page {
    host::Page { id:page.id.clone(), url:page.url.clone(), title:page.title.clone() }
}
pub(super) fn discover(device: &mut Device, evidence: &Path, deadline: Instant, cancelled: &AtomicBool) -> io::Result<host::Completion> {
    let pages = device.enumerate_host_pages(evidence,deadline,cancelled)?;
    Ok(host::Completion::Pages { owner:Box::new(device.app_process().unwrap().clone()), pages:pages.iter().map(descriptor).collect() })
}
fn selected(directory: &Path, discovery: &RequestId, page: &str, url: &str, expected: &frontcap_core::native::android_webview::AppProcess) -> io::Result<host::Page> {
    let operation = Operation::Host { request:host::Operation::DiscoverPages };
    let Some(Reply::Completed { completion }) = super::super::read_result(directory,discovery,&operation)? else {
        return Err(io::Error::other("page selection requires an original successful discovery"));
    };
    let Completion::Host { completion:host::Completion::Pages { owner, pages } } = *completion else {
        return Err(io::Error::other("request is not page discovery"));
    };
    if owner.as_ref() != expected { return Err(io::Error::other("page discovery belongs to another host process")); }
    pages.into_iter().find(|p| p.id==page && p.url==url)
        .ok_or_else(||io::Error::other("selected page or URL differs from discovery"))
}
pub(super) async fn attach_and_serve(driver: &mut Driver<'_>, device: &mut Device, dispatch: &Dispatch,
    operation: &host::Operation) -> io::Result<()> {
    let (discovery, page_id, url, random_seed, replacement) = match operation {
        host::Operation::AttachPage { discovery, page, url, random_seed } => (discovery, page.as_str(), url.as_str(), *random_seed, None),
        host::Operation::AttachDocument { release, discovery, page, url, document } =>
            (discovery, page.as_str(), url.as_str(), None, Some((release, document.as_str()))),
        _ => return Err(io::Error::other("operation is not page attachment")),
    };
    let candidate = (|| {
        if replacement.is_none() { device.require_unclaimed_webview(page_id)?; }
        let candidate = selected(driver.directory,discovery,page_id,url,&driver.ready.owner)?;
        let live = device.enumerate_host_pages(&dispatch.request.join("candidates"),dispatch.deadline,driver.cancelled)?;
        if !live.iter().any(|p| p.id==candidate.id && p.url==candidate.url) {
            return Err(io::Error::other("selected page disappeared or navigated before attachment"));
        }
        let previous = replacement.map(|(release, document)| {
            let original = released_attachment(driver.directory, release, &driver.ready.owner)?;
            if !driver.released_connections.iter().any(|(id,_)| id==&original.id) {
                return Err(io::Error::other("original released transport unavailable"));
            }
            if original.page.id != candidate.id || original.document == document {
                return Err(io::Error::other("replacement must be a new document in the released target"));
            }
            Ok(original)
        }).transpose()?;
        Ok((candidate, previous))
    })();
    let (candidate, previous) = match candidate { Ok(candidate) => candidate, Err(error) => return driver.finish(dispatch,Err(error)) };
    let directory = driver.directory.join("pages").join(dispatch.id.as_str());
    std::fs::create_dir(&directory)?;
    device.invalidate_contact_capture();
    let owner = device.app_process().unwrap().clone();
    let connection = if let Some(previous) = &previous {
        let transport = driver.released_connections.iter().find(|(id,_)| id==&previous.id)
            .ok_or_else(||io::Error::other("original released transport unavailable"))?.1.clone();
        let old_directory = driver.directory.join("pages").join(previous.id.as_str()).join("page");
        device.reuse_webview_transport(page_id, &old_directory, &directory.join("page"), transport,
            dispatch.deadline, driver.cancelled)
    } else {
        device.connect_webview_page(page_id,&directory.join("page"),dispatch.deadline,driver.cancelled).await
    };
    let mut page = match connection {
        Ok(page) => page,
        Err(error) => return driver.finish(dispatch,Err(error)),
    };
    let prepared = if let Some((_,document)) = replacement {
        let previous = previous.as_ref().unwrap();
        let old_directory = driver.directory.join("pages").join(previous.id.as_str()).join("page");
        page.select_document(&old_directory, document, url, &previous.prepared, dispatch.deadline, driver.cancelled).await
            .map(|prepared| (prepared, document.to_owned()))
    } else {
        super::super::prepare_page(&mut page,&directory,url,random_seed,dispatch.deadline,driver.cancelled).await
    };
    let (prepared,document) = match prepared {
        Ok(value) => value,
        Err(error) => {
            // Preparation may have installed scripts or navigated. Retire the owner, never leave
            // a partially controlled producer behind an apparently usable host session.
            driver.finish(dispatch,Err(io::Error::other(error.to_string())))?;
            return Err(error);
        }
    };
    if let Some(previous) = &previous {
        // Keep the original agent: paused navigation loaders belong to it. Removing the retained
        // clone transfers transport ownership; closing it would also close the selected connection.
        let index = driver.released_connections.iter().position(|(id,_)| id==&previous.id)
            .ok_or_else(||io::Error::other("released transport disappeared during document selection"))?;
        driver.released_connections.remove(index);
        driver.save(&directory.join("previous-transport-reused.json"), &previous.id)?;
    }
    let attachment = host::Attachment { id:dispatch.id.clone(), owner, page:candidate, prepared, document };
    driver.save(&directory.join("attachment.json"),&attachment)?;
    driver.finish(dispatch,Ok(host::Completion::Attached { attachment:Box::new(attachment.clone()) }))?;
    while let Some(request) = driver.next()? {
        let operation = match request.operation(driver.cancelled) {
            Ok(operation) => operation,
            Err(error) => { driver.finish(&request,Err(error))?; continue; },
        };
        if let host::Operation::SetAppState {owner,state}=operation {
            driver.visibility(page.host_device(),&request,owner,*state)?; continue;
        }
        if let Err(error)=page.host_device().require_host_foreground() {driver.finish(&request,Err(error))?;continue;}
        if matches!(operation, host::Operation::AttachSpeech { .. }) {
            driver.attach_speech(page.host_device(),&request,operation)?; continue;
        }
        if let host::Operation::Restart { previous, url } = operation {
            if let Err(error) = driver.admit_restart(previous) { driver.finish(&request,Err(error))?; continue; }
            page.close();
            driver.restart(page.host_device(),&request,previous,url.as_deref())?;
            return Ok(());
        }
        if let host::Operation::DispatchUrl { owner, url } = operation {
            // URL routing may leave the original page intact (for example, a busy-app guard).
            // Keep its identity; subsequent page work still verifies the original document.
            driver.dispatch_url(page.host_device(), &request, owner, url)?;
            continue;
        }
        let work = match operation {
            host::Operation::ReleasePage { attachment:id } if id==&attachment.id => {
                match release(&mut page,&attachment,&request,driver) {
                    Ok(completion) => {
                        driver.released_connections.push((attachment.id.clone(), page.retain_transport()));
                        driver.finish(&request,Ok(completion))?;
                        return Ok(());
                    }
                    Err(error) => Err(error),
                }
            }
            host::Operation::RetirePage { attachment:id } if id==&attachment.id => {
                match retire(&mut page,&attachment,&request,driver) {
                    Ok(completion) => {
                        driver.finish(&request,Ok(completion))?;
                        return Ok(());
                    }
                    Err(error) => Err(error),
                }
            }
            host::Operation::Page { attachment:id, command } if id==&attachment.id => {
                execute_page(&mut page,&attachment,&request.id,command,request.deadline,driver.cancelled).await
            }
            _ => driver.execute(page.host_device(),&request,operation),
        };
        driver.finish(&request,work)?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    attachment: host::Attachment,
    frames_sha256: String,
    timers_sha256: String,
}
fn producer_digest(directory: &Path, name: &str) -> io::Result<String> {
    Ok(frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(directory.join(name))?))
}
fn release(page: &mut Connection<'_>, attachment: &host::Attachment, request: &Dispatch,
    driver: &Driver<'_>) -> io::Result<host::Completion> {
    if driver.released_connections.len() >= 64 { return Err(io::Error::other("released page transport limit reached")); }
    let progress = page.inspect_progress()?;
    validate_release_progress(attachment,&progress)?;
    let directory = request.request.parent().unwrap().parent().unwrap()
        .join("pages").join(attachment.id.as_str()).join("page");
    let ledger: crate::android_frames::Ledger = load(&directory.join("frame-producer.json"))?
        .ok_or_else(||io::Error::other("release requires retained frame producer"))?;
    ledger.validate()?;
    if progress.frames.as_ref()!=Some(&ledger.inspect()) {
        return Err(io::Error::other("release frame memory differs from durable producer"));
    }
    let proof = Release { attachment:attachment.clone(),
        frames_sha256:producer_digest(&directory,"frame-producer.json")?,
        timers_sha256:producer_digest(&directory,"timer-state.json")? };
    driver.save(&request.request.join("page-release.json"),&proof)?;
    page.host_device().invalidate_contact_capture();
    Ok(host::Completion::Released { attachment:Box::new(attachment.clone()) })
}
fn validate_release_progress(attachment: &host::Attachment,
    progress: &frontcap_core::native::android_progress::Progress) -> io::Result<()> {
    use frontcap_core::native::android_timers::{State, Action};
    let frames = progress.frames.as_ref().is_some_and(|frames|
        frames.document==attachment.document && frames.pending.is_none());
    let timers = matches!(&progress.timers, Some(State::Paused { completion })
        if completion.document==attachment.document && completion.ticks_base.is_finite()
            && (completion.action==Action::Pause || completion.budget_expired));
    if progress.owner.process()!=attachment.owner || progress.page_id!=attachment.page.id
        || progress.connection_closed || progress.frames_need_reconciliation || progress.timers_need_pause
        || !frames || !timers
        || progress.contacts.as_ref().is_some_and(|contacts|contacts.storage_uncertain || contacts.pending.is_some())
        || progress.lifecycle.as_ref().is_some_and(|lifecycle|lifecycle.uncertain || lifecycle.pending.is_some()) {
        return Err(io::Error::other("page release requires original settled producer evidence; reconcile unresolved work first"));
    }
    Ok(())
}
fn released(directory: &Path, id: &RequestId, expected: &RequestId) -> io::Result<Option<host::Completion>> {
    let Some(proof) = load::<Release>(&directory.join("requests").join(id.as_str()).join("page-release.json"))? else { return Ok(None); };
    let original = attachment(directory,expected,&proof.attachment.owner)?;
    let page = directory.join("pages").join(expected.as_str()).join("page");
    if proof.attachment!=original || proof.frames_sha256!=producer_digest(&page,"frame-producer.json")?
        || proof.timers_sha256!=producer_digest(&page,"timer-state.json")? {
        return Err(io::Error::other("page release differs from original attachment or producer evidence"));
    }
    Ok(Some(host::Completion::Released { attachment:Box::new(original) }))
}

fn released_attachment(directory: &Path, release: &RequestId,
    owner: &frontcap_core::native::android_webview::AppProcess) -> io::Result<host::Attachment> {
    let operation: Operation = load(&directory.join("requests").join(release.as_str()).join("operation.json"))?
        .ok_or_else(||io::Error::other("original release operation missing"))?;
    let Operation::Host { request: host::Operation::ReleasePage { attachment: reference } } = operation else {
        return Err(io::Error::other("replacement requires an original release request"));
    };
    let Some(host::Completion::Released { attachment: original }) = released(directory,release,&reference)? else {
        return Err(io::Error::other("original release proof missing"));
    };
    if original.owner!=*owner {return Err(io::Error::other("released document belongs to another app process"));}
    Ok(*original)
}

fn retire(page: &mut Connection<'_>, attachment: &host::Attachment, request: &Dispatch, driver: &Driver<'_>) -> io::Result<host::Completion> {
    let pages = page.host_device().enumerate_host_pages(&request.request.join("candidates"),request.deadline,driver.cancelled)?;
    if pages.iter().any(|candidate| candidate.id==attachment.page.id) {
        return Err(io::Error::other("attached page still exists; removal is not established"));
    }
    page.close();
    page.host_device().invalidate_contact_capture();
    let completion = host::Completion::Retired { attachment:Box::new(attachment.clone()) };
    driver.save(&request.request.join("page-retirement.json"),&completion)?;
    Ok(completion)
}
async fn execute_page(page: &mut Connection<'_>, attachment: &host::Attachment, id: &RequestId,
    command: &host::PageOperation, deadline: Instant, cancelled: &AtomicBool) -> io::Result<host::Completion> {
    use host::PageOperation as P;
    if let P::Frame { request } = command && request.document!=attachment.document {
        return Err(io::Error::other("frame request differs from selected attachment document"));
    }
    // Native host ownership alone cannot authorize work in a replaced document. A retained
    // inspection stays passive; live work observes document identity without rebinding the old ledger.
    if matches!(command, P::InspectDocument) {
        let receipt = page.observe_document(&child_id(id,"document"),deadline,cancelled).await?;
        return Ok(host::Completion::Page { attachment:attachment.id.clone(), completion:Box::new(Completion::Frames { receipt }) });
    }
    if !matches!(command,P::InspectProgress) {
        let receipt = page.observe_document(&child_id(id,"document"),deadline,cancelled).await?
            .ok_or_else(||io::Error::other("attached producer disappeared"))?;
        if receipt.document!=attachment.document { return Err(io::Error::other("attached document replaced")); }
    }
    let operation = match command {
        P::Read => Operation::Read,
        P::InspectProgress => Operation::InspectProgress,
        P::InspectFrames => Operation::InspectFrames,
        P::InspectDocument => unreachable!("document observation returned above"),
        P::InspectPointers => Operation::InspectPointers,
        P::Frame { request } => Operation::Frame { request:request.clone() },
        P::Timers { action } => Operation::Timers { action:action.clone() },
    };
    let completion = super::super::execute(page,id,&operation,deadline,cancelled).await?;
    if !matches_page_result(attachment,command,&completion)? {
        return Err(io::Error::other("page effect did not retain the original attachment identity; inspect original evidence, never replay"));
    }
    Ok(host::Completion::Page { attachment:attachment.id.clone(), completion:Box::new(completion) })
}
fn attachment(directory: &Path, id: &RequestId, owner: &frontcap_core::native::android_webview::AppProcess) -> io::Result<host::Attachment> {
    let attachment:host::Attachment = load(&directory.join("pages").join(id.as_str()).join("attachment.json"))?
        .ok_or_else(||io::Error::other("original page attachment unavailable"))?;
    if attachment.id!=*id || attachment.owner!=*owner
        || !frontcap_core::native::request::canonical_uuid(&attachment.document)
        || attachment.prepared.url!=attachment.page.url {
        return Err(io::Error::other("page attachment differs from original host"));
    }
    Ok(attachment)
}
pub(super) fn recover(directory: &Path, id: &RequestId, operation: &host::Operation) -> io::Result<Option<host::Completion>> {
    match operation {
        host::Operation::AttachPage { .. } | host::Operation::AttachDocument { .. } => Ok(load::<host::Attachment>(&directory.join("pages").join(id.as_str()).join("attachment.json"))?
            .map(|attachment|host::Completion::Attached { attachment:Box::new(attachment) })),
        host::Operation::ReleasePage { attachment } => released(directory,id,attachment),
        host::Operation::RetirePage { .. } => load(&directory.join("requests").join(id.as_str()).join("page-retirement.json")),
        _ => Ok(None),
    }
}
fn valid_transport_handoff(directory: &Path, previous: &RequestId) -> io::Result<bool> {
    let reused: Option<RequestId> = load(&directory.join("previous-transport-reused.json"))?;
    let retired: Option<RequestId> = load(&directory.join("previous-transport-retired.json"))?;
    // Earlier attempts closed the old transport. Their immutable results remain readable;
    // conflicting handoff records cannot establish which operation occurred.
    Ok(matches!((&reused, &retired), (Some(id), None) | (None, Some(id)) if id == previous))
}
pub(super) fn valid_result(directory: &Path, id: &RequestId, operation: &host::Operation,
    completion: &host::Completion, owner: &frontcap_core::native::android_webview::AppProcess) -> io::Result<bool> {
    use host::{Operation as O, Completion as C};
    Ok(match (operation,completion) {
        (O::DiscoverPages,C::Pages { owner:recorded, pages }) => recorded.as_ref()==owner && pages.len()<=64
            && pages.iter().enumerate().all(|(index,p)| !p.id.is_empty() && p.id.len()<=256 && !pages[..index].iter().any(|other|other.id==p.id)),
        (O::AttachPage { discovery, page, url, random_seed },C::Attached { attachment:recorded }) => {
            let original=attachment(directory,id,owner)?;
            recorded.as_ref()==&original && original.page==selected(directory,discovery,page,url,owner)?
                && original.prepared.random_seed==*random_seed
        }
        (O::AttachDocument { release, discovery, page, url, document }, C::Attached { attachment:recorded }) => {
            let original = attachment(directory,id,owner)?;
            let previous = released_attachment(directory,release,owner)?;
            recorded.as_ref()==&original && original.page==selected(directory,discovery,page,url,owner)?
                && original.page.id==previous.page.id && original.document==*document
                && original.document!=previous.document
                && original.prepared.random_seed==previous.prepared.random_seed
                && original.prepared.script_identifier==previous.prepared.script_identifier
                && original.prepared.attachment_sha256==previous.prepared.attachment_sha256
                && valid_transport_handoff(&directory.join("pages").join(id.as_str()), &previous.id)?
        }
        (O::ReleasePage { attachment:reference },C::Released { attachment:recorded }) => {
            recorded.as_ref()==&attachment(directory,reference,owner)?
                && released(directory,id,reference)?.as_ref()==Some(completion)
        }
        (O::RetirePage { attachment:id },C::Retired { attachment:recorded }) => recorded.as_ref()==&attachment(directory,id,owner)?,
        (O::Page { attachment:reference, command },C::Page { attachment:recorded, completion }) if reference==recorded => {
            let original=attachment(directory,reference,owner)?;
            matches_page_result(&original,command,completion)?
        }
        _ => false,
    })
}

fn matches_page_result(original: &host::Attachment, command: &host::PageOperation, completion: &Completion) -> io::Result<bool> {
    use host::PageOperation as P;
    Ok(match (command,completion) {
                (P::Read,Completion::Read { snapshot }) => { snapshot.validate(Some(&original.document))?; true },
                (P::InspectProgress,Completion::Progress { observation }) => observation.owner.process()==original.owner && observation.page_id==original.page.id
                    && observation.frames.as_ref().is_none_or(|frames|frames.document==original.document)
                    && observation.connection_navigation.as_ref().is_none_or(|history|history.valid()),
                (P::InspectDocument,Completion::Frames { receipt }) => { if let Some(receipt)=receipt { receipt.validate()?; } true },
                (P::InspectFrames,Completion::Frames { receipt:Some(receipt) }) => { receipt.validate()?; receipt.document==original.document },
                (P::InspectPointers,Completion::Pointers { snapshot }) => { snapshot.validate()?; snapshot.document==original.document },
                (P::Frame { request },Completion::Frame { receipt }) => { receipt.validate()?; receipt.document==original.document && receipt.last.as_ref().is_some_and(|last|last.request==*request) },
                (P::Timers { action },Completion::Timers { completion }) => completion.document==original.document && completion.action==*action,
                _ => false,
            })
}

#[cfg(test)]
#[path = "android_host_pages_tests.rs"]
mod tests;
