//! Read-only lineage for native-host process replacement. Paths never confer live authority.
use super::{Binding, Config, Operation, Owner, Ready, bad, required};
use frontcap_core::{ids::RequestId, process::ProcessIdentity, state::load};
use serde::Deserialize;
use std::{collections::HashSet, io, path::{Path, PathBuf}};

pub(super) struct Epoch { pub ready: Ready, pub root: PathBuf, pub depth: usize }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent { previous: Owner, previous_evidence: PathBuf, contacts: bool, pages: bool }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppIntent {
    previous_process: ProcessIdentity, previous_evidence: PathBuf, simulator: String,
    container: PathBuf, bundle: String, attachment_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replaced { previous_process: ProcessIdentity, process: ProcessIdentity, container: PathBuf, simulator: String }

pub(super) fn current(directory: &Path, host: &Path) -> io::Result<Option<RequestId>> {
    let directory = directory.canonicalize()?;
    let host = host.canonicalize()?;
    if host == directory.join("target/host") { return Ok(None); }
    let root = directory.join("host-epochs");
    let parts = host.strip_prefix(root).map_err(bad)?.components().collect::<Vec<_>>();
    if parts.len() != 2 || parts[1].as_os_str() != "host" { return Err(bad("invalid host epoch directory")); }
    Ok(Some(RequestId::new(parts[0].as_os_str().to_str().ok_or_else(|| bad("non-UTF8 host epoch"))?).map_err(bad)?))
}
pub(super) fn unlinked(directory: &Path, path: &Path) -> io::Result<()> {
    let mut cursor = directory.to_path_buf();
    for part in path.strip_prefix(directory).map_err(bad)?.components() {
        if !matches!(part, std::path::Component::Normal(_)) { return Err(bad("invalid host evidence path")); }
        cursor.push(part);
        match std::fs::symlink_metadata(&cursor) {
            Ok(m) if m.file_type().is_symlink() => return Err(bad("linked host epoch evidence")),
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub(super) fn read<T: serde::de::DeserializeOwned>(directory: &Path, path: &Path) -> io::Result<T> {
    unlinked(directory, path)?; required(path)
}
/// Resolve from immutable initial readiness through request-bound replacements, without probing a
/// process or requiring the (possibly retired) consumer container to exist.
pub(super) fn resolve(directory: &Path, config: &Config, initial: &Ready, epoch: Option<&RequestId>) -> io::Result<Option<Epoch>> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut next = epoch.cloned();
    while let Some(id) = next {
        if chain.len() == 64 || !seen.insert(id.clone()) { return Err(bad("cyclic or exhausted host epoch lineage")); }
        let binding: Binding = read(directory, &directory.join("host-requests").join(id.as_str()).join("request.json"))?;
        let (Operation::Restart { previous } | Operation::BeginEntry { previous, .. }) = &binding.operation else { return Err(bad("host epoch lacks replacement request")); };
        if previous != &binding.owner { return Err(bad("host epoch previous owner differs")); }
        next = binding.epoch.clone(); chain.push((id, binding));
    }
    let mut state = Epoch { ready: initial.clone(), root: directory.join("target"), depth: 0 };
    for (id, binding) in chain.into_iter().rev() {
        let root = directory.join("host-epochs").join(id.as_str());
        let publication = root.join("replacement-ready.json"); unlinked(directory, &publication)?;
        let Some(ready): Option<Ready> = load(&publication)? else { return Ok(None); };
        config.validate_ready(&ready)?;
        if let Operation::BeginEntry { previous, url } = &binding.operation {
            if !config.cold_entry() {return Err(bad("cold entry unavailable in frozen launch"));}
            let accepted=crate::simulator_app::entry::read_begin(&root.join("app"),&directory.join("target/simulator"),previous,url)?;
            if accepted.is_none() {return Ok(None);}
            let published:RequestId=read(directory,&root.join("entry-published-by.json"))?;
            let poll:Binding=read(directory,&directory.join("host-requests").join(published.as_str()).join("request.json"))?;
            if poll.owner!=binding.owner || poll.epoch!=binding.epoch
                || poll.operation!=(Operation::PollEntry {begin:id.clone()}) {return Err(bad("cold replacement publication differs from original poll"));}
        }
        if let (Some(system), Some(runner)) = (&config.system_ui, &ready.system_ui) {
            crate::ios_host::system::verify_ready(&root.join("host"), system, &config.simulator.source_sha256, &ready.owner, runner)?;
            if state.ready.system_ui.as_ref() == Some(runner) { return Err(bad("host replacement reused system runner")); }
        }
        let intent: Intent = read(directory, &root.join("replacement-intent.json"))?;
        if binding.owner != state.ready.owner || intent.previous != state.ready.owner
            || intent.previous_evidence != state.root.join("host")
            || intent.contacts != config.contacts || intent.pages != config.app.page_registry
            || ready.owner.simulator != state.ready.owner.simulator
            || ready.owner.attachment_sha256 != state.ready.owner.attachment_sha256
            || ready.owner.process == state.ready.owner.process
            || (config.contacts && ready.contacts.as_ref().map(|e| &e.generation) == state.ready.contacts.as_ref().map(|e| &e.generation))
        { return Err(bad("host epoch lineage differs")); }
        let app = root.join("app");
        if crate::simulator_app::prepared::read_installed(&app, &config.app, &ready.owner.simulator)?
            != crate::simulator_app::prepared::read_installed(&state.root.join("app"), &config.app, &ready.owner.simulator)? {
            return Err(bad("prepared app installation changed across host epochs"));
        }
        let intent: AppIntent = read(directory, &app.join("restart-intent.json"))?;
        let receipt: Replaced = read(directory, &app.join("restarted.json"))?;
        let container: PathBuf = read(directory, &state.root.join("app/container.json"))?;
        if !container.is_absolute() || intent.container != container || intent.bundle != config.app.bundle
            || intent.previous_process != state.ready.owner.process || intent.previous_evidence != state.root.join("app")
            || intent.simulator != ready.owner.simulator || intent.attachment_sha256 != ready.owner.attachment_sha256
            || receipt.previous_process != state.ready.owner.process || receipt.process != ready.owner.process
            || receipt.simulator != ready.owner.simulator || receipt.container != container
            || read::<ProcessIdentity>(directory, &app.join("terminated.json"))? != state.ready.owner.process
            || read::<ProcessIdentity>(directory, &app.join("process.json"))? != ready.owner.process
            || read::<PathBuf>(directory, &app.join("container.json"))? != container
            || read::<Owner>(directory, &root.join("host/ready.json"))? != ready.owner
        { return Err(bad("host epoch app replacement evidence differs")); }
        let mut expected = config.app.clone();
        expected.library.path = app.join("attachment.dylib");
        if let Some(script) = &mut expected.script { script.path = app.join("attachment.js"); }
        for name in ["launch.json", "launch-intent.json"] {
            let launch: crate::simulator_app::Launch = read(directory, &app.join(name))?;
            if serde_json::to_value(launch)? != serde_json::to_value(&expected)? { return Err(bad("host replacement launch differs")); }
        }
        for file in std::iter::once(&expected.library).chain(expected.script.iter()) {
            unlinked(directory, &file.path)?;
            if frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&file.path)?) != file.sha256 {
                return Err(bad("host replacement attachment bytes differ"));
            }
        }
        state = Epoch { ready, root, depth: state.depth + 1 };
    }
    Ok(Some(state))
}
