//! A process image of a generation's browser at a stopped boundary (`Call::Image`): the recorder lets the
//! browser go, has its cell imaged, keeps beside the image what a child starts from (the run's evidence,
//! the control state, the launch spec and the endpoint), and takes the browser again.
use crate::execute::failed;
use crate::server::Recorder;
use frontcap_chromium::generation::{Generation, ReconnectOptions};
use frontcap_core::event::ControllerChange;
use frontcap_recorder_protocol::{Answer, ErrorCode, ErrorReply, Reply};
use std::path::Path;
use std::time::Duration;

/// Where the workspace keeps the image `name`, under a root only the recorder's user may enter (created so
/// if absent): a name of letters, digits, `-` and `_` only, so it names nothing outside that root.
pub fn image_dir(ws: &crate::launcher::Workspace, name: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(format!("an image name is 1-64 letters, digits, '-' or '_', not {name:?}"));
    }
    let root = ws.images();
    {
        use std::os::unix::fs::DirBuilderExt as _;
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(format!("{}: {e}", root.display())),
            _ => {}
        }
    }
    private(&root)?;
    Ok(root.join(name))
}

/// `path` is a directory (not a link) that only this process's user owns and may enter: nobody else can
/// have placed or changed what is read from it with this process's privileges.
pub fn private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.file_type().is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0 {
        Ok(())
    } else {
        Err(format!("{} is not a directory only this recorder's user owns and may enter", path.display()))
    }
}

pub(crate) async fn image(rec: &Recorder, guard: &mut Option<Generation>, name: &str) -> Answer {
    let g = guard.as_mut().expect("a live generation");
    let mut blockers = g.image_blockers();
    if blockers.is_empty() {
        match g.playing_media().await {
            // Media plays by the real clock: a child would not continue it as the page's time.
            Ok(media) => blockers.extend(media.into_iter().map(|m| format!("{m} (media plays by the real clock)"))),
            Err(e) => return failed(e),
        }
    }
    if !blockers.is_empty() {
        return Answer::Err(ErrorReply::new(ErrorCode::UnsupportedCapability, format!("a process image cannot carry {}", blockers.join("; "))));
    }
    let Some(ws) = crate::launcher::Workspace::of_runtime(&rec.dir) else {
        return Answer::Err(ErrorReply::new(ErrorCode::Internal, "the recorder is outside a workspace"));
    };
    let to = match image_dir(&ws, name) {
        Ok(to) => to,
        Err(e) => return Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, e)),
    };
    // Made here, by this recorder, only if absent: an image is never written into a directory found.
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&to) {
            return Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, format!("image {name}: {e}")));
        }
    }
    let to = to.as_path();
    let at = match guard.as_mut().expect("a live generation").observation_boundary().await {
        Ok(at) => at,
        Err(e) => return failed(e),
    };
    let endpoint = match guard.take().expect("a live generation").let_go() {
        Ok(endpoint) => endpoint,
        Err(e) => return failed(e),
    };
    // An image is no journaled request: a recorder that dies here is relaunched as after any fault.
    crate::fault::point(name, "after_let_go");
    let taken = keep(rec, &endpoint, to);
    crate::fault::point(name, "after_keep");
    // The browser is taken again whether or not the image was: it stopped at the boundary meanwhile.
    let control = rec.control.lock().unwrap().clone();
    let options = ReconnectOptions {
        repo_root: rec.spec.repo_root.clone(),
        runs_root: rec.spec.runs_root.clone(),
        run: rec.spec.run_id.clone(),
        launch_id: rec.spec.launch_id.clone(),
        profile: rec.spec.profile.clone(),
        endpoint,
        watches: control.watches.clone(),
        watchdog: Duration::from_millis(rec.spec.watchdog_ms),
        granted_until: None,
        dialog_rules: control.dialog_rules.clone(),
        upstream: crate::upstream().ok(),
    };
    match Generation::adopt(options, ControllerChange::Imaged).await {
        Ok((mut g, _)) => {
            // The page did not change: boundaries observed before the image stay current.
            g.continue_observations(control.observations.clone());
            rec.adopt_handles(&g);
            if control.recording
                && let Err(e) = g.record_start().await
            {
                *guard = Some(g);
                return failed(e);
            }
            *guard = Some(g);
            crate::fault::point(name, "after_adopt");
        }
        Err(e) => {
            rec.mark_failed(format!("the browser could not be taken again after its process image: {e}"));
            return failed(e);
        }
    }
    match taken {
        Ok(()) => Answer::Ok(Reply::Imaged { boundary: at }),
        Err(e) => Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, format!("the process image failed: {e}"))),
    }
}

/// The image and what a child starts from, under `to`.
fn keep(rec: &Recorder, endpoint: &frontcap_chromium::generation::Endpoint, to: &Path) -> Result<(), String> {
    let cell = endpoint.cell.as_ref().ok_or("the browser has no cell")?;
    // The image holds the page's memory, its profile and the proxy's keys: `to` is the owner's alone.
    cell.image(to).map_err(|e| e.to_string())?;
    // The browser trusts its proxy's authority by key: a child's proxy presents the same.
    std::fs::copy(crate::runtime::proxy_keys_path(&rec.dir), to.join("proxy-keys.json")).map_err(|e| e.to_string())?;
    // The run's evidence so far: nothing writes it while the browser is let go.
    let run = rec.spec.runs_root.join(rec.spec.run_id.as_str());
    let copied = std::process::Command::new("cp").arg("-a").arg("--").arg(&run).arg(to.join("run")).status().map_err(|e| e.to_string())?;
    if !copied.success() {
        return Err(format!("copying the run's evidence failed: {copied}"));
    }
    let control = rec.control.lock().unwrap().clone();
    kept(to, "control.json", &control)?;
    kept(to, "spec.json", &rec.spec)?;
    kept(to, "endpoint.json", endpoint)
}

fn kept<T: serde::Serialize>(to: &Path, name: &str, value: &T) -> Result<(), String> {
    frontcap_core::state::save_atomic(&to.join(name), value).map_err(|e| e.to_string())
}
