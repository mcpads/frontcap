use crate::{ChromiumError, session::Session};
use cdp::protocol::{page, runtime};
use frontcap_core::profile::DeterminismProfile;

pub const WORLD: &str = "frontcap";
pub const BINDING: &str = "__frontcapReport";
/// Requests to this prefix are the observer's condition stops; they never reach the network.
pub const STOP_URL: &str = "https://frontcap.invalid/stop/";
const OBSERVER: &str = include_str!("js/observer.js");
const SEED: &str = include_str!("js/seed.js");
const FRAMES: &str = include_str!("js/frames.js");
const DIALOGS: &str = include_str!("js/dialogs.js");
const STREAMS: &str = include_str!("js/streams.js");
const STATE: &str = include_str!("js/state.js");

/// The names a generation's stream wrapper uses — the main world's registry (`window[key]`) and its
/// announcement event — derived from the launch id, so a relaunched recorder reaches the wrapper the
/// document already has.
/// The events dialogs.js announces dialogs on and receives rules on: derived from the launch, so a
/// relaunched recorder addresses the shims of documents its predecessor instrumented, and unknown to pages.
pub fn dialog_names(launch: &frontcap_core::ids::LaunchId) -> (String, String) {
    let h = frontcap_core::canonical::sha256_hex(&format!("{}:dialogs", launch.as_str()));
    (format!("frontcap-dialog-{}", &h[..16]), format!("frontcap-dialog-rules-{}", &h[16..32]))
}

/// The event the observer asks the main world for the focused document's composition with.
pub fn composition_event(launch: &frontcap_core::ids::LaunchId) -> String {
    let h = frontcap_core::canonical::sha256_hex(&format!("{}:composition", launch.as_str()));
    format!("frontcap-composition-{}", &h[..16])
}

/// dialogs.js with `rules` in force from each new document's start.
pub fn dialogs_script(launch: &frontcap_core::ids::LaunchId, rules: &frontcap_core::dialog::DialogRules, popups: frontcap_core::profile::PopupPolicy) -> String {
    let (event, rules_event) = dialog_names(launch);
    DIALOGS
        .replace("__FRONTCAP_POPUPS_BLOCKED__", if popups.is_block() { "true" } else { "false" })
        .replace("__FRONTCAP_DIALOG_EVENT__", &event)
        .replace("__FRONTCAP_DIALOG_RULES_EVENT__", &rules_event)
        .replace("__FRONTCAP_DIALOG_RULES__", &serde_json::to_string(rules).expect("rules serialize"))
        .replace("__FRONTCAP_COMPOSITION_EVENT__", &composition_event(launch))
}

/// The events the observer asks state.js for an app's state providers on, and receives the answer on:
/// derived from the launch, so pages cannot forge them and a relaunched recorder reaches the channels its
/// predecessor's documents have.
pub fn state_names(launch: &frontcap_core::ids::LaunchId) -> (String, String) {
    let h = frontcap_core::canonical::sha256_hex(&format!("{}:state", launch.as_str()));
    (format!("frontcap-state-{}", &h[..16]), format!("frontcap-state-reply-{}", &h[16..32]))
}

pub fn stream_names(launch: &frontcap_core::ids::LaunchId) -> (String, String) {
    let h = frontcap_core::canonical::sha256_hex(launch.as_str());
    (format!("__frontcap_streams_{}", &h[..16]), format!("frontcap-stream-{}", &h[16..32]))
}

/// The binding and the scripts `install` adds to a page target, in order; `dialogs` is the index of the
/// dialog script, which a change of dialog rules replaces.
pub struct Instrumentation {
    pub binding: runtime::AddBindingParams,
    pub scripts: Vec<page::AddScriptToEvaluateOnNewDocumentParams>,
    dialogs: usize,
}

/// The binding, the isolated-world observer, and the seeded `Math.random`. On the patched engine
/// (`patched`), which renders main frames at frame ticks only (engine patch 6), animation frames and
/// rendering observers are the engine's own.
pub fn instrumentation(profile: &DeterminismProfile, launch: &frontcap_core::ids::LaunchId, patched: bool, dialog_rules: &frontcap_core::dialog::DialogRules) -> Instrumentation {
    let binding = runtime::AddBindingParams {
        execution_context_name: Some(WORLD.into()),
        ..runtime::AddBindingParams::new(BINDING.into())
    };
    let frame_ms = (profile.frame_interval_us as f64 / 1000.0).to_string();
    let (dialog_event, _) = dialog_names(launch);
    let observer = OBSERVER
        .replace("__FRONTCAP_DIALOG_EVENT__", &dialog_event)
        .replace("__FRONTCAP_COMPOSITION_EVENT__", &composition_event(launch))
        .replace("__FRONTCAP_INITIAL_DATE_MS__", &profile.initial_date_ms.to_string())
        .replace("__FRONTCAP_NATIVE_CLOCK__", if patched { "true" } else { "false" })
        .replace("__FRONTCAP_FRAME_MS__", &frame_ms)
        .replace("__FRONTCAP_STOP_URL__", STOP_URL)
        .replace("__FRONTCAP_STREAM_EVENT__", &stream_names(launch).1)
        .replace("__FRONTCAP_STATE_REQUEST__", &state_names(launch).0)
        .replace("__FRONTCAP_STATE_REPLY__", &state_names(launch).1);
    let seed = SEED
        .replace("__FRONTCAP_SEED__", &(profile.random_seed % (1 << 32)).to_string())
        .replace("__FRONTCAP_DIALOG_EVENT__", &dialog_event);
    let frames = FRAMES
        .replace("__FRONTCAP_INITIAL_DATE_MS__", &profile.initial_date_ms.to_string())
        .replace("__FRONTCAP_FRAME_MS__", &frame_ms)
        .replace("__FRONTCAP_NATIVE_FRAMES__", if patched { "true" } else { "false" });
    let (key, event) = stream_names(launch);
    let scripts = vec![
        page::AddScriptToEvaluateOnNewDocumentParams { world_name: Some(WORLD.into()), ..page::AddScriptToEvaluateOnNewDocumentParams::new(observer) },
        page::AddScriptToEvaluateOnNewDocumentParams::new(seed),
        page::AddScriptToEvaluateOnNewDocumentParams::new(frames),
        page::AddScriptToEvaluateOnNewDocumentParams::new(dialogs_script(launch, dialog_rules, profile.popups)),
        page::AddScriptToEvaluateOnNewDocumentParams::new(STREAMS.replace("__FRONTCAP_STREAMS__", &key).replace("__FRONTCAP_DIALOG_EVENT__", &event)),
        page::AddScriptToEvaluateOnNewDocumentParams::new(STATE.replace("__FRONTCAP_STATE_REQUEST__", &state_names(launch).0).replace("__FRONTCAP_STATE_REPLY__", &state_names(launch).1)),
    ];
    Instrumentation { binding, scripts, dialogs: 3 }
}

/// Installs the instrumentation (`instrumentation`). Must run before the first navigation.
pub async fn install(page: &Session, profile: &DeterminismProfile, launch: &frontcap_core::ids::LaunchId, patched: bool, dialog_rules: &frontcap_core::dialog::DialogRules) -> Result<page::ScriptIdentifier, ChromiumError> {
    let i = instrumentation(profile, launch, patched, dialog_rules);
    page.call(&i.binding).await?;
    let mut dialogs = None;
    for (n, script) in i.scripts.iter().enumerate() {
        let id = page.call(script).await?.identifier;
        if n == i.dialogs {
            dialogs = Some(id);
        }
    }
    Ok(dialogs.expect("the instrumentation has a dialog script"))
}

/// Sends the instrumentation to a page held at creation, in order, without waiting for the answers: a
/// page with no renderer yet answers only once its first navigation proceeds.
pub fn send(page: &Session, i: &Instrumentation) -> Result<Vec<cdp::Answer>, ChromiumError> {
    let mut answers = vec![page.send(&i.binding)?];
    for script in &i.scripts {
        answers.push(page.send(script)?);
    }
    Ok(answers)
}
