//! A browser-owned routing receipt can become stale before its admission command.
#![cfg(feature = "test-hooks")]
mod support;

use cdp::protocol::{emulation, page, target};
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};

type CheckError = Box<dyn Error + Send + Sync>;

fn require(condition: bool, message: &str) -> Result<(), CheckError> {
    if condition {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}

struct AdmissionCheckpoints {
    directory: std::path::PathBuf,
    began: std::time::Instant,
    sequence: u64,
    dropped_stages: u64,
    recent: std::collections::VecDeque<Value>,
    first: Vec<Value>,
    receipts: serde_json::Map<String, Value>,
}

impl AdmissionCheckpoints {
    fn new() -> Result<Self, CheckError> {
        let directory = tempfile::Builder::new()
            .prefix("shared-admission-observation-")
            .tempdir()?
            .keep();
        eprintln!("FRONTCAP_SHARED_ADMISSION_ARTIFACT {}", directory.display());
        let mut observation = Self {
            directory,
            began: std::time::Instant::now(),
            sequence: 0,
            dropped_stages: 0,
            recent: Default::default(),
            first: Vec::new(),
            receipts: Default::default(),
        };
        observation.mark("created", None)?;
        Ok(observation)
    }

    fn mark(&mut self, stage: &str, receipt: Option<(&str, Value)>) -> Result<(), CheckError> {
        const RECEIPT_NAMES: [&str; 18] = [
            "beforeClock",
            "beforeFrame",
            "beforeInventory",
            "hook",
            "held",
            "navigation",
            "rendererClock",
            "frame",
            "route",
            "inventory",
            "replacementOutcome",
            "advanceOutcome",
            "finalClock",
            "finalInventory",
            "wallClock",
            "wallInventory",
            "detach",
            "stop",
        ];
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("checkpoint sequence exhausted"))?;
        if let Some((name, value)) = receipt {
            require(
                RECEIPT_NAMES.contains(&name),
                "unknown checkpoint receipt slot",
            )?;
            self.receipts
                .insert(name.into(), json!({"sequence":self.sequence,"value":value}));
        }
        let entry = json!({"sequence":self.sequence,"stage":stage,
            "elapsedWallMicros":self.began.elapsed().as_micros().to_string()});
        if self.first.len() < 16 {
            self.first.push(entry.clone());
        }
        if self.recent.len() == 64 {
            self.recent.pop_front();
            self.dropped_stages += 1;
        }
        self.recent.push_back(entry.clone());
        let bytes = serde_json::to_vec_pretty(&json!({
            "diagnosticOnly":true,"current":entry,"firstStages":self.first,"recentStages":self.recent,
            "droppedOldStages":self.dropped_stages,"latestReceipts":self.receipts,
            "limits":{"firstStages":16,"recentStages":64,"receiptSlots":18,"jsonBytes":262144},
            "boundary":"Host-side progress checkpoints only; no additional browser commands, no virtual-time oracle."
        }))?;
        if bytes.len() > 262_144 {
            std::fs::write(
                self.directory.join("checkpoint-error.json"),
                b"{\"error\":\"checkpoint JSON exceeded 262144-byte bound\"}\n",
            )?;
            return Err(std::io::Error::other("checkpoint JSON exceeded bounded storage").into());
        }
        let temporary = self.directory.join("checkpoints.json.writing");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(temporary, self.directory.join("checkpoints.json"))?;
        Ok(())
    }

    fn reply<T: serde::Serialize, E: std::fmt::Display>(
        &mut self,
        stage: &str,
        slot: &str,
        result: &Result<T, E>,
    ) -> Result<(), CheckError> {
        let value = match result {
            Ok(value) => json!({"ok":value}),
            Err(error) => json!({"error":error.to_string()}),
        };
        self.mark(stage, Some((slot, value)))
    }
}

// This copies existing event delivery only; it never enables or resolves a pause.
struct AdmissionEventLog {
    directory: std::path::PathBuf,
    began: std::time::Instant,
    target: String,
    clock: String,
    session: String,
    registered_from: String,
    registered_to: String,
    records: Vec<Value>,
    retained_bytes: usize,
    events_seen: u64,
    overflow: bool,
    errors: Vec<String>,
    end_drain_complete: Option<bool>,
    ended_at: Option<String>,
    subscriptions_dropped: bool,
}

fn event_fields(source: &Value, names: &[&str]) -> Value {
    Value::Object(
        names
            .iter()
            .filter_map(|name| {
                source
                    .get(name)
                    .map(|value| ((*name).into(), value.clone()))
            })
            .collect(),
    )
}

fn event_payload(method: &str, event: Value) -> Value {
    let mut selected = match method {
        "Fetch.requestPaused" => event_fields(
            &event,
            &[
                "requestId",
                "frameId",
                "resourceType",
                "networkId",
                "responseErrorReason",
                "responseStatusCode",
                "frontcapIssueTimeUs",
                "frontcapBlocksMainThread",
                "frontcapWorkerId",
            ],
        ),
        "Network.requestWillBeSent" => event_fields(
            &event,
            &[
                "requestId",
                "loaderId",
                "frameId",
                "type",
                "timestamp",
                "wallTime",
            ],
        ),
        "Network.responseReceived" => event_fields(
            &event,
            &["requestId", "loaderId", "frameId", "type", "timestamp"],
        ),
        "Network.loadingFailed" => event_fields(
            &event,
            &[
                "requestId",
                "type",
                "timestamp",
                "errorText",
                "canceled",
                "blockedReason",
            ],
        ),
        "Page.frameStartedNavigating" => {
            event_fields(&event, &["frameId", "loaderId", "url", "navigationType"])
        }
        "Page.frameNavigated" => event_fields(&event, &["type"]),
        _ => unreachable!("fixed diagnostic event subscription"),
    };
    if let Some(request) = event.get("request") {
        selected["request"] = event_fields(request, &["url", "method"]);
    }
    if let Some(response) = event.get("response") {
        selected["response"] = event_fields(
            response,
            &[
                "url",
                "status",
                "statusText",
                "fromDiskCache",
                "fromServiceWorker",
            ],
        );
    }
    if let Some(frame) = event.get("frame") {
        selected["frame"] = event_fields(
            frame,
            &[
                "id",
                "loaderId",
                "url",
                "unreachableUrl",
                "frontcapDocumentId",
            ],
        );
    }
    selected
}

impl AdmissionEventLog {
    fn error(&mut self, error: impl std::fmt::Display) {
        if self.errors.len() < 8 {
            self.errors
                .push(error.to_string().chars().take(512).collect());
        }
    }

    fn record<E: cdp::Event + serde::Serialize>(
        &mut self,
        tagged: cdp::Tagged<E>,
    ) -> Result<(), CheckError> {
        self.events_seen = self.events_seen.saturating_add(1);
        require(
            tagged.session.as_deref() == Some(self.session.as_str()),
            "navigation event session differs",
        )?;
        let payload = event_payload(E::NAME, serde_json::to_value(&tagged.event)?);
        let item = json!({"method":E::NAME,"arrival":tagged.arrival.0,"session":tagged.session.as_deref(),
            "observedWallMicros":self.began.elapsed().as_micros().to_string(),"payload":payload});
        let size = serde_json::to_vec(&item)?.len();
        if self.records.len() >= 128 || size > 49_152usize.saturating_sub(self.retained_bytes) {
            self.overflow = true;
            return Ok(());
        }
        self.retained_bytes += size;
        self.records.push(item);
        Ok(())
    }

    fn persist(&mut self) {
        let mut ordered = self.records.clone();
        ordered.sort_by_key(|record| record["arrival"].as_u64().unwrap_or(0));
        let report = json!({"diagnosticOnly":true,"target":self.target,"clockId":self.clock,"session":self.session,
            "subscriptionRegisteredFromWallMicros":self.registered_from,"subscriptionRegisteredToWallMicros":self.registered_to,
            "subscriptionEndedWallMicros":self.ended_at,"subscriptionsDropped":self.subscriptions_dropped,
            "events":ordered,"eventsSeen":self.events_seen,"retainedBytes":self.retained_bytes,
            "overflow":self.overflow,"errors":self.errors,"endDrainComplete":self.end_drain_complete,
            "transportFlushProven":false,"absenceConclusionAllowed":false,
            "limits":{"events":128,"retainedPayloadBytes":49152,"reportBytes":65536},
            "queueBoundary":"Existing CDP subscriptions use unbounded queues; only retained diagnostics are bounded. Cap detection and final drain drop these receivers and stop further fanout. No browser/session detach is performed.",
            "boundary":"Already enabled owned-session events only. Arrival orders this connection; timestamps are not assumed virtual time. An empty or incomplete event capture never proves that no request occurred."});
        let result = (|| -> Result<(), CheckError> {
            let bytes = serde_json::to_vec(&report)?;
            require(
                bytes.len() <= 65_536,
                "navigation event report exceeds its bound",
            )?;
            std::fs::write(self.directory.join("events.json.writing"), bytes)?;
            std::fs::rename(
                self.directory.join("events.json.writing"),
                self.directory.join("events.json"),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            self.error(error);
            let _ = std::fs::write(
                self.directory.join("events-error.json"),
                b"{\"error\":\"bounded navigation event artifact could not be written\"}\n",
            );
        }
    }
}

fn drain_event<E: cdp::Event + serde::Serialize>(
    feed: &mut Option<cdp::Subscription<E>>,
    log: &mut AdmissionEventLog,
) -> bool {
    let Some(subscription) = feed.as_mut() else {
        return false;
    };
    for _ in 0..129 {
        if log.overflow {
            return false;
        }
        match subscription.try_next_tagged() {
            Ok(Some(event)) => {
                if let Err(error) = log.record(event) {
                    log.error(format!("{} decode/serialization: {error}", E::NAME));
                    return false;
                }
            }
            Ok(None) => return true,
            Err(error) => {
                log.error(format!("{} subscription: {error}", E::NAME));
                return false;
            }
        }
    }
    log.overflow = true;
    false
}

struct AdmissionEvents {
    started: Option<cdp::Subscription<page::FrameStartedNavigatingEvent>>,
    navigated: Option<cdp::Subscription<page::FrameNavigatedEvent>>,
    sent: Option<cdp::Subscription<cdp::protocol::network::RequestWillBeSentEvent>>,
    response: Option<cdp::Subscription<cdp::protocol::network::ResponseReceivedEvent>>,
    failed: Option<cdp::Subscription<cdp::protocol::network::LoadingFailedEvent>>,
    paused: Option<cdp::Subscription<cdp::protocol::fetch::RequestPausedEvent>>,
    log: AdmissionEventLog,
}

impl AdmissionEvents {
    fn new(
        generation: &Generation,
        target: &str,
        clock: &str,
        began: std::time::Instant,
    ) -> Result<Self, CheckError> {
        let directory = tempfile::Builder::new()
            .prefix("shared-admission-navigation-events-")
            .tempdir()?
            .keep();
        eprintln!(
            "FRONTCAP_SHARED_ADMISSION_EVENTS_ARTIFACT {}",
            directory.display()
        );
        let registered_from = began.elapsed().as_micros().to_string();
        let (session, started) = generation.test_subscribe_shared_admission_event(target, clock)?;
        let (n_session, navigated) =
            generation.test_subscribe_shared_admission_event(target, clock)?;
        let (s_session, sent) = generation.test_subscribe_shared_admission_event(target, clock)?;
        let (r_session, response) =
            generation.test_subscribe_shared_admission_event(target, clock)?;
        let (f_session, failed) =
            generation.test_subscribe_shared_admission_event(target, clock)?;
        let (p_session, paused) =
            generation.test_subscribe_shared_admission_event(target, clock)?;
        require(
            [n_session, s_session, r_session, f_session, p_session]
                .iter()
                .all(|id| id == &session),
            "navigation event subscriptions differ in session",
        )?;
        let mut result = Self {
            started: Some(started),
            navigated: Some(navigated),
            sent: Some(sent),
            response: Some(response),
            failed: Some(failed),
            paused: Some(paused),
            log: AdmissionEventLog {
                directory,
                began,
                target: target.into(),
                clock: clock.into(),
                session,
                registered_from,
                registered_to: began.elapsed().as_micros().to_string(),
                records: Vec::new(),
                retained_bytes: 0,
                events_seen: 0,
                overflow: false,
                errors: Vec::new(),
                end_drain_complete: None,
                ended_at: None,
                subscriptions_dropped: false,
            },
        };
        result.log.persist();
        Ok(result)
    }

    fn drain(&mut self, end: bool) {
        if self.log.subscriptions_dropped {
            if end {
                self.log.end_drain_complete = Some(false);
                self.log.persist();
            }
            return;
        }
        let before = self.log.events_seen;
        let emptied = [
            drain_event(&mut self.started, &mut self.log),
            drain_event(&mut self.navigated, &mut self.log),
            drain_event(&mut self.sent, &mut self.log),
            drain_event(&mut self.response, &mut self.log),
            drain_event(&mut self.failed, &mut self.log),
            drain_event(&mut self.paused, &mut self.log),
        ];
        if end {
            self.log.end_drain_complete = Some(
                emptied.iter().all(|empty| *empty)
                    && !self.log.overflow
                    && self.log.errors.is_empty(),
            );
        }
        if end || self.log.overflow || !self.log.errors.is_empty() {
            self.started = None;
            self.navigated = None;
            self.sent = None;
            self.response = None;
            self.failed = None;
            self.paused = None;
            self.log.subscriptions_dropped = true;
            self.log.ended_at = Some(self.log.began.elapsed().as_micros().to_string());
        }
        if end || self.log.events_seen != before || self.log.overflow || !self.log.errors.is_empty()
        {
            self.log.persist();
        }
    }
}

impl Drop for AdmissionEvents {
    fn drop(&mut self) {
        self.drain(true);
    }
}

// Explicit fixture stimulus: only one observed owned Document pause may fail.
fn replacement_request(
    log: &AdmissionEventLog,
    frame: &str,
    url: &str,
    old_loader: &str,
) -> Result<Option<(cdp::protocol::fetch::RequestId, Value)>, CheckError> {
    require(
        !log.overflow && log.errors.is_empty() && !log.subscriptions_dropped,
        "request stimulus lacks an intact owned event capture",
    )?;
    let by_method = |method: &str| {
        log.records
            .iter()
            .filter(|record| {
                record["method"] == method
                    && record["session"] == log.session
                    && record["payload"]["frameId"] == frame
                    && (record["payload"]["url"] == url
                        || record["payload"]["request"]["url"] == url)
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let starts = by_method("Page.frameStartedNavigating");
    let requests = by_method("Network.requestWillBeSent");
    let pauses = by_method("Fetch.requestPaused");
    require(
        starts.len() <= 1 && requests.len() <= 1 && pauses.len() <= 1,
        "request stimulus matches ambiguous navigation events",
    )?;
    if starts.is_empty() || requests.is_empty() || pauses.is_empty() {
        return Ok(None);
    }
    let (started, sent, paused) = (&starts[0], &requests[0], &pauses[0]);
    let loader = started["payload"]["loaderId"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("started loader missing"))?;
    let request = paused["payload"]["requestId"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("paused request ID missing"))?;
    require(
        !loader.is_empty()
            && loader != old_loader
            && !request.is_empty()
            && started["payload"]["navigationType"] == "differentDocument"
            && sent["payload"]["type"] == "Document"
            && sent["payload"]["loaderId"] == loader
            && sent["payload"]["requestId"] == loader
            && paused["payload"]["networkId"] == loader
            && paused["payload"]["resourceType"] == "Document"
            && paused["payload"]
                .get("responseStatusCode")
                .is_none_or(Value::is_null)
            && paused["payload"]
                .get("responseErrorReason")
                .is_none_or(Value::is_null),
        "request stimulus lacks exact request-stage frame/loader/network identity",
    )?;
    let arrivals = [started, sent, paused].map(|event| event["arrival"].as_u64().unwrap_or(0));
    require(
        arrivals[0] > 0 && arrivals[0] < arrivals[1] && arrivals[1] < arrivals[2],
        "request stimulus event arrival order differs",
    )?;
    Ok(Some((
        cdp::protocol::fetch::RequestId(request.into()),
        json!({
            "session":log.session,"target":log.target,"clockId":log.clock,
            "frameId":frame,"url":url,"loaderId":loader,"networkId":loader,"requestId":request,
            "eventArrivals":arrivals,"resourceType":"Document","stage":"Request"
        }),
    )))
}

fn save_request_action(
    checkpoints: &mut AdmissionCheckpoints,
    saved: &mut Option<Value>,
    action: Value,
) -> Result<(), CheckError> {
    require(
        serde_json::to_vec(&action)?.len() <= 16_384,
        "request stimulus receipt exceeds bound",
    )?;
    *saved = Some(action.clone());
    checkpoints.mark(
        "request-action-checkpoint",
        Some(("navigation", json!({"requestAction":action}))),
    )
}

// A separate subscription copies the genuine isolated-world binding. It cannot
// consume the recorder's subscription or publish a report into the page.
struct NavigationBinding {
    feed: cdp::Subscription<cdp::protocol::runtime::BindingCalledEvent>,
    session: String,
    context: Value,
    seen: usize,
    bytes: usize,
    receipt: Option<Value>,
}

impl NavigationBinding {
    fn drain(&mut self, url: &str, began: std::time::Instant) -> Result<(), CheckError> {
        for _ in 0..65 {
            let Some(tagged) = self.feed.try_next_tagged()? else {
                return Ok(());
            };
            self.seen += 1;
            self.bytes = self.bytes.saturating_add(tagged.event.payload.len());
            require(
                self.seen <= 64 && self.bytes <= 32_768,
                "navigation binding observation exceeds its fixed bound",
            )?;
            require(
                tagged.session.as_deref() == Some(self.session.as_str()),
                "navigation binding belongs to another session",
            )?;
            let event = tagged.event;
            if event.name != "__frontcapReport"
                || serde_json::to_value(&event.execution_context_id)? != self.context
            {
                continue;
            }
            let report: Value = serde_json::from_str(&event.payload)?;
            if report["type"] != "navigate" {
                continue;
            }
            require(
                report["url"] == url
                    && report["t_us"] == "10000"
                    && report["rseq"].as_u64().is_some_and(|sequence| sequence > 0)
                    && tagged.arrival.0 > 0
                    && self.receipt.is_none(),
                "navigation binding URL/time/sequence or uniqueness differs",
            )?;
            self.receipt = Some(json!({"method":"Runtime.bindingCalled","name":event.name,
                "session":self.session,"executionContextId":event.execution_context_id,
                "arrival":tagged.arrival.0,"observedWallMicros":began.elapsed().as_micros().to_string(),
                "report":{"type":"navigate","url":report["url"],"t_us":report["t_us"],"rseq":report["rseq"]}}));
        }
        Err(std::io::Error::other("navigation binding drain did not reach its bounded end").into())
    }
}

#[tokio::test]
async fn a_renderer_replaced_between_routing_and_admission_is_recollected() {
    assert!(
        support::controlled_engine_or_check_baseline().await,
        "shared admission replacement requires the controlled engine"
    );
    let mut checkpoints = AdmissionCheckpoints::new().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let start = "https://admission-handoff.test/start";
    let replacement = "http://127.0.0.1:1/admission-replacement";
    let rules = RuleSet::from_json(&json!([
        {"id":"start","match":{"url":start},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><title>first document</title>"}}
    ]).to_string()).unwrap();
    let mut options =
        support::options(runs.path(), "shared-admission-handoff", start.into(), rules);
    options.passthrough_origins = Some(vec!["http://127.0.0.1:1".into()]);
    let epoch = i128::from(options.profile.initial_date_ms) * 1000;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    generation.test_enable_shared_time().await.unwrap();
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
            },
        )
        .await
        .unwrap();
    let session = Some(attached.session_id.0.as_str());
    checkpoints.mark("before-clock-dispatched", None).unwrap();
    let before_result = client
        .call(session, &emulation::FrontcapGetClockParams::default())
        .await;
    checkpoints
        .reply("before-clock-replied", "beforeClock", &before_result)
        .unwrap();
    let before = before_result.unwrap();
    checkpoints.mark("before-frame-dispatched", None).unwrap();
    let before_frame_result = client
        .call(session, &page::GetFrameTreeParams::default())
        .await;
    checkpoints
        .reply("before-frame-replied", "beforeFrame", &before_frame_result)
        .unwrap();
    let before_frame = before_frame_result.unwrap().frame_tree.frame;
    checkpoints
        .mark("before-inventory-dispatched", None)
        .unwrap();
    let before_inventory_result = client
        .call(session, &page::FrontcapGetSharedClockParams::default())
        .await;
    checkpoints
        .reply(
            "before-inventory-replied",
            "beforeInventory",
            &before_inventory_result,
        )
        .unwrap();
    let before_inventory = before_inventory_result.unwrap();
    assert_eq!(
        before.unix_time_us.as_deref(),
        Some((epoch + 10_000).to_string().as_str())
    );
    assert!(before_inventory.activated && !before_inventory.failed);
    assert!(
        before_inventory
            .participants
            .iter()
            .any(|p| p.renderer_id == before.renderer_id
                && p.alive
                && p.confirmed
                && p.armed
                && !p.retired)
    );
    let navigation_events = AdmissionEvents::new(
        &generation,
        &endpoint.target_id,
        &before_inventory.clock_id,
        checkpoints.began,
    )
    .unwrap();
    let request_owner = generation
        .test_shared_admission_request_owner(&endpoint.target_id, &before_inventory.clock_id)
        .unwrap();
    let (reached, release) = generation
        .test_pause_shared_admission(
            endpoint.target_id.clone(),
            before_inventory.clock_id.clone(),
        )
        .unwrap();

    let (read_binding, binding_readback, document) = generation
        .test_read_shared_admission_navigation(replacement.into())
        .unwrap();
    let (binding_session, binding_events) = generation
        .test_subscribe_shared_admission_event::<cdp::protocol::runtime::BindingCalledEvent>(
            &endpoint.target_id,
            &before_inventory.clock_id,
        )
        .unwrap();
    assert_eq!(binding_session, request_owner.id);
    assert_eq!(
        document["frameId"],
        serde_json::to_value(&before_frame.id).unwrap()
    );
    let navigation_binding = NavigationBinding {
        feed: binding_events,
        session: binding_session,
        context: document["observerContextId"].clone(),
        seen: 0,
        bytes: 0,
        receipt: None,
    };

    // The one-shot pauses only this generation/target/clock. One explicit main-
    // world location navigation is the fixture action. It never writes clock,
    // recorder ownership, or observer state. The existing reader consumes the
    // genuine navigate report before the single request failure permits commit.
    let navigation_clock = checkpoints.began;
    let mut navigation_receipt: Option<Value> = None;
    let mut request_action_receipt: Option<Value> = None;
    let replace = async {
        let mut navigation_events = navigation_events;
        let request_owner = request_owner;
        let mut navigation_binding = navigation_binding;
        let mut read_binding = Some(read_binding);
        let mut binding_readback = Some(binding_readback);
        let mut reader_receipt: Option<Value> = None;
        checkpoints.mark("hook-waiting", None)?;
        let reached_result = reached.await;
        checkpoints.reply("hook-reached", "hook", &reached_result)?;
        let observed = reached_result?;
        require(
            observed["target"] == endpoint.target_id,
            "hook reached another target",
        )?;
        require(
            observed["clockId"] == before_inventory.clock_id,
            "hook reached another shared clock",
        )?;
        require(
            observed["rendererId"] == before.renderer_id,
            "hook did not retain the initial routing identity",
        )?;
        require(
            observed["unixTimeUs"] == (epoch + 10_000).to_string(),
            "hook escaped the initial held instant",
        )?;
        checkpoints.mark("held-query-dispatched", None)?;
        let held_result = client
            .call(session, &page::FrontcapGetSharedClockParams::default())
            .await;
        checkpoints.reply("held-query-replied", "held", &held_result)?;
        let held = held_result?;
        require(
            held.activated
                && !held.failed
                && !held.suspension_id.is_empty()
                && held.clock_id == before_inventory.clock_id
                && held.unix_time_us == (epoch + 10_000).to_string(),
            "admission hook lacks an active publication hold",
        )?;
        checkpoints.mark("navigation-dispatched", None)?;
        // Poll the original navigation reply and native observation together.
        // No detached task outlives this control Future or its original timeout.
        let navigate = async {
            let result = request_owner
                .call(&cdp::protocol::runtime::EvaluateParams {
                    unique_context_id: Some(
                        document["mainContextUnique"]
                            .as_str()
                            .ok_or_else(|| std::io::Error::other("original main world missing"))?
                            .into(),
                    ),
                    return_by_value: Some(true),
                    ..cdp::protocol::runtime::EvaluateParams::new(format!(
                        "location.href = {}; undefined",
                        serde_json::to_string(replacement)?
                    ))
                })
                .await;
            let receipt = match &result {
                Ok(value) => json!({"ok":value}),
                Err(error) => json!({"error":error.to_string()}),
            };
            navigation_receipt = Some(json!({
                "observedWallMicros":navigation_clock.elapsed().as_micros().to_string(),
                "reply":receipt
            }));
            let result = result?;
            require(
                result.exception_details.is_none()
                    && serde_json::to_value(&result.result)?["type"] == "undefined",
                "page navigation stimulus did not complete successfully",
            )?;
            Ok::<_, CheckError>(result)
        };
        let observe_replacement = async {
            let mut request_action_attempted = false;
            let mut request_action_complete = false;
            let (after, after_frame, route, inventory) = loop {
                navigation_events.drain(false);
                navigation_binding.drain(replacement, navigation_clock)?;
                checkpoints.mark("renderer-clock-dispatched", None)?;
                let after_result = client
                    .call(session, &emulation::FrontcapGetClockParams::default())
                    .await;
                checkpoints.reply("renderer-clock-replied", "rendererClock", &after_result)?;
                let after = after_result?;
                checkpoints.mark("frame-dispatched", None)?;
                let frame_result = client
                    .call(session, &page::GetFrameTreeParams::default())
                    .await;
                checkpoints.reply("frame-replied", "frame", &frame_result)?;
                let frame = frame_result?.frame_tree.frame;
                checkpoints.mark("route-dispatched", None)?;
                let route_result = client
                    .call(
                        session,
                        &page::FrontcapGetSharedWindowRendererParams::new(held.clock_id.clone()),
                    )
                    .await;
                checkpoints.reply("route-replied", "route", &route_result)?;
                let route = route_result?;
                checkpoints.mark("inventory-dispatched", None)?;
                let inventory_result = client
                    .call(session, &page::FrontcapGetSharedClockParams::default())
                    .await;
                checkpoints.reply("inventory-replied", "inventory", &inventory_result)?;
                let inventory = inventory_result?;
                require(
                    inventory.clock_id == held.clock_id
                        && inventory.suspension_id == held.suspension_id
                        && inventory.unix_time_us == held.unix_time_us
                        && inventory.activated
                        && !inventory.failed
                        && route.unix_time_us == held.unix_time_us,
                    "request stimulus or replacement changed the held authority",
                )?;
                if !request_action_attempted
                    && let Some((request_id, identity)) = replacement_request(
                        &navigation_events.log,
                        &before_frame.id.0,
                        replacement,
                        &before_frame.loader_id.0,
                    )?
                {
                    require(
                        request_owner.id == navigation_events.log.session
                            && after.renderer_id == before.renderer_id
                            && route.renderer_id == before.renderer_id
                            && after.unix_time_us.as_deref() == Some(held.unix_time_us.as_str())
                            && frame.id == before_frame.id
                            && frame.loader_id == before_frame.loader_id,
                        "request stimulus owner or original routing identity changed",
                    )?;
                    require(
                        serde_json::to_value(&frame)?["frontcapDocumentId"]
                            == serde_json::to_value(&before_frame)?["frontcapDocumentId"],
                        "original document changed before observer read or request stimulus",
                    )?;
                    let Some(binding) = navigation_binding.receipt.as_ref() else {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    };
                    require(
                        binding["arrival"].as_u64().is_some_and(|arrival| {
                            identity["eventArrivals"][1]
                                .as_u64()
                                .is_some_and(|request| arrival < request)
                        }),
                        "genuine navigation binding did not precede its network announcement",
                    )?;
                    if reader_receipt.is_none() {
                        checkpoints.mark("observer-reader-dispatched", None)?;
                        read_binding
                            .take()
                            .ok_or_else(|| {
                                std::io::Error::other("observer read already requested")
                            })?
                            .send(())
                            .map_err(|_| std::io::Error::other("admission reader dropped"))?;
                        let receipt = binding_readback
                            .take()
                            .ok_or_else(|| {
                                std::io::Error::other("observer receipt already awaited")
                            })?
                            .await?;
                        require(
                            receipt["url"] == replacement
                                && receipt["issueTimeUs"] == 10_000
                                && receipt["nowUs"] == 10_000
                                && receipt["world"] == document["observerContextId"]
                                && receipt["worldUnique"] == document["observerContextUnique"]
                                && receipt["mainUnique"] == document["mainContextUnique"]
                                && receipt["document"] == document["document"]
                                && receipt["ownNavigation"] == false
                                && receipt["beforeMatching"] == 0
                                && receipt["afterMatching"] == 1
                                && receipt["readerCalls"] == 1
                                && receipt["target"] == endpoint.target_id
                                && receipt["clockId"] == held.clock_id
                                && receipt["rendererId"] == before.renderer_id
                                && receipt["heldUnixTimeUs"] == held.unix_time_us,
                            "normal recorder reader did not retain the genuine report under the original authority",
                        )?;
                        checkpoints.mark(
                            "observer-reader-replied",
                            Some((
                                "navigation",
                                json!({
                            "observerNavigate":binding,"readerReceipt":receipt}),
                            )),
                        )?;
                        reader_receipt = Some(receipt);
                        // The next complete native snapshot proves original renderer,
                        // document and held authority remained unchanged after this read.
                        continue;
                    }
                    let pending = request_owner
                        .call(&cdp::protocol::fetch::FrontcapPendingRequestsParams::default())
                        .await?;
                    require(
                        pending
                            .request_ids
                            .iter()
                            .filter(|id| **id == request_id)
                            .count()
                            == 1,
                        "request stimulus is not exactly one native pending pause",
                    )?;
                    let stimulus_hold = client
                        .call(session, &page::FrontcapGetSharedClockParams::default())
                        .await?;
                    require(
                        stimulus_hold.clock_id == held.clock_id
                            && stimulus_hold.suspension_id == held.suspension_id
                            && stimulus_hold.unix_time_us == held.unix_time_us
                            && stimulus_hold.activated
                            && !stimulus_hold.failed,
                        "native pending validation changed the held authority",
                    )?;
                    request_action_attempted = true;
                    let mut action = json!({"attempts":1,"method":"Fetch.failRequest","errorReason":"ConnectionRefused",
                        "identity":identity,"pendingBefore":true,"stage":"dispatch-intent",
                        "heldClockId":held.clock_id,"heldSuspensionId":held.suspension_id,"heldUnixTimeUs":held.unix_time_us,
                        "observerNavigate":binding,"readerReceipt":reader_receipt});
                    save_request_action(
                        &mut checkpoints,
                        &mut request_action_receipt,
                        action.clone(),
                    )?;
                    let failed = request_owner
                        .call(&cdp::protocol::fetch::FailRequestParams::new(
                            request_id.clone(),
                            cdp::protocol::network::ErrorReason::ConnectionRefused,
                        ))
                        .await;
                    action["commandReply"] = match &failed {
                        Ok(value) => json!({"ok":value}),
                        Err(error) => json!({"error":error.to_string()}),
                    };
                    action["stage"] = json!("command-replied");
                    save_request_action(
                        &mut checkpoints,
                        &mut request_action_receipt,
                        action.clone(),
                    )?;
                    failed?;
                    let resolved = request_owner
                        .call(&cdp::protocol::fetch::FrontcapReceiptsParams::default())
                        .await?;
                    let matching = resolved
                        .receipts
                        .iter()
                        .filter(|receipt| receipt.request_id == request_id)
                        .collect::<Vec<_>>();
                    require(
                        matching.len() == 1
                            && matching[0].outcome
                                == cdp::protocol::fetch::FrontcapReceiptOutcome::Failed,
                        "request stimulus lacks its unique native failed receipt",
                    )?;
                    action["nativeReceipt"] = serde_json::to_value(matching[0])?;
                    action["stage"] = json!("native-failure-receipted");
                    save_request_action(&mut checkpoints, &mut request_action_receipt, action)?;
                    request_action_complete = true;
                }
                let serialized_frame = serde_json::to_value(&frame)?;
                let replaced_document = frame.id == before_frame.id
                    && frame.loader_id != before_frame.loader_id
                    && !frame.loader_id.0.is_empty()
                    && (frame.url == replacement
                        || serialized_frame.get("unreachableUrl") == Some(&json!(replacement)));
                if after.renderer_id != before.renderer_id
                    && replaced_document
                    && route.renderer_id.is_empty()
                {
                    require(
                        request_action_complete,
                        "replacement lacks the single owned request failure receipt",
                    )?;
                    require(
                        serialized_frame.get("unreachableUrl") == Some(&json!(replacement)),
                        "replacement error document lacks the original unreachable URL",
                    )?;
                    break (after, frame, route, inventory);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            require(
                after.unix_time_us.as_deref() == Some((epoch + 10_000).to_string().as_str()),
                "replacement renderer did not retain the held instant",
            )?;
            require(
                route.unix_time_us == held.unix_time_us
                    && inventory.unix_time_us == held.unix_time_us
                    && inventory.clock_id == held.clock_id
                    && inventory.suspension_id == held.suspension_id
                    && inventory.activated
                    && !inventory.failed,
                "replacement changed the held authority",
            )?;
            let evidence = json!({"hook":observed,"beforeClock":before,"beforeFrame":before_frame,
                "held":held,"replacementClock":after,"replacementFrame":after_frame,
                "replacementRoute":route,"replacementInventory":inventory});
            eprintln!("FRONTCAP_SHARED_ADMISSION_NATIVE_REPLACEMENT {evidence}");
            checkpoints.mark("replacement-proven", None)?;
            checkpoints.mark("release-dispatched", None)?;
            release.send(()).map_err(|_| {
                std::io::Error::other("admission operation dropped the one-shot release")
            })?;
            checkpoints.mark("release-sent", None)?;
            Ok::<Value, CheckError>(evidence)
        };
        let (navigation, mut evidence) = tokio::try_join!(navigate, observe_replacement)?;
        require(
            navigation.exception_details.is_none()
                && serde_json::to_value(&navigation.result)?["type"] == "undefined",
            "genuine page navigation stimulus reply failed",
        )?;
        evidence["requestAction"] = request_action_receipt
            .clone()
            .ok_or_else(|| std::io::Error::other("request action receipt missing"))?;
        evidence["stimulus"] = json!({"method":"Runtime.evaluate","action":"location.href assignment in original main world",
            "reply":navigation,"observerNavigate":navigation_binding.receipt,"readerReceipt":reader_receipt});
        eprintln!("FRONTCAP_SHARED_ADMISSION_REPLACEMENT {evidence}");
        Ok::<Value, CheckError>(evidence)
    };
    let (advance, replacement_observation) = tokio::join!(
        generation.advance(VirtualDuration::from_ms(50)),
        tokio::time::timeout(Duration::from_secs(5), replace),
    );
    // Merge an already completed reply after both owned Futures have ended or
    // been dropped. The receipt retains its original host completion instant.
    let replied = navigation_receipt.is_some();
    let mut navigation_record = navigation_receipt
        .take()
        .unwrap_or_else(|| json!({"replyRecorded":false}));
    navigation_record["requestAction"] = request_action_receipt
        .take()
        .unwrap_or_else(|| json!({"attempts":0}));
    let checkpoint_navigation = checkpoints.mark(
        if replied {
            "navigation-replied"
        } else {
            "navigation-unreplied"
        },
        Some(("navigation", navigation_record)),
    );
    let replacement_status = match &replacement_observation {
        Ok(Ok(_)) => json!({"ok":true}),
        Ok(Err(error)) => json!({"error":error.to_string()}),
        Err(error) => json!({"timeout":error.to_string()}),
    };
    let checkpoint_control = checkpoints.mark(
        "replacement-finished",
        Some(("replacementOutcome", replacement_status)),
    );
    let checkpoint_advance = checkpoints.reply("advance-finished", "advanceOutcome", &advance);
    eprintln!("FRONTCAP_SHARED_ADMISSION_ADVANCE {advance:?}");
    let final_observation = tokio::time::timeout(Duration::from_secs(5), async {
        checkpoints.mark("final-clock-dispatched", None)?;
        let clock_result = client
            .call(session, &emulation::FrontcapGetClockParams::default())
            .await;
        checkpoints.reply("final-clock-replied", "finalClock", &clock_result)?;
        let clock = clock_result?;
        checkpoints.mark("final-inventory-dispatched", None)?;
        let inventory_result = client
            .call(session, &page::FrontcapGetSharedClockParams::default())
            .await;
        checkpoints.reply(
            "final-inventory-replied",
            "finalInventory",
            &inventory_result,
        )?;
        let inventory = inventory_result?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        checkpoints.mark("wall-clock-dispatched", None)?;
        let wall_clock_result = client
            .call(session, &emulation::FrontcapGetClockParams::default())
            .await;
        checkpoints.reply("wall-clock-replied", "wallClock", &wall_clock_result)?;
        let wall_clock = wall_clock_result?;
        checkpoints.mark("wall-inventory-dispatched", None)?;
        let wall_inventory_result = client
            .call(session, &page::FrontcapGetSharedClockParams::default())
            .await;
        checkpoints.reply(
            "wall-inventory-replied",
            "wallInventory",
            &wall_inventory_result,
        )?;
        let wall_inventory = wall_inventory_result?;
        Ok::<_, CheckError>((clock, inventory, wall_clock, wall_inventory))
    })
    .await;
    let now = generation.now();
    let checkpoint_detach_start = checkpoints.mark("detach-dispatched", None);
    let detached = client
        .call(
            None,
            &target::DetachFromTargetParams {
                session_id: Some(attached.session_id.clone()),
                ..Default::default()
            },
        )
        .await;
    let checkpoint_detach = checkpoints.reply("detach-replied", "detach", &detached);
    let checkpoint_stop_start = checkpoints.mark("stop-dispatched", None);
    let trace_path = generation.trace_path().to_path_buf();
    let stopped = generation.stop().await;
    let checkpoint_stop = checkpoints.reply("stop-replied", "stop", &stopped);
    // Cleanup precedes the baseline's expected failure assertion. The identical
    // fixture keeps the success oracle; baseline errors are retained, not blessed.
    for saved in [
        checkpoint_navigation,
        checkpoint_control,
        checkpoint_advance,
        checkpoint_detach_start,
        checkpoint_detach,
        checkpoint_stop_start,
        checkpoint_stop,
    ] {
        saved.expect("bounded diagnostic checkpoint must be retained");
    }
    let replacement_observation = replacement_observation
        .expect("replacement evidence timed out")
        .expect("real replacement/held/empty-route evidence is required");
    let (clock, inventory, wall_clock, wall_inventory) = final_observation
        .expect("final native observation timed out")
        .expect("final native observation failed");
    eprintln!(
        "FRONTCAP_SHARED_ADMISSION_FINAL {}",
        json!({"now":now.0,"clock":clock,
        "inventory":inventory,"wallClock":wall_clock,"wallInventory":wall_inventory,
        "stopOk":stopped.is_ok(),"detachOk":detached.is_ok()})
    );
    advance.unwrap();
    detached.unwrap();
    stopped.unwrap();
    assert_eq!(now.0, 60_000);
    assert_eq!(
        Some(clock.renderer_id.as_str()),
        replacement_observation["replacementClock"]["rendererId"].as_str()
    );
    assert_ne!(clock.renderer_id, before.renderer_id);
    assert_eq!(clock.unix_time_us, Some((epoch + 60_000).to_string()));
    assert!(
        inventory.activated && !inventory.failed && inventory.clock_id == before_inventory.clock_id
    );
    assert_eq!(inventory.unix_time_us, (epoch + 60_000).to_string());
    assert!(
        inventory
            .participants
            .iter()
            .any(|p| p.renderer_id == clock.renderer_id
                && p.alive
                && p.confirmed
                && p.armed
                && !p.retired)
    );
    assert!(
        inventory
            .participants
            .iter()
            .any(|p| p.renderer_id == before.renderer_id && p.retired),
        "native retirement must account for the replaced renderer's vote"
    );
    assert_eq!(wall_clock.renderer_id, clock.renderer_id);
    assert_eq!(wall_clock.unix_time_us, clock.unix_time_us);
    assert_eq!(wall_inventory.clock_id, inventory.clock_id);
    assert_eq!(wall_inventory.unix_time_us, inventory.unix_time_us);
    let recorded = frontcap_core::log::BehaviorLog::read(&trace_path).unwrap();
    let replacement_url = replacement_observation["replacementFrame"]["url"]
        .as_str()
        .unwrap();
    let navigation_records = recorded
        .iter()
        .filter(|event| {
            matches!(&event.kind,
        frontcap_core::event::EventKind::Navigation(navigation)
            if navigation.url == replacement_url && navigation.context.is_none())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        navigation_records.len(),
        1,
        "exactly one replacement Navigation must be recorded"
    );
    assert_eq!(
        navigation_records[0].t_us.0, 10_000,
        "replacement Navigation retains its genuine issue time"
    );
    eprintln!(
        "FRONTCAP_SHARED_ADMISSION_RECORDED_NAVIGATION {}",
        serde_json::to_string(&navigation_records[0]).unwrap()
    );
}
