//! Verify raw iOS receipts and observation bytes before request-store completion or offline return.
use frontcap_core::{
    canonical::sha256_bytes_hex,
    native::{
        ios::{ClockCompletion, ClockStatus},
        ios_request::{Completion, Owner, Request},
        ios_ui::{UiCompletion, UiOutcome, UiRequest},
        observation::Artifact,
        request::ClockRequest,
    },
};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};
#[derive(Deserialize)]
struct ClockReceipt {
    request: ClockRequest,
    generation: String,
    pid: u32,
    last_id: u64,
    status: ClockStatus,
    before_ns: u64,
    now_ns: u64,
    applied: bool,
    advance_result: Option<u32>,
    drain_result: Option<u32>,
    port: u16,
}
#[derive(Deserialize)]
struct UiReceipt<R> {
    request: UiRequest,
    generation: String,
    pid: u32,
    last_id: u64,
    status: String,
    result: R,
    port: u16,
}
#[derive(Deserialize)]
struct Endpoint {
    generation: String,
    pid: u32,
    port: u16,
}
impl Endpoint {
    fn matches(&self, owner: &Owner) -> bool {
        self.generation == owner.app.generation && self.pid == owner.app.pid && self.port != 0
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Targets {
    state: String,
    app: Endpoint,
    now_ns: u64,
    catalog: frontcap_core::native::ios_targets::Catalog,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemAlerts {
    state: String,
    catalog: frontcap_core::native::ios_system_ui::Catalog,
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
#[serde(tag = "input", rename_all = "snake_case", deny_unknown_fields)]
enum SystemInput {
    XctestSystemTap { reference: String, observation: String },
    NotApplied { reference: String, observation: String, reason: frontcap_core::native::ios_system_ui::NotAppliedReason },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Press {
    app: Endpoint,
    #[serde(flatten)]
    evidence: frontcap_core::native::ios_press::Evidence,
}
#[derive(Deserialize)]
#[serde(tag="input",rename_all="snake_case",deny_unknown_fields)]
enum TextEdit {
    UikitTextControl { app:Endpoint, reference:String, observation:String, now_ns:u64,
        receipt:Box<frontcap_core::native::ios_edit::Receipt> },
    NotApplied { app:Endpoint, reference:String, observation:String, now_ns:u64,
        reason:frontcap_core::native::ios_targets::NotAppliedReason },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PressNotApplied {
    input: String,
    app: Endpoint,
    reference: String,
    observation: String,
    clock_request: frontcap_core::native::request::ClockRequest,
    reason: frontcap_core::native::ios_targets::NotAppliedReason,
}
#[derive(Deserialize)]
#[serde(tag = "input", rename_all = "snake_case", deny_unknown_fields)]
enum TargetInput {
    XctestTap {
        app: Endpoint,
        reference: String,
        observation: String,
    },
    NotApplied {
        app: Endpoint,
        reference: String,
        observation: String,
        reason: frontcap_core::native::ios_targets::NotAppliedReason,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scrolled {
    input:String, app:Endpoint, reference:String, observation:String, now_ns:u64,
    direction:frontcap_core::native::ios_scroll::Direction,
    before:frontcap_core::native::ios_scroll::Scroll, after:frontcap_core::native::ios_scroll::Scroll,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unavailable {
    state:String, app:Endpoint, now_ns:u64,
    reason:frontcap_core::native::ios_ui::UnavailableReason, artifacts:Vec<Artifact>,
}
#[derive(Deserialize)]
struct Retirement {
    endpoint: Endpoint,
    process_absent: bool,
    port_closed: bool,
}
#[derive(Deserialize)]
struct Finished {
    state: String,
    retirement: Retirement,
}
fn ui_result<R: DeserializeOwned>(bytes: &[u8], expected: &UiCompletion) -> io::Result<R> {
    let raw: UiReceipt<R> = serde_json::from_slice(bytes)?;
    if raw.status != "ok"
        || raw.port == 0
        || raw.request != expected.request
        || raw.generation != expected.generation
        || raw.pid != expected.pid
        || raw.last_id != expected.last_id
    {
        return Err(io::Error::other("iOS raw UI receipt identity differs"));
    }
    Ok(raw.result)
}
pub fn verify(
    directory: &Path,
    owner: &Owner,
    request: &Request,
    completion: &Completion,
) -> io::Result<()> {
    completion
        .validate(owner, request)
        .map_err(io::Error::other)?;
    if let Completion::Press { ui, clock } = completion {
        use frontcap_core::native::ios_request::PressClock;
        verify(directory,owner,&Request::Ui(ui.request.clone()),&Completion::Ui(ui.clone()))?;
        return match clock {
            PressClock::Delivered { completion } => verify(directory,owner,
                &Request::Clock(completion.request.clone()),&Completion::Clock(completion.clone())),
            PressClock::Released { request, ui_receipt_sha256 } => {
                let root=directory.join("clock-client");
                let releases=root.join("releases");
                for path in [&root,&releases] {
                    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                        return Err(io::Error::other("linked clock release directory"));
                    }
                }
                let file=std::fs::OpenOptions::new().read(true)
                    .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK)
                    .open(releases.join(format!("{ui_receipt_sha256}.json")))?;
                if !file.metadata()?.is_file() { return Err(io::Error::other("invalid clock release file")); }
                let mut raw=Vec::new();file.take(65537).read_to_end(&mut raw)?;
                if raw.len()>65536 { return Err(io::Error::other("oversized clock release")); }
                let reply: crate::ios::ClockReply=serde_json::from_slice(&raw)?;
                reply.validate(&crate::ios::ClockCall::ReleaseUi {
                    identity: owner.app.clone(),request: request.clone(),ui_identity: owner.runner.clone(),
                    ui_request: ui.request.clone(),receipt_sha256: ui_receipt_sha256.clone(),
                }).map_err(io::Error::other)
            }
        };
    }
    let (client, digest) = match completion {
        Completion::Clock(c) => ("clock-client", &c.receipt_sha256),
        Completion::Ui(c) => ("ui-client", &c.receipt_sha256),
        Completion::Press { .. } => unreachable!(),
    };
    let bytes = read_receipt(directory, client, digest)?;
    let root = directory.join(client);
    match completion {
        Completion::Press { .. } => unreachable!(),
        Completion::Clock(expected) => {
            let r: ClockReceipt = serde_json::from_slice(&bytes)?;
            if r.port == 0 {
                return Err(io::Error::other("invalid iOS clock endpoint"));
            }
            let actual = ClockCompletion {
                request: r.request,
                generation: r.generation,
                pid: r.pid,
                last_id: r.last_id,
                status: r.status,
                before_ns: r.before_ns,
                now_ns: r.now_ns,
                applied: r.applied,
                advance_result: r.advance_result,
                drain_result: r.drain_result,
                receipt_sha256: digest.clone(),
            };
            if &actual != expected {
                return Err(io::Error::other("iOS raw clock result differs"));
            }
        }
        Completion::Ui(expected) => {
            let actual = match &expected.outcome {
                UiOutcome::AppStateChanged { .. } => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct StateChanged {state:String,app:Endpoint,change:frontcap_core::native::ios_lifecycle::StateChange}
                    let raw:StateChanged=ui_result(&bytes,expected)?;
                    if raw.state!="app_state_changed" || !raw.app.matches(owner) {return Err(io::Error::other("UIKit host state receipt owner differs"));}
                    raw.change.validate().map_err(io::Error::other)?;
                    UiOutcome::AppStateChanged {app:owner.app.clone(),change:raw.change}
                },
                UiOutcome::HostAppStateChanged { .. } | UiOutcome::SystemAlertsObserved { .. } | UiOutcome::SystemButtonTapped { .. }
                    | UiOutcome::SystemButtonNotApplied { .. } => system_outcome(&bytes, expected)?,
                UiOutcome::Scrolled { .. } => {
                    let r:Scrolled=ui_result(&bytes,expected)?;
                    if r.input!="xctest_scroll" || !r.app.matches(owner) { return Err(io::Error::other("UIKit scroll owner differs")); }
                    UiOutcome::Scrolled { app:owner.app.clone(),reference:r.reference,observation:r.observation,
                        now_ns:r.now_ns,direction:r.direction,before:r.before,after:r.after }
                }
                UiOutcome::TextEdit { .. } => {
                    let r:TextEdit=ui_result(&bytes,expected)?;
                    use frontcap_core::native::ios_edit::Delivery;
                    let (app,reference,observation,now_ns,delivery)=match r {
                        TextEdit::UikitTextControl { app,reference,observation,now_ns,receipt } =>
                            (app,reference,observation,now_ns,Delivery::UikitTextControl { receipt }),
                        TextEdit::NotApplied { app,reference,observation,now_ns,reason } =>
                            (app,reference,observation,now_ns,Delivery::NotApplied { reason }),
                    };
                    if !app.matches(owner) { return Err(io::Error::other("UIKit text app identity differs")); }
                    UiOutcome::TextEdit { app:owner.app.clone(),reference,observation,now_ns,delivery }
                }
                UiOutcome::TargetPressed { .. } => {
                    let r: Press = ui_result(&bytes, expected)?;
                    if !r.app.matches(owner) || r.app.port != r.evidence.clock_reply.port {
                        return Err(io::Error::other("UIKit press clock identity differs"));
                    }
                    UiOutcome::TargetPressed { app: owner.app.clone(), evidence: r.evidence }
                }
                UiOutcome::TargetPressNotApplied { .. } => {
                    let r: PressNotApplied = ui_result(&bytes, expected)?;
                    if r.input != "not_applied" || !r.app.matches(owner) {
                        return Err(io::Error::other("UIKit press rejection identity differs"));
                    }
                    UiOutcome::TargetPressNotApplied { app: owner.app.clone(), reference: r.reference,
                        observation: r.observation, clock_request: r.clock_request, reason: r.reason }
                }
                UiOutcome::ObservationUnavailable { .. } => {
                    let r:Unavailable=ui_result(&bytes,expected)?;
                    if r.state!="unavailable" || !r.app.matches(owner) { return Err(io::Error::other("unavailable observation owner differs")); }
                    UiOutcome::ObservationUnavailable { app:owner.app.clone(),now_ns:r.now_ns,reason:r.reason,artifacts:r.artifacts }
                }
                UiOutcome::ObservedTargets { .. } => {
                    let r: Targets = ui_result(&bytes, expected)?;
                    if r.state != "observed" || !r.app.matches(owner) {
                        return Err(io::Error::other("UIKit catalog identity differs"));
                    }
                    UiOutcome::ObservedTargets {
                        app: owner.app.clone(),
                        now_ns: r.now_ns,
                        catalog: r.catalog,
                        artifacts: r.artifacts,
                    }
                }
                UiOutcome::TargetTapped { .. } | UiOutcome::TargetNotApplied { .. } => {
                    match ui_result::<TargetInput>(&bytes, expected)? {
                        TargetInput::XctestTap {
                            app,
                            reference,
                            observation,
                        } => {
                            if !app.matches(owner) {
                                return Err(io::Error::other("UIKit input identity differs"));
                            }
                            UiOutcome::TargetTapped {
                                app: owner.app.clone(),
                                reference,
                                observation,
                            }
                        }
                        TargetInput::NotApplied {
                            app,
                            reference,
                            observation,
                            reason,
                        } => {
                            if !app.matches(owner) {
                                return Err(io::Error::other("UIKit rejection identity differs"));
                            }
                            UiOutcome::TargetNotApplied {
                                app: owner.app.clone(),
                                reference,
                                observation,
                                reason,
                            }
                        }
                    }
                }
                UiOutcome::Terminated { .. } => {
                    let r: Finished = ui_result(&bytes, expected)?;
                    if r.state != "terminated" || !r.retirement.endpoint.matches(owner) {
                        return Err(io::Error::other("iOS raw retirement identity differs"));
                    }
                    UiOutcome::Terminated {
                        app: owner.app.clone(),
                        process_absent: r.retirement.process_absent,
                        port_closed: r.retirement.port_closed,
                    }
                }
                _ => return Err(io::Error::other("unsupported stored iOS completion")),
            };
            if actual != expected.outcome {
                return Err(io::Error::other("iOS raw UI result differs"));
            }
            if matches!(
                actual,
                UiOutcome::SystemAlertsObserved { .. } | UiOutcome::ObservedTargets { .. } | UiOutcome::ObservationUnavailable { .. }
            ) {
                crate::ios_observation::verify(&actual, &root)?;
            }
        }
    }
    Ok(())
}

fn system_outcome(bytes: &[u8], expected: &UiCompletion) -> io::Result<UiOutcome> {
    match &expected.outcome {
        UiOutcome::HostAppStateChanged { .. } => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Changed { state: String, change: frontcap_core::native::ios_ui::HostAppStateChange }
            let raw: Changed = ui_result(bytes, expected)?;
            if raw.state != "host_app_state_changed" { return Err(io::Error::other("invalid host state receipt")); }
            raw.change.validate().map_err(io::Error::other)?;
            Ok(UiOutcome::HostAppStateChanged { change: raw.change })
        }
        UiOutcome::SystemAlertsObserved { .. } => {
            let r: SystemAlerts = ui_result(bytes, expected)?;
            if r.state != "system_observed" { return Err(io::Error::other("unconfirmed system observation")); }
            Ok(UiOutcome::SystemAlertsObserved { catalog: r.catalog, artifacts: r.artifacts })
        }
        UiOutcome::SystemButtonTapped { .. } | UiOutcome::SystemButtonNotApplied { .. } => {
            Ok(match ui_result::<SystemInput>(bytes, expected)? {
                SystemInput::XctestSystemTap { reference, observation } => UiOutcome::SystemButtonTapped { reference, observation },
                SystemInput::NotApplied { reference, observation, reason } => UiOutcome::SystemButtonNotApplied { reference, observation, reason },
            })
        }
        _ => Err(io::Error::other("not a system UI receipt")),
    }
}
/// Verify system-only receipts against the actual runner, raw receipt and retained artifact bytes.
pub fn verify_system(client: &Path, runner: &frontcap_core::native::ios_ui::UiIdentity,
    completion: &UiCompletion) -> io::Result<()> {
    completion.validate_system(runner, &completion.request).map_err(io::Error::other)?;
    let bytes = read_receipt(client, "", &completion.receipt_sha256)?;
    let actual = system_outcome(&bytes, completion)?;
    if actual != completion.outcome { return Err(io::Error::other("system UI raw receipt differs")); }
    if matches!(actual, UiOutcome::SystemAlertsObserved { .. }) { crate::ios_observation::verify(&actual, client)?; }
    Ok(())
}

fn read_receipt(directory: &Path, client: &str, digest: &str) -> io::Result<Vec<u8>> {
    let root = directory.join(client);
    let receipts = root.join("receipts");
    if std::fs::symlink_metadata(&root)?.file_type().is_symlink()
        || std::fs::symlink_metadata(&receipts)?
            .file_type()
            .is_symlink()
    {
        return Err(io::Error::other("linked iOS receipt directory"));
    }
    let path = receipts.join(format!("{digest}.json"));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("invalid iOS receipt file"));
    }
    let mut bytes = Vec::new();
    f.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 || sha256_bytes_hex(&bytes) != digest {
        return Err(io::Error::other("iOS receipt bytes changed"));
    }
    Ok(bytes)
}
/// Acquisition has no prior app identity; verify its raw receipt before binding the new app.
pub fn verify_launch(
    directory: &Path,
    runner: &frontcap_core::native::ios_ui::UiIdentity,
    completion: &UiCompletion,
) -> io::Result<()> {
    completion
        .validate(runner, &completion.request, None)
        .map_err(io::Error::other)?;
    let UiOutcome::Launched { app } = &completion.outcome else {
        return Err(io::Error::other("expected initial launch"));
    };
    if completion.request.request_id != 1 {
        return Err(io::Error::other("initial launch sequence differs"));
    }
    #[derive(Deserialize)]
    struct Reads {
        endpoint: Endpoint,
    }
    #[derive(Deserialize)]
    struct Launched {
        state: String,
        reads: Reads,
    }
    let bytes = read_receipt(directory, "ui-client", &completion.receipt_sha256)?;
    let raw: Launched = ui_result(&bytes, completion)?;
    if raw.state != "launched"
        || raw.reads.endpoint.generation != app.generation
        || raw.reads.endpoint.pid != app.pid
        || raw.reads.endpoint.port == 0
    {
        return Err(io::Error::other("raw launched app differs"));
    }
    Ok(())
}

/// Verify the original same-runner replacement receipt before creating a new app epoch.
pub fn verify_restart(directory: &Path, owner: &Owner, completion: &UiCompletion) -> io::Result<()> {
    completion.validate(&owner.runner, &completion.request, Some(&owner.app)).map_err(io::Error::other)?;
    let UiOutcome::Restarted { previous, app, process_absent, port_closed, now_ns } = &completion.outcome else {
        return Err(io::Error::other("expected UIKit replacement completion"));
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reads { endpoint: Endpoint, now_ns: u64 }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Restarted { state: String, retirement: Retirement, reads: Reads, #[serde(default)] url: Option<String> }
    let bytes = read_receipt(directory, "ui-client", &completion.receipt_sha256)?;
    let raw: Restarted = ui_result(&bytes, completion)?;
    let frontcap_core::native::ios_ui::UiOperation::Restart { url, .. } = &completion.request.operation else {
        return Err(io::Error::other("missing UIKit restart request"));
    };
    if &raw.url != url { return Err(io::Error::other("raw UIKit entry URL differs")); }
    if raw.state != "restarted" || previous != &owner.app || !raw.retirement.endpoint.matches(owner)
        || raw.retirement.process_absent != *process_absent || raw.retirement.port_closed != *port_closed
        || raw.reads.endpoint.generation != app.generation || raw.reads.endpoint.pid != app.pid
        || raw.reads.endpoint.port == 0 || raw.reads.now_ns != *now_ns {
        return Err(io::Error::other("raw UIKit replacement differs from completion"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "ios_receipt_tests.rs"]
mod tests;
