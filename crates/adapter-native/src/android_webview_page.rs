//! Selected Android page reads and journaled preparation. No automatic reconnect or effect replay.
use crate::{
    android_webview::{Debugger, Page},
    device::Device,
};
use frontcap_core::{
    ids::RequestId,
    state::load,
    write_budget::{Class, WriteBudget},
};
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

const READ: &str = include_str!("android_page_read.js");
#[derive(Serialize)]
struct ReadPage {
    expression: &'static str,
    #[serde(rename = "returnByValue")]
    return_by_value: bool,
}
impl cdp::Command for ReadPage {
    const METHOD: &'static str = "Runtime.evaluate";
    type Returns = serde_json::Value;
}
const FRAME_GATE: &str = include_str!("../../../spikes/android-webview-clock/frame-gate.js");
fn prepared_script(random_seed: Option<u32>)->String {
    let observer=include_str!("../../../spikes/webview-timeline/pointers.js")
        .replace("export function observePointers", "function observePointers");
    let navigation=include_str!("../../../spikes/webview-timeline/navigation.js")
        .replace("export function observeNavigation", "function observeNavigation");
    let events=include_str!("../../../spikes/webview-timeline/page-events.js")
        .replace("export function observePageEvents", "function observePageEvents");
    let media=include_str!("../../../spikes/webview-timeline/media.js")
        .replace("export function observeMedia", "function observeMedia");
    let timers=include_str!("../../../spikes/android-webview-clock/timer-observer.js")
        .replace("export function observeTimers", "function observeTimers");
    let mut script = format!("{FRAME_GATE}\n(() => {{ {observer}\n{navigation}\n{timers}\n{events}\n{media}\nObject.defineProperty(globalThis, '__frontcapMedia', {{value:observeMedia(globalThis), configurable:false, writable:false}});\nObject.defineProperty(globalThis, '__frontcapPointers', {{value:observePointers(globalThis), configurable:false, writable:false}});\nObject.defineProperty(globalThis, '__frontcapNavigation', {{value:observeNavigation(globalThis, __frontcapFrames.receipt().document), configurable:false, writable:false}});\nObject.defineProperty(globalThis, '__frontcapEvents', {{value:observePageEvents(globalThis), configurable:false, writable:false}});\nObject.defineProperty(globalThis, '__frontcapTimers', {{value:observeTimers(globalThis, __frontcapFrames.receipt().document), configurable:false, writable:false}}); }})();");
    if let Some(seed) = random_seed {
        let seeded = include_str!("android_starting_randomness.js")
            .replace("__FRONTCAP_RANDOM_SOURCE__", include_str!("../../adapter-chromium/src/js/seed.js"))
            .replace("__FRONTCAP_RANDOM_OBSERVER__", include_str!("../../../spikes/webview-timeline/random_observer.js"))
            .replace("__FRONTCAP_SEED__", &seed.to_string())
            .replace("__FRONTCAP_DIALOG_EVENT__", "frontcap:starting-randomness");
        script.push_str(&seeded);
    }
    script
}
#[derive(Serialize)]
struct EnablePage {}
impl cdp::Command for EnablePage {
    const METHOD: &'static str = "Page.enable";
    type Returns = serde_json::Value;
}
#[derive(Serialize)]
struct EnableRuntime {}
impl cdp::Command for EnableRuntime {
    const METHOD: &'static str = "Runtime.enable";
    type Returns = serde_json::Value;
}
#[derive(Serialize)]
struct AddFrameGate {
    source: String,
}
impl cdp::Command for AddFrameGate {
    const METHOD: &'static str = "Page.addScriptToEvaluateOnNewDocument";
    type Returns = serde_json::Value;
}
#[derive(Serialize)]
struct Navigate<'a> {
    url: &'a str,
}
impl cdp::Command for Navigate<'_> {
    const METHOD: &'static str = "Page.navigate";
    type Returns = serde_json::Value;
}
#[derive(Serialize)]
struct Reload {}
impl cdp::Command for Reload {
    const METHOD: &'static str = "Page.reload";
    type Returns = serde_json::Value;
}
pub use frontcap_core::native::android_webview::{Prepared, Snapshot};
pub(crate) async fn connect(
    page: &Page,
    evidence: &Path,
    deadline: Instant,
) -> io::Result<cdp::Client> {
    std::fs::create_dir(evidence)?;
    save_evidence(&evidence.join("selected-page.json"), page)?;
    tokio::time::timeout_at(deadline.into(), cdp::Client::connect(&page.websocket))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Android page connection deadline"))?
        .map_err(io::Error::other)
}
pub struct Connection<'a> {
    owner: &'a mut Device,
    debugger: Debugger,
    page: Page,
    client: cdp::Client,
    console: crate::android_console::Observer,
    navigation: crate::android_navigation::Observer,
    directory: PathBuf,
    closed: bool,
    close_on_drop: bool,
    frames: Option<crate::android_frames::Ledger>,
    needs_receipt: bool,
    timer_session_changed: bool,
}
impl<'a> Connection<'a> {
    /// Host operations retain process ownership even after this page disappears or closes.
    pub(crate) fn host_device(&mut self) -> &mut Device { self.owner }

    pub(crate) fn inspect_lifecycle_progress(&self) -> Option<frontcap_core::native::android_progress::Lifecycle> {
        self.owner.inspect_lifecycle_progress()
    }

    /// Read retained local evidence without contacting the page, reconciling effects or granting a basis.
    pub fn inspect_progress(&self) -> io::Result<frontcap_core::native::android_progress::Progress> {
        use frontcap_core::native::{android_progress::Progress, webview_progress::{Basis, Service}};
        Ok(Progress {
            owner: self.debugger.clone(), page_id: self.page.id.clone(), basis: Basis::RetainedReceipts,
            connection_closed: self.closed, frames_need_reconciliation: self.needs_receipt,
            timers_need_pause: self.timer_session_changed,
            connection_navigation: Some(self.navigation.snapshot()),
            frames: self.frames.as_ref().map(crate::android_frames::Ledger::inspect),
            timers: load(&self.directory.join("timer-state.json"))?,
            lifecycle: self.owner.inspect_lifecycle_progress(),
            contacts: self.owner.inspect_contact_progress(), service: Service::NotInspected,
            limitations: vec!["retained_local_state_not_live_or_atomic".into(),
                "frame_memory_may_differ_from_disk_after_storage_failure".into(),
                "timer_policy_not_pending_timer_inventory".into(),
                "native_input_receipt_not_page_consumption".into(),
                "navigation_focus_and_pixels_not_sampled".into(),
                "host_transition_receipt_not_page_lifecycle_delivery".into(),
                "lifecycle_frontier_not_event_history_or_restart_recovery".into(),
                "unknown_producers_not_idle".into()],
        })
    }
    pub(crate) fn new(
        owner: &'a mut Device,
        debugger: Debugger,
        page: Page,
        client: cdp::Client,
        directory: PathBuf,
    ) -> Self {
        Self {
            owner,
            debugger,
            page,
            console: crate::android_console::Observer::new(&client),
            navigation: crate::android_navigation::Observer::new(&client),
            client,
            directory,
            closed: false,
            close_on_drop: true,
            frames: None,
            needs_receipt: false,
            timer_session_changed: false,
        }
    }
    pub(crate) fn restored(
        owner: &'a mut Device,
        debugger: Debugger,
        page: Page,
        client: cdp::Client,
        directory: PathBuf,
        ledger: crate::android_frames::Ledger,
    ) -> Self {
        let mut connection = Self::new(owner, debugger, page, client, directory);
        connection.frames = Some(ledger);
        connection.needs_receipt = true;
        connection.timer_session_changed = true;
        connection
    }
    /// Native-screen targeting while retaining this page's device borrow.
    pub fn capture_contacts(&mut self, id: &RequestId, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<crate::device::ContactCapture> {
        self.check_frame_owner(deadline, cancelled)?;
        self.owner.capture_contacts(id, deadline, cancelled)
    }
    pub fn input_contacts(&mut self, id: &RequestId, observation: &RequestId,
        operation: &frontcap_core::native::contacts::Operation, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<serde_json::Value> {
        self.owner.input_contacts(id, observation, operation, deadline, cancelled)
    }
    pub fn reconcile_contacts(&mut self, id: &RequestId, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<serde_json::Value> {
        self.owner.reconcile_contacts(id, deadline, cancelled)
    }
    pub fn page(&self) -> &Page {
        &self.page
    }
    /// Completed reads remain original. An interrupted read ID is never dispatched again.
    pub async fn read(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Snapshot> {
        let attempt = self.directory.join(id.as_str());
        if let Some(value) = load(&attempt.join("snapshot.json"))? {
            return Ok(value);
        }
        self.owner.check_webview_lifecycle()?;
        if self.closed || cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other(
                "Android page connection closed or read budget exhausted",
            ));
        }
        if let Err(error) = self
            .owner
            .verify_webview_process(&self.debugger, deadline, cancelled)
        {
            self.close();
            return Err(error);
        }
        std::fs::create_dir(&attempt)?;
        save_evidence(
            &attempt.join("intent.json"),
            &(id, &self.page, &self.debugger),
        )?;
        let work = read_once(&self.client, &attempt, deadline, cancelled).await;
        let work = work.and_then(|mut snapshot| {
            snapshot.connection_console = Some(Box::new(self.console.snapshot()));
            snapshot.validate(self.frames.as_ref().map(|frames| frames.document()))?;
            self.owner
                .verify_webview_process(&self.debugger, deadline, cancelled)?;
            save_evidence(&attempt.join("snapshot.json"), &snapshot)?;
            Ok(snapshot)
        });
        if let Err(error) = &work {
            let _ = save_evidence(&attempt.join("failure.json"), &error.to_string());
            self.close();
        }
        work
    }
    /// Install before the next document's scripts, then navigate to the caller's declared URL.
    /// A completed preparation does not establish content readiness or paused virtual time.
    pub async fn prepare_frames(
        &mut self,
        id: &RequestId,
        url: &str,
        random_seed: Option<u32>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Prepared> {
        if !url.starts_with("http://") && !url.starts_with("https://")
            || url.chars().any(char::is_control)
        {
            return Err(io::Error::other(
                "preparation requires an explicit HTTP(S) entry URL",
            ));
        }
        let attempt = self.directory.join(id.as_str());
        if let Some(value) = load::<Prepared>(&attempt.join("prepared.json"))? {
            if value.url != url || value.random_seed != random_seed {
                return Err(io::Error::other("preparation request URL or seed conflict"));
            }
            return Ok(value);
        }
        if self.closed || cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other(
                "Android page preparation closed or out of budget",
            ));
        }
        self.owner
            .claim_webview_control(&self.page.id, &self.directory)?;
        if self.frames.is_some() {
            return Err(io::Error::other(
                "frame producer already bound; preparation cannot replace it",
            ));
        }
        self.owner
            .verify_webview_process(&self.debugger, deadline, cancelled)?;
        std::fs::create_dir(&attempt)?;
        save_evidence(
            &attempt.join("intent.json"),
            &(id, url, &self.page, &self.debugger),
        )?;
        self.owner.invalidate_contact_capture();
        let work = prepare_once(&self.client, url, random_seed, &attempt, deadline, cancelled).await;
        let work = work.and_then(|prepared| {
            self.owner
                .verify_webview_process(&self.debugger, deadline, cancelled)?;
            save_evidence(&attempt.join("prepared.json"), &prepared)?;
            Ok(prepared)
        });
        if let Err(error) = &work {
            let _ = save_evidence(&attempt.join("failure.json"), &error.to_string());
            self.close();
        }
        work
    }
    /// Preserve a replacement document as observation without rebinding the old ledger or closing
    /// its transport. Same-document receipts retain normal reconciliation and uncertainty checks.
    pub(crate) async fn observe_document(
        &mut self, id: &RequestId, deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<Option<crate::android_frames::Receipt>> {
        self.inspect_frame_receipt(id, deadline, cancelled, true).await
    }
    pub(crate) async fn select_document(
        &mut self, previous: &Path, expected: &str, url: &str, prepared: &Prepared,
        deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<Prepared> {
        if self.frames.is_some() || prepared.attachment_sha256 != frontcap_core::canonical::sha256_bytes_hex(prepared_script(prepared.random_seed).as_bytes()) {
            return Err(io::Error::other("replacement requires an unbound connection and the original platform script"));
        }
        let selection = self.directory.join("document-selection");
        std::fs::create_dir(&selection)?;
        save_evidence(&selection.join("intent.json"), &(previous, expected, url, prepared))?;
        command_once(&self.client, &EnablePage {}, &selection.join("enable-page"), deadline, cancelled).await?;
        command_once(&self.client, &EnableRuntime {}, &selection.join("enable-runtime"), deadline, cancelled).await?;
        let observed = self.observe_document(&RequestId::new("selected-document").unwrap(), deadline, cancelled).await?
            .ok_or_else(|| io::Error::other("replacement document has no installed producer"))?;
        if observed.document != expected { return Err(io::Error::other("replacement document changed before selection")); }
        let ledger = crate::android_frames::Ledger::bind(&observed)?;
        self.owner.transfer_webview_control(&self.page.id, previous, &self.directory)?;
        save_evidence(&self.directory.join("frame-producer.json"), &ledger)?;
        self.frames = Some(ledger);
        self.needs_receipt = false;
        let snapshot = self.read(&RequestId::new("selected-state").unwrap(), deadline, cancelled).await?;
        if snapshot.href != url { return Err(io::Error::other("replacement URL changed before selection")); }
        self.control_timers(&RequestId::new("selected-pause").unwrap(), &crate::android_timers::Action::Pause, deadline, cancelled).await?;
        Ok(Prepared { url: url.into(), ..prepared.clone() })
    }
    /// Inspect the producer without advancing timers or delivering a frame. Missing gate is explicit.
    pub async fn inspect_frames(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Option<crate::android_frames::Receipt>> {
        self.inspect_frame_receipt(id, deadline, cancelled, false).await
    }
    async fn inspect_frame_receipt(
        &mut self, id: &RequestId, deadline: Instant, cancelled: &AtomicBool,
        observe_replacement: bool,
    ) -> io::Result<Option<crate::android_frames::Receipt>> {
        let attempt = self.directory.join(id.as_str());
        if let Some(receipt) = load(&attempt.join("frame-observation.json"))? {
            return Ok(receipt);
        }
        let work=async {
            self.check_frame_owner(deadline,cancelled)?;
            std::fs::create_dir(&attempt)?;
            let reply=command_once(&self.client,&FrameEvaluation{expression:"JSON.stringify(globalThis.__frontcapFrames ? globalThis.__frontcapFrames.receipt() : null)".into(),return_by_value:true},&attempt.join("read"),deadline,cancelled).await?;
            self.check_frame_owner(deadline,cancelled)?;
            let receipt:Option<crate::android_frames::Receipt>=decode_evaluation(&reply)?;
            if let Some(receipt)=&receipt {
                receipt.validate()?;
                if observe_replacement && self.frames.as_ref().is_none_or(|ledger| ledger.document()!=receipt.document) {
                    save_evidence(&attempt.join("frame-observation.json"), &Some(receipt))?;
                    return Ok(Some(receipt.clone()));
                }
                if let Some(ledger)=&mut self.frames {ledger.observe(receipt)?;}
                else if !receipt.legacy_used && receipt.last.is_none() {
                    self.owner.claim_webview_control(&self.page.id,&self.directory)?;
                    self.frames=Some(crate::android_frames::Ledger::bind(receipt)?);
                }
                save_evidence(&self.directory.join("frame-producer.json"),&self.frames)?;
                self.needs_receipt=false;
            } else if self.frames.is_some() {return Err(io::Error::other("bound frame producer disappeared"));}
            save_evidence(&attempt.join("frame-observation.json"),&receipt)?;
            Ok(receipt)
        }.await;
        if let Err(error) = &work {
            let _ = save_evidence(&attempt.join("failure.json"), &error.to_string());
            self.close();
        }
        work
    }
    /// Passive page delivery only. Reads do not advance time or imply native acknowledgement.
    pub async fn inspect_pointers(&mut self,id:&RequestId,deadline:Instant,cancelled:&AtomicBool)
        ->io::Result<crate::android_pointers::Snapshot> {
        let attempt=self.directory.join(id.as_str());
        if let Some(saved)=load(&attempt.join("pointer-observation.json"))? {return Ok(saved);}
        if attempt.exists() {return Err(io::Error::other("pointer read unresolved; original attempt retained"));}
        self.check_frame_owner(deadline,cancelled)?;
        let document=self.frames.as_ref().ok_or_else(||io::Error::other("frame document not bound"))?.document().to_owned();
        std::fs::create_dir(&attempt)?;
        let work=async {
            let reply=command_once(&self.client,&FrameEvaluation {
                expression:"JSON.stringify(globalThis.__frontcapFrames && globalThis.__frontcapPointers ? {document:__frontcapFrames.receipt().document,...__frontcapPointers.snapshot()} : null)".into(),
                return_by_value:true,
            },&attempt.join("read"),deadline,cancelled).await?;
            self.check_frame_owner(deadline,cancelled)?;
            let snapshot:crate::android_pointers::Snapshot=decode_evaluation(&reply)?;
            snapshot.validate()?;
            if snapshot.document!=document {return Err(io::Error::other("pointer document changed"));}
            save_evidence(&attempt.join("pointer-observation.json"),&snapshot)?;
            Ok(snapshot)
        }.await;
        if let Err(error)=&work {let _=save_evidence(&attempt.join("failure.json"),&error.to_string());self.close();}
        work
    }
    pub async fn dispatch_frame(
        &mut self,
        id: &RequestId,
        request: &crate::android_frames::Request,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<crate::android_frames::Receipt> {
        request.validate()?;
        self.owner.invalidate_contact_capture();
        let attempt = self.directory.join(id.as_str());
        if let Some(original) =
            load::<crate::android_frames::Request>(&attempt.join("frame-request.json"))?
        {
            if original != *request {
                return Err(io::Error::other("frame request ID conflict"));
            }
            return load(&attempt.join("frame-receipt.json"))?.ok_or_else(|| {
                io::Error::other("frame request unresolved; inspect producer without replay")
            });
        }
        self.owner.check_webview_lifecycle()?;
        self.check_frame_owner(deadline, cancelled)?;
        if self.needs_receipt {
            return Err(io::Error::other(
                "reconnected frame producer requires a fresh receipt",
            ));
        }
        let mut ledger = self
            .frames
            .clone()
            .ok_or_else(|| io::Error::other("inspect an unused frame producer before dispatch"))?;
        let timer_state =
            load::<crate::android_timers::State>(&self.directory.join("timer-state.json"))?;
        if matches!(
            timer_state,
            Some(crate::android_timers::State::Pending { .. })
        ) || (timer_state.is_some() && self.timer_session_changed)
        {
            return Err(io::Error::other(
                "timer effect unresolved; frame dispatch blocked",
            ));
        }
        ledger.admit(request)?;
        std::fs::create_dir(&attempt)?;
        save_evidence(&attempt.join("frame-request.json"), request)?;
        let work: io::Result<crate::android_frames::Receipt> = async {
            save_evidence(&self.directory.join("frame-producer.json"), &ledger)?;
            self.frames = Some(ledger);
            let expression = format!(
                "JSON.stringify(globalThis.__frontcapFrames.dispatch({}))",
                serde_json::to_string(request)?
            );
            let reply = command_once(
                &self.client,
                &FrameEvaluation {
                    expression,
                    return_by_value: true,
                },
                &attempt.join("dispatch"),
                deadline,
                cancelled,
            )
            .await?;
            self.check_frame_owner(deadline, cancelled)?;
            let receipt: crate::android_frames::Receipt = decode_evaluation(&reply)?;
            self.frames.as_mut().unwrap().observe(&receipt)?;
            save_evidence(&attempt.join("frame-receipt.json"), &receipt)?;
            save_evidence(&self.directory.join("frame-producer.json"), &self.frames)?;
            Ok(receipt)
        }
        .await;
        if let Err(error) = &work {
            let _ = save_evidence(&attempt.join("failure.json"), &error.to_string());
            self.close();
        }
        work
    }
    /// Timer-domain policy only. It does not deliver an animation frame or control other producers.
    pub async fn control_timers(
        &mut self,
        id: &RequestId,
        action: &crate::android_timers::Action,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<crate::android_timers::Completion> {
        use crate::android_timers::{Action, Completion, State};
        action.validate()?;
        self.owner.invalidate_contact_capture();
        let attempt = self.directory.join(id.as_str());
        if let Some(original) = load::<Action>(&attempt.join("timer-action.json"))? {
            if original != *action {
                return Err(io::Error::other("timer request ID conflict"));
            }
            return load(&attempt.join("timer-result.json"))?.ok_or_else(|| {
                io::Error::other("timer request unresolved; never replay a budget")
            });
        }
        self.owner.check_webview_lifecycle()?;
        self.check_frame_owner(deadline, cancelled)?;
        if self.needs_receipt {
            return Err(io::Error::other(
                "timer control requires fresh producer reconciliation",
            ));
        }
        if self.timer_session_changed && matches!(action, Action::Advance { .. }) {
            return Err(io::Error::other(
                "reconnected timer domain requires an explicit pause",
            ));
        }
        let document = self
            .frames
            .as_ref()
            .ok_or_else(|| io::Error::other("frame producer not bound"))?
            .idle_document()?
            .to_owned();
        let state = load::<State>(&self.directory.join("timer-state.json"))?;
        match &state {
            Some(State::Pending { .. }) => {
                return Err(io::Error::other("previous timer effect unresolved"));
            }
            Some(State::Paused { completion }) if completion.document != document => {
                return Err(io::Error::other("timer document changed"));
            }
            None if matches!(action, Action::Advance { .. }) => {
                return Err(io::Error::other("pause timers before granting a budget"));
            }
            _ => {}
        }
        std::fs::create_dir(&attempt)?;
        save_evidence(&attempt.join("timer-action.json"), action)?;
        let work: io::Result<Completion> = async {
            save_evidence(
                &self.directory.join("timer-state.json"),
                &State::Pending {
                    document: document.clone(),
                    request: id.clone(),
                    action: action.clone(),
                },
            )?;
            let (ticks_base, budget_expired) =
                crate::android_timers::execute(&self.client, action, &attempt, deadline, cancelled)
                    .await?;
            self.check_frame_owner(deadline, cancelled)?;
            let reply = command_once(
                &self.client,
                &FrameEvaluation {
                    expression: "JSON.stringify(globalThis.__frontcapFrames.receipt())".into(),
                    return_by_value: true,
                },
                &attempt.join("producer-after"),
                deadline,
                cancelled,
            )
            .await?;
            let receipt: crate::android_frames::Receipt = decode_evaluation(&reply)?;
            self.frames.as_mut().unwrap().observe(&receipt)?;
            let completion = Completion {
                document,
                action: action.clone(),
                ticks_base,
                budget_expired,
            };
            save_evidence(&attempt.join("timer-result.json"), &completion)?;
            save_evidence(
                &self.directory.join("timer-state.json"),
                &State::Paused {
                    completion: completion.clone(),
                },
            )?;
            self.timer_session_changed = false;
            Ok(completion)
        }
        .await;
        if let Err(error) = &work {
            let _ = save_evidence(&attempt.join("failure.json"), &error.to_string());
            self.close();
        }
        work
    }
    fn check_frame_owner(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::other("Android page connection closed"));
        }
        self.owner
            .verify_webview_process(&self.debugger, deadline, cancelled)
    }
    /// Sequential native pixels; this does not create input authority or an atomic page snapshot.
    pub fn capture(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<PathBuf> {
        self.check_frame_owner(deadline, cancelled)?;
        self.owner
            .capture_webview_png(&self.directory.join(id.as_str()), deadline, cancelled)
    }
    /// Transfer transport lifetime to the host without changing the page clock policy.
    pub(crate) fn retain_transport(&mut self) -> cdp::Client {
        self.close_on_drop = false;
        self.client.clone()
    }
    pub fn close(&mut self) {
        self.client.close();
        self.closed = true;
    }
}
impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if self.close_on_drop { self.close(); }
    }
}
async fn read_once(
    client: &cdp::Client,
    evidence: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Snapshot> {
    let call = client.call(
        None,
        &ReadPage {
            expression: READ,
            return_by_value: true,
        },
    );
    let cancel = async {
        while !cancelled.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let reply = tokio::select! {
        result=tokio::time::timeout_at(deadline.into(),call) => result
            .map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"Android page read deadline"))?.map_err(io::Error::other)?,
        _=cancel => return Err(io::Error::new(io::ErrorKind::Interrupted,"Android page read cancelled")),
    };
    save_evidence(&evidence.join("reply.json"), &reply)?;
    if reply.get("exceptionDetails").is_some() {
        return Err(io::Error::other("Android page observation threw"));
    }
    let value = reply["result"]["value"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing Android page observation"))?;
    let snapshot: Snapshot = serde_json::from_str(value)?;
    snapshot.validate(None)?;
    Ok(snapshot)
}

#[derive(Serialize)]
struct FrameEvaluation {
    expression: String,
    #[serde(rename = "returnByValue")]
    return_by_value: bool,
}
impl cdp::Command for FrameEvaluation {
    const METHOD: &'static str = "Runtime.evaluate";
    type Returns = serde_json::Value;
}
fn decode_evaluation<T: serde::de::DeserializeOwned>(reply: &serde_json::Value) -> io::Result<T> {
    if reply.get("exceptionDetails").is_some() {
        return Err(io::Error::other("frame producer evaluation threw"));
    }
    let value = reply["result"]["value"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing frame producer reply"))?;
    serde_json::from_str(value).map_err(io::Error::other)
}
fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    save_with_budget(path, value, WriteBudget::from_environment()?.as_ref())
}
fn save_with_budget<T: Serialize>(path: &Path, value: &T, budget: Option<&WriteBudget>) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(Class::Ordinary, path, value),
        None => frontcap_core::state::save_atomic(path, value),
    }
}

async fn command_once<C: cdp::Command<Returns = serde_json::Value>>(
    client: &cdp::Client,
    command: &C,
    evidence: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<serde_json::Value> {
    let budget = WriteBudget::from_environment()?;
    command_once_with_budget(client, command, evidence, deadline, cancelled, budget.as_ref()).await
}
async fn command_once_with_budget<C: cdp::Command<Returns = serde_json::Value>>(
    client: &cdp::Client, command: &C, evidence: &Path, deadline: Instant,
    cancelled: &AtomicBool, budget: Option<&WriteBudget>,
) -> io::Result<serde_json::Value> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err(io::Error::other("preparation budget exhausted"));
    }
    std::fs::create_dir(evidence)?;
    save_with_budget(&evidence.join("intent.json"), &(C::METHOD, command), budget)?;
    let cancel = async {
        while !cancelled.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let reply = tokio::select! {
        result=tokio::time::timeout_at(deadline.into(),client.call(None,command)) => result
            .map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"Android preparation deadline"))?.map_err(io::Error::other)?,
        _=cancel => return Err(io::Error::new(io::ErrorKind::Interrupted,"Android preparation cancelled")),
    };
    save_with_budget(&evidence.join("reply.json"), &reply, budget)?;
    Ok(reply)
}
async fn prepare_once(
    client: &cdp::Client,
    url: &str,
    random_seed: Option<u32>,
    attempt: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Prepared> {
    command_once(client, &EnablePage {}, &attempt.join("enable-page"), deadline, cancelled).await?;
    command_once(client, &EnableRuntime {}, &attempt.join("enable-runtime"), deadline, cancelled).await?;
    let installed = command_once(
        client,
        &AddFrameGate { source: prepared_script(random_seed) },
        &attempt.join("install"),
        deadline,
        cancelled,
    )
    .await?;
    let identifier = installed["identifier"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| io::Error::other("missing frame script identifier"))?;
    let navigated = command_once(
        client,
        &Navigate { url },
        &attempt.join("navigate"),
        deadline,
        cancelled,
    )
    .await?;
    if navigated.get("errorText").is_some() || navigated["frameId"].as_str().is_none() {
        return Err(io::Error::other("Android prepared navigation failed"));
    }
    // Same-document navigation does not execute newly registered document scripts.
    // Reload only after a definite navigation reply; a lost reply remains unresolved.
    if navigated.get("loaderId").is_none() {
        command_once(client, &Reload {}, &attempt.join("reload"), deadline, cancelled).await?;
    }
    Ok(Prepared {
        random_seed,
        script_identifier: identifier.into(),
        url: url.into(),
        attachment_sha256: frontcap_core::canonical::sha256_bytes_hex(prepared_script(random_seed).as_bytes()),
    })
}

#[cfg(test)]
#[path = "android_webview_page_tests.rs"]
mod tests;
