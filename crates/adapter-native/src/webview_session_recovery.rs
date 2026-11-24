//! Offline clock lookup uses the original worker result and independently validated clock journal.
use super::{Completion, Config, Launch, Message, Operation, Reply};
use crate::webview_store::RequestStore;
use frontcap_core::{canonical, ids::RequestId, process::ProcessIdentity, state::load};
use std::{io, path::Path};

fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| io::Error::other(format!("missing WebView evidence: {}", path.display())))
}

/// Never contacts the app, opens a store writer, repairs history or acquires target ownership.
/// Other operation families and missing worker publication remain unresolved.
pub fn read_clock_result(
    directory: &Path,
    config: &Config,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<Reply>> {
    let Operation::Clock { kind, delta_ns } = operation else { return Ok(None); };
    let Some(mut launch): Option<Launch> = load(&directory.join("launch.json"))? else { return Ok(None); };
    // The recorder inserts this generated service endpoint after freezing the profile. It is not
    // a clock identity; all declared consumer environment entries must still match the profile.
    if let Some(key) = &config.service_url_environment
        && !config.app.environment.contains_key(key) {
        launch.config.app.environment.remove(key);
    }
    if serde_json::to_value(&launch.config)? != serde_json::to_value(config)? {
        return Err(io::Error::other("WebView launch differs from frozen profile"));
    }
    let commands = directory.join("commands");
    if !commands.exists() { return Ok(None); }
    let mut found = None;
    for entry in std::fs::read_dir(commands)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { return Err(io::Error::other("invalid WebView command directory")); }
        let Some(command): Option<Message> = load(&entry.path().join("command.json"))? else { continue; };
        let Message::Execute { number, id: original_id, operation: original, .. } = command else {
            return Err(io::Error::other("invalid saved WebView command"));
        };
        if original_id != *id { continue; }
        if found.is_some() || entry.file_name().to_str() != Some(number.to_string().as_str()) || original != *operation {
            return Err(io::Error::other("WebView command binding differs or is ambiguous"));
        }
        found = Some(entry.path());
    }
    let Some(command) = found else { return Ok(None); };
    let Some(reply): Option<Reply> = load(&command.join("result.json"))? else { return Ok(None); };
    let completion = match &reply {
        Reply::Failed { .. } => return Ok(Some(reply)),
        Reply::Completed { completion } => completion,
    };
    let Completion::Clock { reply: clock } = completion.as_ref() else {
        return Err(io::Error::other("WebView completion differs from original clock operation"));
    };
    let simulator: String = required(&directory.join("target/simulator/simulator.json"))?;
    let simulator_launch: crate::simulator::SimulatorLaunch = required(&directory.join("target/simulator/launch.json"))?;
    if serde_json::to_value(simulator_launch)? != serde_json::to_value(&config.simulator)? {
        return Err(io::Error::other("WebView simulator differs from frozen profile"));
    }
    let attachment = canonical::sha256_hex(&canonical::canonical_of(&(
        &config.app.library.sha256, &config.app.script.as_ref().map(|script| &script.sha256), config.app.mute_audio,
    )));
    let mut epochs = vec![(directory.join("target/webview"), directory.join("target/app"))];
    if directory.join("epochs").exists() {
        for entry in std::fs::read_dir(directory.join("epochs"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() { return Err(io::Error::other("invalid WebView epoch directory")); }
            epochs.push((entry.path(), entry.path().join("app")));
        }
    }
    let mut matched = false;
    for (epoch, app) in epochs {
        if let Some((owner, saved)) = RequestStore::read_completion(&epoch.join("requests"), id, *kind, *delta_ns)
            .map_err(io::Error::other)? {
            let process: ProcessIdentity = required(&app.join("process.json"))?;
            if matched || owner.simulator != simulator || owner.process != process
                || owner.attachment_sha256 != attachment || saved != *clock {
                return Err(io::Error::other("WebView clock completion differs from original owner or journal"));
            }
            matched = true;
        }
    }
    Ok(matched.then_some(reply))
}
