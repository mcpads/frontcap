//! The per-generation recorder: hosts the Chromium generation, the request journal, and control state,
//! and serves control servers over a Unix socket.
pub mod control;
mod blocking;
mod dialogs;
mod dialog_recovery;
pub mod execute;
pub mod image;
pub mod fault;
pub mod launcher;
pub mod basis;
pub mod branch;
mod finalization;
pub mod observe;
mod predicate_wait;
pub mod recover;
pub mod replay;
pub mod runtime;
pub mod native;
pub mod server;

use control::ControlState;
use frontcap_chromium::generation::{Generation, LaunchOptions, ReconnectOptions};
use frontcap_core::journal::Journal;
use frontcap_core::process::ProcessIdentity;
use runtime::{RuntimeRecord, RuntimeState};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Chromium(#[from] frontcap_chromium::ChromiumError),
    #[error(transparent)]
    Journal(#[from] frontcap_core::journal::JournalError),
    #[error("{0}")]
    Missing(String),
}

/// Binds the socket, removing a stale socket file that no recorder answers on.
async fn bind(socket: &Path) -> Result<tokio::net::UnixListener, RecorderError> {
    if socket.exists() {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Err(RecorderError::Missing(format!("another recorder listens on {}", socket.display())));
        }
        std::fs::remove_file(socket)?;
    }
    Ok(tokio::net::UnixListener::bind(socket)?)
}

struct RecorderLock {
    file: std::fs::File,
    owner_pid: u32,
}
impl Drop for RecorderLock {
    fn drop(&mut self) {
        // Closing alone can leave the lock held by an unrelated fork's inherited descriptor.
        // A forked child's destructor must not release its parent's ownership.
        if std::process::id() == self.owner_pid { let _ = self.file.unlock(); }
    }
}

/// The runtime's exclusive lock. Normal release explicitly unlocks before closing; abrupt owner
/// death remains subject to OS descriptor lifetime. None means the runtime must not be mutated.
fn take_lock(dir: &Path) -> Result<Option<RecorderLock>, RecorderError> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("recorder.lock"))?;
    // A lock its owner lost abruptly can stay busy for a moment: a process forked meanwhile shares the
    // lock's file description until its exec closes it. An owning recorder holds the lock for its
    // lifetime, so a short bounded retry does not mistake that moment for an owner.
    let start = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(RecorderLock { file, owner_pid: std::process::id() })),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < std::time::Duration::from_millis(200) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Runs `body` as the runtime's only recorder; a failure is recorded in `runtime.json` by the lock holder.
async fn as_owner<F: std::future::Future<Output = Result<(), RecorderError>>>(dir: &Path, body: impl FnOnce() -> F) -> Result<(), RecorderError> {
    let Some(_lock) = take_lock(dir)? else {
        eprintln!("another recorder owns {}", dir.display());
        return Ok(());
    };
    // Terminal executions cannot be launched or adopted again. Reject under the ownership lock,
    // outside the failure writer, so a rejected request preserves the original terminal evidence.
    if let Some(record) = runtime::load_runtime(dir)? {
        match record.state {
            RuntimeState::Failed { reason, .. } => return Err(RecorderError::Missing(reason)),
            RuntimeState::Stopped => return Err(RecorderError::Missing("a stopped generation cannot resume".into())),
            _ => {}
        }
    }
    let result = body().await;
    if let Err(e) = &result {
        let typed = match e {
            RecorderError::Chromium(error) => {
                let code = execute::code_of(error);
                if code == frontcap_recorder_protocol::ErrorCode::Internal {
                    None // Transport/unknown startup failures retain recorder-fault semantics.
                } else {
                    let mut reply = frontcap_recorder_protocol::ErrorReply::new(code, e.to_string());
                    if code == frontcap_recorder_protocol::ErrorCode::InvalidArguments {
                        reply.recovery = "correct the launch inputs and launch with a new request_id; the failed generation cannot resume".into();
                    }
                    if error.sandbox_restricted() {
                        reply.recovery = "run the command the message names on this host once, then launch with a new request_id".into();
                    }
                    Some(reply)
                }
            }
            _ => None,
        };
        if native::spec_path(dir).exists() {
            let _ = native::mark_failed(dir, &e.to_string(), typed);
        } else {
            let _ = runtime::mark_failed_with_error(dir, &e.to_string(), typed);
        }
    }
    result
}

/// `frontcap-recorder launch --runtime-dir <dir>`.
pub async fn run_launch(dir: &Path) -> Result<(), RecorderError> {
    as_owner(dir, || async {
        if native::spec_path(dir).exists() { native::launch_owned(dir).await } else { launch_owned(dir).await }
    }).await
}

/// `frontcap-recorder recover --runtime-dir <dir>`.
pub async fn run_recover(dir: &Path) -> Result<(), RecorderError> {
    as_owner(dir, || async {
        if native::spec_path(dir).exists() {
            Err(RecorderError::Missing("native recorder restart cannot adopt a session; inspect saved results and retirement evidence".into()))
        } else { recover_owned(dir).await }
    }).await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProxyKeys {
    ca_key_pem: String,
    leaf_key_pem: String,
}

/// `FRONTCAP_EXTRA_ROOTS`: a PEM file of upstream roots besides the system's (a test server's authority).
fn extra_roots() -> Result<Vec<frontcap_proxy::CertificateDer<'static>>, RecorderError> {
    Ok(match std::env::var_os("FRONTCAP_EXTRA_ROOTS") {
        Some(path) => frontcap_proxy::certificates_from_pem(&std::fs::read(path)?),
        None => Vec::new(),
    })
}

/// The platform's connections to stream servers (passed-through streams).
pub(crate) fn upstream() -> Result<frontcap_proxy::upstream::Upstream, RecorderError> {
    frontcap_proxy::upstream::Upstream::new(extra_roots()?).map_err(|e| RecorderError::Missing(e.to_string()))
}

/// Where a cell's browser sees the cell's directory: one path in every cell, each with its own mounts,
/// under a directory only root writes.
pub(crate) const CELL_MOUNT: &str = "/run/frontcap-cell";

/// `FRONTCAP_CELLS=<uid>:<gid>` (Linux, privileged): each generation's browser runs in a cell, as that
/// unprivileged user (`frontcap_chromium::generation::Cell`).
pub(crate) fn cell_user() -> Result<Option<frontcap_chromium::generation::CellUser>, RecorderError> {
    let Some(value) = std::env::var_os("FRONTCAP_CELLS") else { return Ok(None) };
    let value = value.to_string_lossy();
    let (uid, gid) = value.split_once(':').and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)))
        .ok_or_else(|| RecorderError::Missing(format!("FRONTCAP_CELLS is <uid>:<gid>, not {value}")))?;
    Ok(Some(frontcap_chromium::generation::CellUser { uid, gid }))
}

/// Where a replacement recorder's proxy listens: the address the browser was given, in its cell if it has
/// one.
fn proxy_bind(endpoint: &frontcap_chromium::generation::Endpoint, route: &frontcap_chromium::generation::ProxyRoute) -> std::io::Result<frontcap_proxy::Bind> {
    Ok(match &endpoint.cell {
        Some(cell) => frontcap_proxy::Bind::Listener(cell.bind(route.addr.port())?),
        None => route.addr.into(),
    })
}

/// `dir`, made the owner's alone: it holds the proxy's keys and a cell's directory.
pub(crate) fn private(dir: &Path) -> Result<&Path, RecorderError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Ends a cell unless disarmed: a launch that fails after creating it.
struct EndUnlessDisarmed(Option<frontcap_chromium::generation::Cell>);

impl Drop for EndUnlessDisarmed {
    fn drop(&mut self) {
        if let Some(cell) = self.0.take() {
            cell.end();
        }
    }
}

/// Starts the generation's proxy: on `bind`, with the authority kept in the runtime directory when one
/// is there (a relaunch), else a new one, which is kept. `FRONTCAP_EXTRA_ROOTS` names a PEM file of
/// further upstream roots (a test server's authority).
async fn start_proxy(dir: &Path, bind: frontcap_proxy::Bind) -> Result<(frontcap_proxy::Proxy, frontcap_chromium::generation::ProxyRoute), RecorderError> {
    let kept: Option<ProxyKeys> = frontcap_core::state::load(&runtime::proxy_keys_path(dir))?;
    let extra_roots = extra_roots()?;
    let authority = kept.map(|k| frontcap_proxy::AuthorityKeys { ca_key_pem: k.ca_key_pem, leaf_key_pem: k.leaf_key_pem });
    let at = match &bind {
        frontcap_proxy::Bind::Addr(addr) => addr.to_string(),
        frontcap_proxy::Bind::Listener(listener) => listener.local_addr().map_or_else(|e| e.to_string(), |a| format!("{a} in the browser's cell")),
    };
    let proxy = frontcap_proxy::Proxy::start(frontcap_proxy::Options { bind, authority, extra_roots })
        .await
        .map_err(|e| RecorderError::Missing(format!("the proxy cannot start on {at}: {e}")))?;
    let keys = proxy.authority_keys();
    // The keys let anyone holding them stand in for any server this browser reaches: the owner's only.
    let path = runtime::proxy_keys_path(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    frontcap_core::state::save_atomic(&path, &ProxyKeys { ca_key_pem: keys.ca_key_pem, leaf_key_pem: keys.leaf_key_pem })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let route = frontcap_chromium::generation::ProxyRoute { addr: proxy.addr(), spki: proxy.spki_sha256_base64() };
    Ok((proxy, route))
}

async fn launch_owned(dir: &Path) -> Result<(), RecorderError> {
    let spec = runtime::load_spec(dir)?.ok_or_else(|| RecorderError::Missing(format!("no spec.json in {}", dir.display())))?;
    if let Some(name) = spec.restore.clone() {
        return restore_owned(dir, spec, &name).await;
    }
    let socket = runtime::socket_path(dir);
    let mut record = RuntimeRecord {
        launch_id: spec.launch_id.clone(),
        state: RuntimeState::Starting,
        recorder: ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: socket.clone(),
    };
    runtime::save_runtime(dir, &record)?;
    // Every connection the page opens goes through the recorder's proxy (criterion 11): in a cell, on the
    // cell's loopback, where the browser reaches it.
    let cell = match cell_user()? {
        Some(user) => Some(frontcap_chromium::generation::Cell::create(&private(dir)?.join("cell"), Path::new(CELL_MOUNT), user)
            .map_err(|e| RecorderError::Missing(format!("the browser's cell: {e}")))?),
        None => None,
    };
    let mut unused_cell = EndUnlessDisarmed(cell.clone());
    let listen = match &cell {
        Some(cell) => frontcap_proxy::Bind::Listener(cell.bind(0)?),
        None => "127.0.0.1:0".parse::<std::net::SocketAddr>().expect("an address").into(),
    };
    let (proxy, route) = start_proxy(dir, listen).await?;
    let recorded = dir.to_path_buf();
    let g = Generation::launch(LaunchOptions {
        repo_root: spec.repo_root.clone(),
        runs_root: spec.runs_root.clone(),
        run: spec.run_id.clone(),
        url: spec.url.clone(),
        profile: spec.profile.clone(),
        rules: spec.rules.clone(),
        passthrough_origins: spec.passthrough_origins.clone(),
        watchdog: Duration::from_millis(spec.watchdog_ms),
        browser_profile: cell.is_none().then(|| dir.join("profile")),
        launch_id: Some(spec.launch_id.clone()),
        // The browser outlives this process; its identity is recorded before anything else can fail.
        on_browser: Some(Box::new(move |identity| {
            if let Ok(Some(mut r)) = runtime::load_runtime(&recorded) {
                r.browser = Some(identity.clone());
                let _ = runtime::save_runtime(&recorded, &r);
            }
        })),
        proxy: Some(route),
        upstream: Some(upstream()?),
        cell,
    })
    .await?;
    // The generation owns its cell now: it ends it with its browser.
    unused_cell.0 = None;
    let inputs = frontcap_core::inputs::LaunchInputs {
        url: spec.url.clone(),
        profile: spec.profile.clone(),
        // As installed: a replayed cassette names the bundle's copy, which re-execution reads.
        rules: g.caller_rules().clone(),
        passthrough_origins: spec.passthrough_origins.clone(),
    };
    let bundle_root = g.trace_path().parent().unwrap_or(Path::new(".")).to_path_buf();
    if let Err(e) = frontcap_core::inputs::write_launch(&bundle_root, &inputs) {
        g.kill_browser();
        return Err(RecorderError::Missing(e.to_string()));
    }
    // A branch child names its saved basis in its evidence; it is not a re-execution input.
    if let Some(origin) = &spec.origin
        && let Err(e) = frontcap_core::state::save_atomic(&bundle_root.join("origin.json"), origin)
    {
        g.kill_browser();
        return Err(RecorderError::Missing(e.to_string()));
    }
    let endpoint = g.endpoint();
    record.browser = ProcessIdentity::of(endpoint.browser_pid)?;
    record.endpoint = Some(endpoint);
    let ready = async {
        let journal = Journal::open(&runtime::journal_path(dir))?;
        let control = ControlState::default();
        frontcap_core::state::save_atomic(&runtime::control_path(dir), &control)?;
        let listener = bind(&socket).await?;
        Ok::<_, RecorderError>((journal, control, listener))
    };
    let (journal, control, listener) = match ready.await {
        Ok(parts) => parts,
        Err(e) => {
            g.kill_browser();
            return Err(e);
        }
    };
    record.state = RuntimeState::Ready;
    runtime::save_runtime(dir, &record)?;
    let rec = server::Recorder::new(dir.to_path_buf(), spec, g, journal, control);
    server::serve(rec.clone(), listener).await?;
    drop(proxy);
    let _ = std::fs::remove_file(&socket);
    record.state = rec.failure_reason().map_or(RuntimeState::Stopped, |reason| RuntimeState::Failed { reason, error: None });
    runtime::save_runtime(dir, &record)?;
    Ok(())
}

/// A generation restored from a process image (`Call::Image`'s directory): its run continues the image's
/// evidence under its own run id, its cell is a copy of the image's in fresh namespaces where its proxy
/// listens at the browser's address before the restore, and it adopts the restored browser at the image's
/// boundary under its own launch id, keeping only what `ControlState::for_child` keeps.
/// Written in a restored child's runtime directory once its restore finished, just before it is ready: a
/// relaunched recorder that does not find it fails the child instead of resuming it.
const RESTORED: &str = "restored";

async fn restore_owned(dir: &Path, spec: runtime::LaunchSpec, name: &str) -> Result<(), RecorderError> {
    use frontcap_chromium::generation::{Endpoint, Image, Restoring};
    // Only an image this recorder's user made, in the workspace's own images: the restore brings its
    // processes back with the credentials it holds.
    let ws = launcher::Workspace::of_runtime(dir).ok_or_else(|| RecorderError::Missing("the recorder is outside a workspace".into()))?;
    let from = image::image_dir(&ws, name).map_err(RecorderError::Missing)?;
    image::private(&from).map_err(RecorderError::Missing)?;
    let from = from.as_path();
    let socket = runtime::socket_path(dir);
    let mut record = RuntimeRecord {
        launch_id: spec.launch_id.clone(),
        state: RuntimeState::Starting,
        recorder: ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: socket.clone(),
    };
    runtime::save_runtime(dir, &record)?;
    let load = |name: &str| std::fs::read(from.join(name)).map_err(|e| RecorderError::Missing(format!("the image's {name}: {e}")));
    let endpoint: Endpoint = serde_json::from_slice(&load("endpoint.json")?).map_err(|e| RecorderError::Missing(e.to_string()))?;
    let source: ControlState = serde_json::from_slice(&load("control.json")?).map_err(|e| RecorderError::Missing(e.to_string()))?;
    let route = endpoint.proxy.clone().ok_or_else(|| RecorderError::Missing("the image's browser had no proxy".into()))?;
    let run = spec.runs_root.join(spec.run_id.as_str());
    let copied = std::process::Command::new("cp").arg("-a").arg("--").arg(from.join("run")).arg(&run).status()?;
    if !copied.success() {
        return Err(RecorderError::Missing(format!("copying the image's evidence to {} failed: {copied}", run.display())));
    }
    // A branch child names its saved basis in its evidence, in place of any its source named.
    if let Some(origin) = &spec.origin {
        frontcap_core::state::save_atomic(&run.join("origin.json"), origin)?;
    }
    private(dir)?;
    std::fs::copy(from.join("proxy-keys.json"), runtime::proxy_keys_path(dir))?;
    let image = Image { images: from.join("images"), dir: from.join("cell") };
    let restoring = Restoring::prepare(&image, &dir.join("cell"), Path::new(CELL_MOUNT))
        .map_err(|e| RecorderError::Missing(format!("the child's cell: {e}")))?;
    let (proxy, _) = start_proxy(dir, frontcap_proxy::Bind::Listener(restoring.bind(route.addr.port())?)).await?;
    fault::point(spec.request_id.as_str(), "before_restore");
    let cell = restoring.restore(&image).map_err(|e| RecorderError::Missing(format!("restoring the image: {e}")))?;
    let mut unused_cell = EndUnlessDisarmed(Some(cell.clone()));
    let endpoint = Endpoint { cell: Some(cell.clone()), browser_pid: cell.init, ..endpoint };
    record.browser = ProcessIdentity::of(cell.init)?;
    record.endpoint = Some(endpoint.clone());
    runtime::save_runtime(dir, &record)?;
    fault::point(spec.request_id.as_str(), "after_restore");
    let mut control = source.for_child();
    // Its start is a process image, which re-execution cannot rebuild.
    control.missing_inputs.push(format!("the process image {} it was restored from", from.display()));
    let options = ReconnectOptions {
        repo_root: spec.repo_root.clone(),
        runs_root: spec.runs_root.clone(),
        run: spec.run_id.clone(),
        launch_id: spec.launch_id.clone(),
        profile: spec.profile.clone(),
        endpoint,
        watches: control.watches.clone(),
        watchdog: Duration::from_millis(spec.watchdog_ms),
        granted_until: None,
        dialog_rules: control.dialog_rules.clone(),
        upstream: Some(upstream()?),
    };
    let (mut g, _) = Generation::adopt(options, frontcap_core::event::ControllerChange::Restored).await?;
    if control.recording {
        g.record_start().await?;
    }
    frontcap_core::state::save_atomic(&runtime::control_path(dir), &control)?;
    let journal = Journal::open(&runtime::journal_path(dir))?;
    let listener = bind(&socket).await?;
    fault::point(spec.request_id.as_str(), "after_adopt");
    std::fs::write(dir.join(RESTORED), b"")?;
    unused_cell.0 = None;
    record.state = RuntimeState::Ready;
    runtime::save_runtime(dir, &record)?;
    let rec = server::Recorder::new(dir.to_path_buf(), spec, g, journal, control);
    server::serve(rec.clone(), listener).await?;
    drop(proxy);
    let _ = std::fs::remove_file(&socket);
    record.state = rec.failure_reason().map_or(RuntimeState::Stopped, |reason| RuntimeState::Failed { reason, error: None });
    runtime::save_runtime(dir, &record)?;
    Ok(())
}

/// Relaunches the recorder of a live generation whose recorder died, resolves every interrupted request,
/// and serves again.
async fn recover_owned(dir: &Path) -> Result<(), RecorderError> {
    let spec = runtime::load_spec(dir)?.ok_or_else(|| RecorderError::Missing(format!("no spec.json in {}", dir.display())))?;
    let mut record = runtime::load_runtime(dir)?.ok_or_else(|| RecorderError::Missing(format!("no runtime.json in {}", dir.display())))?;
    // A restore that never finished publishes no child: its control state and evidence were never
    // completed, so it is failed, and a browser it brought back is ended with its cell.
    if let Some(image) = &spec.restore
        && !dir.join(RESTORED).exists()
    {
        if let Some(cell) = record.endpoint.as_ref().and_then(|e| e.cell.as_ref())
            && record.browser.as_ref().is_some_and(|b| b.is_alive())
        {
            cell.end();
        }
        record.recorder = ProcessIdentity::current();
        record.state = RuntimeState::Failed { reason: format!("the restore from image {image} was interrupted before the child was ready; branch again with a new request id"), error: None };
        runtime::save_runtime(dir, &record)?;
        return Ok(());
    }
    let endpoint = record.endpoint.clone().ok_or_else(|| RecorderError::Missing("the generation never became ready".into()))?;
    if !record.browser.as_ref().is_some_and(|b| b.is_alive()) {
        record.state = RuntimeState::Failed { reason: "target_crashed: the browser is gone".into(), error: None };
        runtime::save_runtime(dir, &record)?;
        return Ok(());
    }
    record.recorder = ProcessIdentity::current();
    record.state = RuntimeState::Starting;
    runtime::save_runtime(dir, &record)?;
    // The browser keeps its proxy flags: the proxy comes back on the same port with the same authority,
    // or the generation cannot go on (a connection would fail, never go direct).
    let proxy = match &endpoint.proxy {
        Some(route) => match proxy_bind(&endpoint, route) {
            Ok(bind) => match start_proxy(dir, bind).await {
                Ok((proxy, _)) => Some(proxy),
                Err(e) => {
                    record.state = RuntimeState::Failed { reason: format!("target_crashed: {e}"), error: None };
                    runtime::save_runtime(dir, &record)?;
                    return Ok(());
                }
            },
            Err(e) => {
                record.state = RuntimeState::Failed { reason: format!("target_crashed: the proxy cannot listen in the browser's cell: {e}"), error: None };
                runtime::save_runtime(dir, &record)?;
                return Ok(());
            }
        },
        None => None,
    };
    let mut control: ControlState = frontcap_core::state::load(&runtime::control_path(dir))?.unwrap_or_default();
    if let Some(observations) = frontcap_core::state::load(&runtime::observations_path(dir))? {
        control.observations = observations;
    }
    let mut journal = Journal::open(&runtime::journal_path(dir))?;
    let options = ReconnectOptions {
        repo_root: spec.repo_root.clone(),
        runs_root: spec.runs_root.clone(),
        run: spec.run_id.clone(),
        launch_id: spec.launch_id.clone(),
        profile: spec.profile.clone(),
        endpoint,
        watches: control.watches.clone(),
        watchdog: Duration::from_millis(spec.watchdog_ms),
        granted_until: control.inflight.as_ref().and_then(|i| i.goal_us),
        dialog_rules: control.dialog_rules.clone(),
        upstream: Some(upstream()?),
    };
    let (g, reconnected) = dialog_recovery::reconnect(dir, &mut control, &mut journal, options).await?;
    // A fault at the start boundary restarts the generation in a fresh target.
    record.endpoint = Some(g.endpoint());
    let mut affected = reconnected.affected.clone();
    if let Some((granted, reached)) = reconnected.ran_past {
        affected.push(format!("virtual time ran from {} ms to {} ms unobserved", granted.0 / 1000, reached.0 / 1000));
    }
    affected.extend(journal.unresolved().iter().map(|r| r.to_string()));
    control.faults.push(frontcap_recorder_protocol::Fault {
        at_us: g.now(),
        affected,
        detail: "the recorder died and was relaunched".into(),
    });
    frontcap_core::state::save_atomic(&runtime::control_path(dir), &control)?;
    let mut g = g;
    // Recovery's temporary dialog-wait holds belong to its grant, not the configured rules.
    // Native cleanup has finished before this renderer-side restoration.
    if control.inflight.as_ref().is_some_and(|i| matches!(i.op, frontcap_recorder_protocol::Op::Next {
        event: frontcap_core::event::ProgressEvent::Dialog, ..
    })) {
        g.set_dialog_rules(control.dialog_rules.clone()).await?;
    }
    g.restore_observations(control.observations.clone());
    // Frames go on being recorded; those kept before the fault are already in the bundle.
    if control.recording {
        g.record_start().await?;
    }
    let rec = server::Recorder::new(dir.to_path_buf(), spec, g, journal, control);
    rec.cleanup_pending.store(true, std::sync::atomic::Ordering::SeqCst);
    recover::resolve(&rec).await?;
    rec.cleanup_inputs().await.map_err(std::io::Error::other)?;
    let listener = bind(&record.socket).await?;
    record.state = RuntimeState::Ready;
    runtime::save_runtime(dir, &record)?;
    server::serve(rec.clone(), listener).await?;
    drop(proxy);
    let _ = std::fs::remove_file(&record.socket);
    record.state = rec.failure_reason().map_or(RuntimeState::Stopped, |reason| RuntimeState::Failed { reason, error: None });
    runtime::save_runtime(dir, &record)?;
    Ok(())
}

#[cfg(test)]
#[path = "owner_tests.rs"]
mod owner_tests;
