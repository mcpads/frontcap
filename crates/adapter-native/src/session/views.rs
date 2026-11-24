//! View captures and shared clock commands on the existing serial session ownership lease.
use super::*;
use frontcap_core::native::android_capture::{Capture, Collection, Unavailable};
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ViewReply {
    Observed { capture: Capture },
    Unavailable { observation: Unavailable },
    Completed { completion: Completion },
    Failed { error: String },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    id: RequestId,
    started_monotonic_ns: u64,
    deadline_ns: u64,
}

/// Offline lookup only. A missing result does not authorize another collection of this request.
pub fn read_view_observation(directory: &Path, id: &RequestId) -> io::Result<Option<Capture>> {
    match read_view_result(directory, id)? {
        Some(Collection::Observed(capture)) => Ok(Some(capture)),
        Some(Collection::Unavailable(_)) => {
            Err(io::Error::other("observation has no actionable capture"))
        }
        None => Ok(None),
    }
}
pub fn view_observation_directory(root: &Path, id: &RequestId) -> io::Result<Option<PathBuf>> {
    let mut found = None;
    for (_, directory) in crate::android_restarts::epochs(root)? {
        if directory.join("views").join(id.as_str()).exists() {
            if found.is_some() {
                return Err(io::Error::other(
                    "View observation occurs in multiple epochs",
                ));
            }
            found = Some(directory);
        }
    }
    Ok(found)
}
pub fn read_view_result(root: &Path, id: &RequestId) -> io::Result<Option<Collection>> {
    let Some(directory) = view_observation_directory(root, id)? else {
        return Ok(None);
    };
    read_epoch_result(&directory, id)
}
fn read_epoch_result(directory: &Path, id: &RequestId) -> io::Result<Option<Collection>> {
    let root = directory.join("views").join(id.as_str());
    let Some(binding): Option<Binding> = load(&root.join("request.json"))? else {
        if root.exists() {
            return Err(io::Error::other("incomplete View request binding"));
        }
        return Ok(None);
    };
    if binding.id != *id {
        return Err(io::Error::other("View request identity changed"));
    }
    let captured: Option<Capture> = load(&root.join("result.json"))?;
    let unavailable: Option<Unavailable> = load(&root.join("unavailable.json"))?;
    let (result, started, finished) = match (captured, unavailable) {
        (Some(c), None) => {
            let times = (c.started_monotonic_ns, c.finished_monotonic_ns);
            (Collection::Observed(c), times.0, times.1)
        }
        (None, Some(u)) => {
            let times = (u.started_monotonic_ns, u.finished_monotonic_ns);
            (Collection::Unavailable(u), times.0, times.1)
        }
        (None, None) => return Ok(None),
        _ => return Err(io::Error::other("contradictory retained View result")),
    };
    let ready: crate::android_app::Ready = load(&directory.join("view-ready.json"))?
        .ok_or_else(|| io::Error::other("missing View session owner"))?;
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("missing View session launch"))?;
    let app = launch
        .config
        .view_app
        .ok_or_else(|| io::Error::other("missing View profile"))?;
    if ready.package != app.package
        || ready.activity != app.activity
        || ready.serial != format!("emulator-{}", launch.config.port)
        || ready.source_sha256 != launch.config.source_sha256
        || ready.apk_sha256 != launch.config.apk_sha256
    {
        return Err(io::Error::other("View owner differs from session launch"));
    }
    let verified = crate::android_collection::verify_retained_observation(
        &root.join("collection"),
        &crate::android_app::capture_owner(&ready),
    )?;
    if verified != result
        || started < binding.started_monotonic_ns
        || finished > binding.deadline_ns
    {
        return Err(io::Error::other(
            "View result differs from its bound collection",
        ));
    }
    Ok(Some(result))
}
fn observe(
    directory: &Path,
    device: &mut Device,
    id: &RequestId,
    deadline_ns: u64,
    cancelled: &AtomicBool,
) -> io::Result<Collection> {
    if device.has_view_entry(id) {
        return Err(io::Error::other(
            "request ID already names an external entry",
        ));
    }
    if let Some(result) = read_epoch_result(directory, id)? {
        return Ok(result);
    }
    let root = directory.join("views").join(id.as_str());
    // create_dir fails for any earlier partial attempt: a new read would destroy retry identity.
    std::fs::create_dir(&root)?;
    let binding = Binding {
        id: id.clone(),
        started_monotonic_ns: monotonic_ns()?,
        deadline_ns,
    };
    save_atomic(&root.join("request.json"), &binding)?;
    let result = device
        .collect_view_observation(&root.join("collection"), instant(deadline_ns)?, cancelled)
        .map_err(io::Error::other)?;
    let (started, finished) = match &result {
        Collection::Observed(c) => (c.started_monotonic_ns, c.finished_monotonic_ns),
        Collection::Unavailable(u) => (u.started_monotonic_ns, u.finished_monotonic_ns),
    };
    if started < binding.started_monotonic_ns || finished > deadline_ns {
        return Err(io::Error::other(
            "View collection exceeded its request interval",
        ));
    }
    match &result {
        Collection::Observed(capture) => {
            save_atomic(&root.join("result.json"), capture)?;
            save_atomic(&directory.join("latest-view.json"), &Some(id.clone()))?;
        }
        Collection::Unavailable(observation) => {
            save_atomic(&root.join("unavailable.json"), observation)?;
            save_atomic(
                &directory.join("latest-view.json"),
                &Option::<RequestId>::None,
            )?;
        }
    }
    Ok(result)
}

pub(super) fn serve(
    directory: &Path,
    device: &mut Device,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> io::Result<()> {
    let ready = device
        .view_ready()
        .ok_or_else(|| io::Error::other("missing View owner"))?
        .clone();
    std::fs::create_dir(directory.join("commands"))?;
    let mut history = crate::android_restarts::History::create(directory, ready.clone())?;
    let mut requests = create_epoch(directory, &ready, None)?;
    let mut epoch = directory.to_path_buf();
    let mut retired_requests = Vec::new();
    let mut expected = 1;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return if stopping.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(io::Error::other(
                    "View controller disconnected or sent invalid commands",
                ))
            };
        }
        let message = match commands.recv_timeout(Duration::from_millis(10)) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let number = match &message {
            Message::ObserveViews { number, .. }
            | Message::ExecuteViewOperation { number, .. }
            | Message::DispatchViewUrl { number, .. }
            | Message::SetViewState { number, .. }
            | Message::RestartViews { number, .. } => *number,
            _ => return Err(io::Error::other("unsupported View session command")),
        };
        if number != expected {
            return Err(io::Error::other("out-of-order View session command"));
        }
        expected += 1;
        let attempt = directory.join("commands").join(number.to_string());
        std::fs::create_dir(&attempt)?;
        save_atomic(&attempt.join("command.json"), &message)?;
        if let Message::SetViewState { id, generation, state, deadline_ns, .. } = &message {
            let result = (|| {
                if let Some(result) = crate::android_app_state::read_completion(directory, id, generation, *state)? {
                    return Ok(result);
                }
                let owner = crate::android_app_state::dispatch(directory, id, generation, *state)?;
                let transition = device.set_view_state(id, &owner, *state, instant(*deadline_ns)?, cancelled)?;
                crate::android_app_state::complete(directory, id, &transition)?;
                Ok::<_, io::Error>(transition)
            })();
            let failure = result.as_ref().err().map(ToString::to_string);
            let reply = match result {
                Ok(transition) => crate::android_app_state::Reply::Completed { transition: Box::new(transition) },
                Err(error) => crate::android_app_state::Reply::Failed { error: error.to_string() },
            };
            save_atomic(&attempt.join("result.json"), &reply)?;
            if let Some(error) = failure { return Err(io::Error::other(error)); }
            continue;
        }
        if let Message::RestartViews {
            id,
            generation,
            url,
            deadline_ns,
            ..
        } = &message
        {
            let result = (|| {
                if let Some(result) =
                    crate::android_restarts::read_completion(directory, id, generation, url.as_deref())?
                {
                    return Ok(result);
                }
                crate::android_restarts::validate_restart(directory, id, generation, url.as_deref())?;
                if device.has_view_entry(id) {
                    return Err(io::Error::other("restart ID already names a URL"));
                }
                history.dispatch(id, generation, url.as_deref())?;
                let replacement =
                    device.restart_view(id, generation, url.as_deref(), instant(*deadline_ns)?, cancelled)?;
                let next = crate::android_restarts::epoch_directory(
                    directory,
                    &replacement.owner.generation,
                )?;
                std::fs::create_dir_all(next.parent().unwrap())?;
                std::fs::create_dir(&next)?;
                let launch: Launch = load(&directory.join("launch.json"))?
                    .ok_or_else(|| io::Error::other("missing View launch"))?;
                let next_requests = create_epoch(&next, &replacement.owner, Some(&launch))?;
                history.complete(id, replacement.clone())?;
                retired_requests.push(std::mem::replace(&mut requests, next_requests));
                epoch = next;
                Ok(replacement)
            })();
            let failure = result.as_ref().err().map(ToString::to_string);
            let reply = match result {
                Ok(replacement) => crate::android_restarts::Reply::Completed { replacement: Box::new(replacement) },
                Err(error) => crate::android_restarts::Reply::Failed {
                    error: error.to_string(),
                },
            };
            save_atomic(&attempt.join("result.json"), &reply)?;
            if let Some(error) = failure {
                return Err(io::Error::other(error));
            }
            continue;
        }
        if let Message::DispatchViewUrl {
            id,
            request,
            deadline_ns,
            ..
        } = &message
        {
            let result = (|| {
                if history.contains(id)
                    || crate::android_app_state::has_request(directory, id)?
                    || crate::android_restarts::request_epoch(directory, id)?.is_some()
                    || view_observation_directory(directory, id)?.is_some()
                {
                    return Err(io::Error::other("URL ID already names a View request"));
                }
                let (owner, _) = crate::android_restarts::epochs(directory)?
                    .into_iter()
                    .find(|(o, _)| o.generation == request.generation)
                    .ok_or_else(|| {
                        io::Error::other("URL generation absent from Android lineage")
                    })?;
                let entries = crate::android_restarts::device_epoch(directory, &owner)?
                    .join("external-entry");
                if entries.join("owner.json").exists()
                    && let Some(accepted) =
                        crate::external_entry::read_accepted(&entries, id, request)?
                {
                    return Ok(accepted);
                }
                crate::android_app_state::require_foreground(directory)?;
                dispatch_url(
                    &epoch,
                    device,
                    &requests,
                    id,
                    request.clone(),
                    instant(*deadline_ns)?,
                    cancelled,
                )
            })();
            let failure = result.as_ref().err().map(ToString::to_string);
            let reply = match result {
                Ok(acceptance) => crate::external_entry::Reply::Accepted { acceptance },
                Err(error) => crate::external_entry::Reply::Failed {
                    error: error.to_string(),
                },
            };
            save_atomic(&attempt.join("result.json"), &reply)?;
            if let Some(error) = failure {
                return Err(io::Error::other(error));
            }
            continue;
        }
        let work = (|| match &message {
            Message::ObserveViews {
                id, deadline_ns, ..
            } => {
                if history.contains(id)
                    || crate::android_app_state::has_request(directory, id)?
                    || device.has_view_entry(id)
                    || crate::android_restarts::request_epoch(directory, id)?.is_some()
                {
                    return Err(io::Error::other(
                        "observation ID already names an operation",
                    ));
                }
                let result = match read_view_result(directory, id)? {
                    Some(result) => result,
                    None if view_observation_directory(directory, id)?.is_some() => {
                        return Err(io::Error::other("original View observation unresolved"));
                    }
                    None => {
                        crate::android_app_state::require_foreground(directory)?;
                        observe(&epoch, device, id, *deadline_ns, cancelled)?
                    },
                };
                Ok(match result {
                    Collection::Observed(capture) => ViewReply::Observed { capture },
                    Collection::Unavailable(observation) => ViewReply::Unavailable { observation },
                })
            }
            Message::ExecuteViewOperation {
                id,
                operation,
                deadline_ns,
                ..
            } => {
                if history.contains(id)
                    || crate::android_app_state::has_request(directory, id)?
                    || device.has_view_entry(id)
                    || view_observation_directory(directory, id)?.is_some()
                {
                    return Err(io::Error::other(
                        "operation ID already names another View request",
                    ));
                }
                if let Some((owner, original)) =
                    crate::android_restarts::request_epoch(directory, id)?
                {
                    let completion = RequestStore::read_completion(
                        &original.join("requests"),
                        &owner.identity(),
                        &owner.generation,
                        &owner.source_sha256,
                        id,
                        operation,
                    )
                    .map_err(io::Error::other)?
                    .ok_or_else(|| {
                        io::Error::other("original View operation unresolved; no replay")
                    })?;
                    return Ok(ViewReply::Completed { completion });
                }
                crate::android_app_state::require_foreground(directory)?;
                ViewExecutor { directory: &epoch, device, requests: &mut requests }.execute(
                    id,
                    operation.clone(),
                    &attempt,
                    instant(*deadline_ns)?,
                    cancelled,
                )
                .map(|completion| ViewReply::Completed { completion })
            }
            _ => unreachable!(),
        })();
        let failure = work.as_ref().err().map(ToString::to_string);
        let reply = match work {
            Ok(reply) => reply,
            Err(error) => ViewReply::Failed {
                error: error.to_string(),
            },
        };
        save_atomic(&attempt.join("result.json"), &reply)?;
        if let Some(error) = failure {
            return Err(io::Error::other(error));
        }
    }
}

struct ViewExecutor<'a> {
    directory: &'a Path,
    device: &'a mut Device,
    requests: &'a mut RequestStore,
}
impl ViewExecutor<'_> {
    fn execute(
        &mut self,
        id: &RequestId,
        operation: Operation,
        attempt: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Completion> {
        let Self { directory, device, requests } = self;
        check(deadline, cancelled)?;
        if device.has_view_entry(id) {
            return Err(io::Error::other(
                "request ID already names an external entry",
            ));
        }
        if directory.join("views").join(id.as_str()).exists() {
            return Err(io::Error::other("request ID already names an observation"));
        }
        validate_operation(directory, id, &operation)?;
        let invalidates_basis = matches!(
            operation,
            Operation::Advance { .. }
                | Operation::ReplaceViewText { .. }
                | Operation::TapView { .. }
                | Operation::ScrollView { .. }
        );
        match requests
            .prepare(id.clone(), operation)
            .map_err(io::Error::other)?
        {
            RequestRecovery::Completed(completion) => return Ok(completion),
            RequestRecovery::Reconcile(_) => {
                return Err(io::Error::other(
                    "View request remains unresolved; no redispatch authorized",
                ));
            }
            RequestRecovery::Prepared(_) => (),
        }
        let request = requests.dispatch(id).map_err(io::Error::other)?;
        let completion =
            device.execute_view_operation(&request, &attempt.join("exchange"), deadline, cancelled)?;
        requests
            .complete(id, completion.clone())
            .map_err(io::Error::other)?;
        if invalidates_basis {
            save_atomic(
                &directory.join("latest-view.json"),
                &Option::<RequestId>::None,
            )?;
        }
        Ok(completion)
    }
}

pub(super) fn validate_entry(
    directory: &Path,
    request: &frontcap_core::native::external_entry::Request,
) -> io::Result<()> {
    use frontcap_core::native::external_entry::Target;
    request.validate().map_err(io::Error::other)?;
    let ready: crate::android_app::Ready = load(&directory.join("view-ready.json"))?
        .ok_or_else(|| io::Error::other("missing View session owner"))?;
    let target = Target::Android {
        serial: ready.serial,
        package: ready.package,
    };
    if request.target != target || request.generation != ready.generation {
        return Err(io::Error::other(
            "external entry differs from View session owner",
        ));
    }
    Ok(())
}

fn dispatch_url(
    directory: &Path,
    device: &mut Device,
    requests: &RequestStore,
    id: &RequestId,
    request: frontcap_core::native::external_entry::Request,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<frontcap_core::native::external_entry::Accepted> {
    validate_entry(directory, &request)?;
    if directory.join("views").join(id.as_str()).exists() {
        return Err(io::Error::other(
            "request ID already names a View observation",
        ));
    }
    match requests.recovery(id) {
        Err(crate::store::StoreError::Unknown) => (),
        Ok(_) => {
            return Err(io::Error::other(
                "request ID already names a View operation",
            ));
        }
        Err(error) => return Err(io::Error::other(error)),
    }
    check(deadline, cancelled)?;
    // Invalidate before possible delivery, including a lost ADB reply. A cached receipt above
    // does not invalidate a later observation: querying a result performs no new app work.
    save_atomic(
        &directory.join("latest-view.json"),
        &Option::<RequestId>::None,
    )?;
    device.dispatch_view_url(id, request, deadline, cancelled)
}

pub(super) fn validate_operation(
    directory: &Path,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<()> {
    let ready: crate::android_app::Ready = load(&directory.join("view-ready.json"))?
        .ok_or_else(|| io::Error::other("missing View owner"))?;
    operation
        .validate(&ready.identity())
        .map_err(io::Error::other)?;
    let observation = match operation {
        Operation::ReplaceViewText { edit } => &edit.observation,
        Operation::TapView { tap } => &tap.observation,
        Operation::ScrollView { scroll } => &scroll.observation,
        _ => return Ok(()),
    };
    if RequestStore::read_completion(
        &directory.join("requests"),
        &ready.identity(),
        &ready.generation,
        &ready.source_sha256,
        id,
        operation,
    )
    .map_err(io::Error::other)?
    .is_some()
    {
        return Ok(());
    }
    let latest: Option<Option<RequestId>> = load(&directory.join("latest-view.json"))?;
    if latest.flatten().as_ref() != Some(observation) {
        return Err(io::Error::other("input requires a fresh View observation"));
    }
    let capture = match read_epoch_result(directory, observation)? {
        Some(Collection::Observed(capture)) => capture,
        _ => {
            return Err(io::Error::other(
                "input observation has no retained capture",
            ));
        }
    };
    match operation {
        Operation::ReplaceViewText { edit } => edit.validate_target(&capture),
        Operation::TapView { tap } => tap.validate_target(&capture),
        Operation::ScrollView { scroll } => scroll.validate_target(&capture),
        _ => unreachable!(),
    }
    .map_err(io::Error::other)
}

fn create_epoch(
    directory: &Path,
    owner: &crate::android_app::Ready,
    launch: Option<&Launch>,
) -> io::Result<RequestStore> {
    std::fs::create_dir(directory.join("views"))?;
    if let Some(launch) = launch {
        save_atomic(&directory.join("launch.json"), launch)?;
    }
    let requests = RequestStore::create(
        &directory.join("requests"),
        owner.identity(),
        owner.generation.clone(),
        owner.source_sha256.clone(),
    )
    .map_err(io::Error::other)?;
    crate::android_app_state::create(directory, owner)?;
    save_atomic(&directory.join("view-ready.json"), owner)?;
    Ok(requests)
}
