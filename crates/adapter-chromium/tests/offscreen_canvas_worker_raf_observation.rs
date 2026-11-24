//! External P02 observation: genuine worker rAF callbacks, with native trace boundaries.
//! Callback qualification does not qualify backing pixels, presentation, or all of P02.
mod support;

use cdp::protocol::{emulation, io, runtime, system_info, target, tracing};
use frontcap_chromium::generation::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

type ProbeResult<T> = Result<T, String>;
macro_rules! ensure {
    ($condition:expr, $($message:tt)*) => {
        if !$condition { return Err(format!($($message)*)); }
    };
}


mod trace_config {
    // Pinned Perfetto TraceConfig fields; independently decoded with the checkout's
    // existing protoc. This fixture runs on Linux, whose native primary clock is 3.
    pub fn encode(browser: u32, renderer: u32) -> Vec<u8> {
        fn varint(out: &mut Vec<u8>, mut value: u64) {
            while value >= 128 {
                out.push((value as u8 & 0x7f) | 0x80);
                value >>= 7;
            }
            out.push(value as u8);
        }
        fn number(out: &mut Vec<u8>, field: u64, value: u64) {
            varint(out, field << 3);
            varint(out, value);
        }
        fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
            varint(out, (field << 3) | 2);
            varint(out, value.len() as u64);
            out.extend_from_slice(value);
        }
        fn source(name: &str, target: u64) -> Vec<u8> {
            let mut source = Vec::new();
            bytes(&mut source, 1, name.as_bytes());
            number(&mut source, 2, target);
            let mut chrome = Vec::new();
            number(&mut chrome, 2, 0); // privacy_filtering_enabled
            number(&mut chrome, 3, 1); // convert_to_legacy_json
            bytes(&mut source, 101, &chrome);
            source
        }
        let mut config = Vec::new();
        for size in [8192, 256] {
            let mut buffer = Vec::new();
            number(&mut buffer, 1, size);
            number(&mut buffer, 4, 2); // DISCARD, as recordUntilFull
            bytes(&mut config, 1, &buffer);
        }
        let mut track = Vec::new();
        bytes(&mut track, 1, b"*");
        for category in ["frontcap.raf.debug", "__metadata"] {
            bytes(&mut track, 2, category.as_bytes());
        }
        number(&mut track, 6, 1000); // timestamp_unit_multiplier
        number(&mut track, 8, 1); // enable_thread_time_sampling
        let mut events = source("track_event", 0);
        bytes(&mut events, 113, &track);
        let mut filtered = Vec::new();
        bytes(&mut filtered, 1, &events);
        for pid in [browser, renderer] {
            bytes(&mut filtered, 2, format!("org.chromium-{pid}").as_bytes());
        }
        bytes(&mut config, 2, &filtered);
        let mut metadata = Vec::new();
        bytes(&mut metadata, 1, &source("org.chromium.trace_metadata2", 1));
        bytes(&mut config, 2, &metadata);
        let mut builtin = Vec::new();
        for field in [2, 3, 4] {
            number(&mut builtin, field, 0); // retain native service/config metadata
        }
        number(&mut builtin, 5, 3); // BUILTIN_CLOCK_MONOTONIC on Linux
        bytes(&mut config, 20, &builtin);
        let mut incremental = Vec::new();
        number(&mut incremental, 1, 500);
        bytes(&mut config, 21, &incremental);
        config
    }
}

// Included verbatim inside the external fixture; isolated controls use this same code.
mod trace_collection {
    use std::io::Write;

    pub const TRACE_BYTE_LIMIT: usize = 16 * 1024 * 1024;

    #[derive(Debug)]
    pub struct Collection {
        limit: usize,
        pub received_bytes: usize,
        pub confirmed_written_bytes: usize,
        pub chunks: usize,
        pub eof_received: bool,
        pub truncated: bool,
    }

    impl Collection {
        pub fn new(limit: usize) -> Self {
            Self {
                limit,
                received_bytes: 0,
                confirmed_written_bytes: 0,
                chunks: 0,
                eof_received: false,
                truncated: false,
            }
        }

        pub fn append(
            &mut self,
            output: &mut impl Write,
            data: &str,
            base64_encoded: bool,
            eof: bool,
        ) -> Result<(), String> {
            if self.eof_received || self.truncated {
                return Err("trace chunk after terminal collection state".into());
            }
            self.chunks += 1;
            self.received_bytes = self.received_bytes.checked_add(data.len())
                .ok_or("trace byte count overflow")?;
            self.eof_received = eof;
            if base64_encoded {
                return Err("unknown trace stream encoding".into());
            }
            let retained = data.len().min(self.limit - self.confirmed_written_bytes);
            output.write_all(&data.as_bytes()[..retained])
                .map_err(|error| format!("trace artifact write failed: {error}"))?;
            self.confirmed_written_bytes += retained;
            if retained != data.len() {
                self.truncated = true;
                return Err("trace exceeds bounded collection size".into());
            }
            if !eof && data.is_empty() {
                return Err("trace stream made no progress before EOF".into());
            }
            Ok(())
        }

        pub fn validate_completion(
            &self,
            data_loss: bool,
            json_format: bool,
            uncompressed: bool,
        ) -> Result<(), String> {
            if data_loss {
                return Err("incomplete evidence: trace data loss".into());
            }
            if !json_format {
                return Err("unknown trace format".into());
            }
            if !uncompressed {
                return Err("unknown trace compression".into());
            }
            if self.truncated || !self.eof_received {
                return Err("incomplete evidence: bounded trace lacks full EOF data".into());
            }
            Ok(())
        }
    }
}

const WORKER: &str = r#"
onmessage = event => {
  if (globalThis.__offscreenState) throw new Error('duplicate initialization');
  const context = event.data.canvas.getContext('2d', {willReadFrequently:true});
  const state = {updates:0, startedDate:Date.now(), lastDate:Date.now(), lastPerf:performance.now(), rafTime:null, armedDate:null, callbacks:[]};
  Object.defineProperty(globalThis, '__offscreenState', {value:state});
  Object.defineProperty(globalThis, '__offscreenContext', {value:context});
  const colors = ['#ff0000', '#00ff00', '#0000ff'];
  const paint = () => {
    context.fillStyle = colors[state.updates % colors.length];
    context.fillRect(0, 0, 1, 1);
    state.lastDate = Date.now();
    state.lastPerf = performance.now();
  };
  paint();
  const frame = stamp => {
    state.updates++; state.rafTime = stamp; paint();
    state.callbacks.push({stamp, date:state.lastDate, perf:state.lastPerf});
    requestAnimationFrame(frame);
  };
  // A setup-only timer avoids the initial frame at zero. All animation updates
  // after arming are produced by genuine DedicatedWorkerGlobalScope rAF.
  setTimeout(() => { state.armedDate = Date.now(); requestAnimationFrame(frame); }, 25);
};
"#;

fn save(path: &Path, value: &Value) -> ProbeResult<()> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn token(value: &str) -> ProbeResult<String> {
    let value = value.replace('-', "").to_ascii_lowercase();
    ensure!(
        value.len() == 32 && value.bytes().all(|v| v.is_ascii_hexdigit()),
        "invalid native target token"
    );
    Ok(value)
}

fn timestamp(event: &Value) -> ProbeResult<f64> {
    let time = event["ts"]
        .as_f64()
        .ok_or("trace event has no numeric timestamp")?;
    ensure!(time.is_finite() && time >= 0.0, "invalid trace timestamp");
    Ok(time)
}

fn trace_events(trace: &Value) -> ProbeResult<&Vec<Value>> {
    trace
        .get("traceEvents")
        .and_then(Value::as_array)
        .ok_or("unknown trace JSON format".into())
}

fn marker_time(events: &[Value], id: &str) -> ProbeResult<f64> {
    let found: Vec<_> = events
        .iter()
        .filter(|event| event["name"] == "clock_sync" && event["args"]["sync_id"] == id)
        .collect();
    ensure!(
        found.len() == 1,
        "missing or duplicate marker {id}: {}",
        found.len()
    );
    ensure!(found[0]["ph"] == "c", "unknown clock_sync phase for {id}");
    timestamp(found[0])
}

#[derive(Debug)]
struct Invocation {
    at: f64,
    end: f64,
    id: u64,
}

fn worker_invocations(trace: &Value, worker: &str, url: &str) -> ProbeResult<Vec<Invocation>> {
    let events = trace_events(trace)?;
    let expected = token(worker)?;
    let mut identities = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event["name"] == "TracingSessionIdForWorker")
    {
        let data = &event["args"]["data"];
        let id = data["workerId"]
            .as_str()
            .ok_or("worker mapping lacks target identity")?;
        if token(id)? == expected {
            ensure!(
                data["url"] == url,
                "worker trace URL differs from native inventory"
            );
            let pid = event["pid"]
                .as_u64()
                .ok_or("worker mapping has no process id")?;
            let tid = data["workerThreadId"]
                .as_u64()
                .ok_or("worker mapping has no thread id")?;
            ensure!(pid > 0 && tid > 0, "invalid worker process/thread identity");
            identities.insert((pid, tid));
        }
    }
    ensure!(
        identities.len() == 1,
        "ambiguous or absent worker identity: {identities:?}"
    );
    let (pid, tid) = *identities.iter().next().ok_or("missing worker identity")?;
    for event in events
        .iter()
        .filter(|event| event["name"] == "FireAnimationFrame" && event["ph"] != "E")
    {
        if event["args"]["data"].get("worker").is_some() {
            ensure!(
                event["pid"].as_u64() == Some(pid) && event["tid"].as_u64() == Some(tid),
                "unknown worker callback process/thread"
            );
        }
        if event["pid"].as_u64() == Some(pid) && event["tid"].as_u64() == Some(tid) {
            timestamp(event)?;
        }
    }
    let mut order: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["pid"].as_u64() == Some(pid)
                && event["tid"].as_u64() == Some(tid)
                && event["ts"].is_number()
        })
        .collect();
    order.sort_by(|(ai, a), (bi, b)| {
        a["ts"]
            .as_f64()
            .unwrap_or(f64::NAN)
            .total_cmp(&b["ts"].as_f64().unwrap_or(f64::NAN))
            .then(ai.cmp(bi))
    });
    let mut callbacks = Vec::new();
    let mut scopes = BTreeSet::new();
    for (index, (_, event)) in order.iter().enumerate() {
        if event["name"] != "FireAnimationFrame" {
            continue;
        }
        if event["ph"] == "E" {
            continue;
        }
        let data = &event["args"]["data"];
        let scope = data["worker"]
            .as_str()
            .ok_or("worker callback lacks worker scope identity")?;
        ensure!(
            !scope.is_empty() && data.get("frame").is_none(),
            "ambiguous page/worker callback"
        );
        scopes.insert(scope);
        let id = data["id"]
            .as_u64()
            .ok_or("callback lacks native callback id")?;
        let at = timestamp(event)?;
        let end = match event["ph"].as_str() {
            Some("X") => {
                let duration = event["dur"]
                    .as_f64()
                    .ok_or("complete callback has no duration")?;
                ensure!(
                    duration.is_finite() && duration >= 0.0,
                    "invalid callback duration"
                );
                at + duration
            }
            Some("B") => {
                let mut depth = 1_u64;
                let mut end = None;
                for (_, nested) in order.iter().skip(index + 1) {
                    match nested["ph"].as_str() {
                        Some("B") => depth += 1,
                        Some("E") => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(timestamp(nested)?);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                end.ok_or("incomplete worker callback trace duration")?
            }
            _ => return Err("unknown worker callback trace phase".into()),
        };
        ensure!(
            end.is_finite() && end >= at,
            "invalid worker callback interval"
        );
        callbacks.push(Invocation { at, end, id });
    }
    ensure!(
        scopes.len() <= 1,
        "multiple worker global scopes share the selected thread"
    );
    Ok(callbacks)
}

fn qualify_trace(trace: &Value, ledger: &Value, worker: &str, url: &str) -> ProbeResult<Value> {
    let events = trace_events(trace)?;
    let callbacks = worker_invocations(trace, worker, url)?;
    let phases = ledger.as_array().ok_or("invalid phase ledger")?;
    let mut previous_end = None;
    let mut matched = 0;
    let mut report = Vec::new();
    for phase in phases {
        let label = phase["phase"].as_str().ok_or("phase has no label")?;
        let begin = marker_time(events, &format!("{label}.begin"))?;
        let end = marker_time(events, &format!("{label}.end"))?;
        ensure!(
            begin < end && previous_end.is_none_or(|previous| previous < begin),
            "ambiguous trace marker order for {label}"
        );
        previous_end = Some(end);
        ensure!(
            callbacks
                .iter()
                .all(|c| c.at != begin && c.at != end && c.end != begin && c.end != end),
            "callback/marker timestamp tie at {label}"
        );
        let observed: Vec<_> = callbacks
            .iter()
            .filter(|c| c.at > begin && c.at < end)
            .collect();
        ensure!(
            observed.iter().all(|c| c.end < end),
            "worker callback crosses boundary {label}"
        );
        let expected = phase["expected_callbacks"]
            .as_u64()
            .ok_or("phase has no independent count")?;
        ensure!(
            observed.len() as u64 == expected,
            "native worker callback count at {label}: observed {}, expected {expected}",
            observed.len()
        );
        matched += observed.len();
        report.push(json!({"phase":label,"expected_callbacks":expected,"native_callback_ids":observed.iter().map(|c| c.id).collect::<Vec<_>>(),"trace_begin":begin,"trace_end":end}));
    }
    ensure!(
        matched == callbacks.len(),
        "native worker callback ran outside a declared phase"
    );
    ensure!(
        callbacks.iter().map(|c| c.id).collect::<Vec<_>>() == vec![1, 2, 3],
        "unexpected actual rAF invocation identity/order"
    );
    Ok(json!({"callback_count":callbacks.len(),"phases":report,"trace_time_is_virtual_time":false}))
}

async fn properties(
    client: &cdp::Client,
    session: &str,
    object: runtime::RemoteObjectId,
) -> ProbeResult<BTreeMap<String, runtime::RemoteObject>> {
    let result = client
        .call(
            Some(session),
            &runtime::GetPropertiesParams {
                own_properties: Some(true),
                generate_preview: Some(false),
                ..runtime::GetPropertiesParams::new(object)
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    ensure!(
        result.exception_details.is_none(),
        "property acquisition exception: {:?}",
        result.exception_details
    );
    Ok(result
        .result
        .into_iter()
        .filter_map(|p| {
            if p.get.is_none() && p.set.is_none() {
                p.value.map(|v| (p.name, v))
            } else {
                None
            }
        })
        .collect())
}

fn primitive(object: &runtime::RemoteObject) -> ProbeResult<Value> {
    if object.r#type == runtime::RemoteObjectType::Object
        && object.subtype == Some(runtime::RemoteObjectSubtype::Null)
    {
        Ok(Value::Null)
    } else {
        object
            .value
            .clone()
            .ok_or("missing primitive data descriptor".into())
    }
}

async fn read_state(
    client: &cdp::Client,
    worker: &str,
    object: runtime::RemoteObjectId,
) -> ProbeResult<Value> {
    let props = properties(client, worker, object).await?;
    let mut state = serde_json::Map::new();
    for key in [
        "updates",
        "startedDate",
        "lastDate",
        "lastPerf",
        "rafTime",
        "armedDate",
    ] {
        state.insert(
            key.into(),
            primitive(props.get(key).ok_or("missing state data property")?)?,
        );
    }
    let array = props
        .get("callbacks")
        .and_then(|p| p.object_id.clone())
        .ok_or("missing callback log")?;
    let entries = properties(client, worker, array).await?;
    let length = primitive(entries.get("length").ok_or("missing callback log length")?)?
        .as_u64()
        .ok_or("invalid callback log length")?;
    ensure!(length <= 32, "unbounded callback activity: {length}");
    let mut callbacks = Vec::new();
    for index in 0..length {
        let entry = entries
            .get(&index.to_string())
            .and_then(|p| p.object_id.clone())
            .ok_or("callback log has a hole")?;
        let values = properties(client, worker, entry).await?;
        let mut callback = serde_json::Map::new();
        for key in ["stamp", "date", "perf"] {
            callback.insert(
                key.into(),
                primitive(values.get(key).ok_or("missing callback timestamp")?)?,
            );
        }
        callbacks.push(Value::Object(callback));
    }
    state.insert("callbacks".into(), json!(callbacks));
    Ok(Value::Object(state))
}

fn process_producers(roster: &Value) -> ProbeResult<(u32, u32)> {
    let rows = roster["processInfo"].as_array().ok_or("missing native process roster")?;
    let one = |kind: &str| -> ProbeResult<u32> {
        let selected: Vec<_> = rows.iter().filter(|row| row["type"] == kind).collect();
        ensure!(selected.len() == 1, "expected one native {kind} process: {selected:?}");
        let pid = selected[0]["id"].as_u64().ok_or("process id is not a positive integer")?;
        ensure!(pid > 0, "process id is zero");
        u32::try_from(pid).map_err(|_| "process id exceeds the native producer range".into())
    };
    let pair = (one("browser")?, one("renderer")?);
    ensure!(pair.0 != pair.1, "browser and renderer share a process id");
    Ok(pair)
}

fn trace_producers(
    trace: &Value,
    worker: &str,
    url: &str,
    producers: (u32, u32),
    marker_ids: &[&str],
) -> ProbeResult<()> {
    let events = trace_events(trace)?;
    let worker = token(worker)?;
    let mut mappings = 0;
    let mut markers = 0;
    for event in events {
        if event["name"] == "TracingSessionIdForWorker" {
            let data = &event["args"]["data"];
            let id = data["workerId"].as_str().ok_or("worker mapping lacks target identity")?;
            if token(id)? == worker {
                ensure!(data["url"] == url && event["pid"].as_u64() == Some(u64::from(producers.1)),
                    "native worker trace mapping does not belong to the selected renderer");
                mappings += 1;
            }
        }
        if event["name"] == "clock_sync"
            && event["args"]["sync_id"].as_str().is_some_and(|id| marker_ids.contains(&id)) {
            ensure!(event["pid"].as_u64() == Some(u64::from(producers.0)),
                "clock-sync marker does not belong to the selected browser");
            markers += 1;
        }
    }
    ensure!(mappings > 0 && markers > 0, "selected producers lack worker/clock-sync evidence");
    Ok(())
}

struct Observation {
    client: cdp::Client,
    root: String,
    worker: Option<String>,
    tracing: bool,
    stream: Option<io::StreamHandle>,
    complete: cdp::Subscription<tracing::TracingCompleteEvent>,
}

impl Observation {
    async fn connect(g: &Generation) -> ProbeResult<Self> {
        let endpoint = g.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .map_err(|e| e.to_string())?;
        let root = client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
                },
            )
            .await
            .map_err(|e| e.to_string())?
            .session_id
            .0;
        let complete = client.subscribe::<tracing::TracingCompleteEvent>(None);
        Ok(Self {
            client,
            root,
            worker: None,
            tracing: false,
            stream: None,
            complete,
        })
    }

    async fn clock(&self, epoch: u64, ms: u64) -> ProbeResult<Value> {
        let clock = self
            .client
            .call(
                Some(&self.root),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let expected = (i128::from(epoch) * 1000 + i128::from(ms) * 1000).to_string();
        ensure!(
            clock.enabled && clock.unix_time_us.as_ref() == Some(&expected),
            "root native clock differs at {ms}: {clock:?}"
        );
        serde_json::to_value(clock).map_err(|e| e.to_string())
    }

    async fn mark(&self, id: String, epoch: u64, ms: u64) -> ProbeResult<Value> {
        let before = self.clock(epoch, ms).await?;
        self.client
            .call(None, &tracing::RecordClockSyncMarkerParams::new(id.clone()))
            .await
            .map_err(|e| e.to_string())?;
        let after = self.clock(epoch, ms).await?;
        ensure!(
            before == after,
            "native clock identity changed around marker {id}"
        );
        Ok(json!({"marker":id,"virtual_ms":ms,"clock":before}))
    }

    async fn process_roster(&self, output: &Path) -> ProbeResult<Value> {
        let result = self.client.call(None, &system_info::GetProcessInfoParams::default()).await;
        match result {
            Ok(roster) => {
                let roster = serde_json::to_value(roster).map_err(|e| e.to_string())?;
                save(output, &roster)?;
                Ok(roster)
            }
            Err(error) => {
                save(output, &json!({"error":error.to_string()}))?;
                Err(error.to_string())
            }
        }
    }

    async fn start_trace(
        &mut self,
        directory: &Path,
        label: &str,
        expected: Option<(u32, u32)>,
    ) -> ProbeResult<(u32, u32)> {
        ensure!(cfg!(target_os = "linux"), "producer-filter trace config is pinned to Linux");
        let roster = self.process_roster(&directory.join(format!("{label}-producers-before.json"))).await?;
        let producers = process_producers(&roster)?;
        ensure!(expected.is_none_or(|expected| expected == producers),
            "native process identities changed between measurement and inspection");
        let config = trace_config::encode(producers.0, producers.1);
        std::fs::write(directory.join(format!("{label}-trace-config.pb")), &config).map_err(|e| e.to_string())?;
        let encoded = frontcap_core::encode::base64(&config);
        save(&directory.join(format!("{label}-producer-selection.json")), &json!({
            "browser_pid":producers.0,"renderer_pid":producers.1,
            "track_event_producers":[format!("org.chromium-{}",producers.0),format!("org.chromium-{}",producers.1)],
            "metadata_producers_filtered":false,"perfetto_config_base64":encoded,
            "main_buffer_kib":8192,"metadata_buffer_kib":256,"stream_byte_limit":trace_collection::TRACE_BYTE_LIMIT,
            "primary_trace_clock":"BUILTIN_CLOCK_MONOTONIC",
            "binding":"SystemInfo process.Pid -> tracing client PID -> org.chromium-<PID>; pinned source review"
        }))?;
        let params: tracing::StartParams = serde_json::from_value(json!({
            "transferMode":"ReturnAsStream", "streamFormat":"json", "streamCompression":"none",
            "perfettoConfig":encoded
        })).map_err(|e| e.to_string())?;
        self.client.call(None, &params).await.map_err(|e| e.to_string())?;
        self.tracing = true;
        Ok(producers)
    }

    async fn check_producers(
        &self,
        directory: &Path,
        label: &str,
        expected: (u32, u32),
    ) -> ProbeResult<()> {
        let roster = self.process_roster(&directory.join(format!("{label}-producers-after.json"))).await?;
        let actual = process_producers(&roster)?;
        ensure!(actual == expected, "native producer identity changed during {label}: {expected:?} -> {actual:?}");
        Ok(())
    }

    async fn finish_trace(&mut self, path: &Path) -> ProbeResult<Value> {
        self.client
            .call(None, &tracing::EndParams::default())
            .await
            .map_err(|e| e.to_string())?;
        self.tracing = false;
        let complete = tokio::time::timeout(Duration::from_secs(15), self.complete.next())
            .await
            .map_err(|_| "trace completion timeout")?
            .map_err(|e| e.to_string())?;
        save(
            &path.with_extension("completion.json"),
            &serde_json::to_value(&complete).map_err(|e| e.to_string())?,
        )?;
        let handle = complete
            .stream
            .clone()
            .ok_or("trace completed without a stream")?;
        self.stream = Some(handle.clone());
        // Keep bounded raw bytes even when the result is incomplete. Collection
        // occurs after tracing is disabled, outside the measured phase.
        let partial = path.with_extension("partial.json");
        let report_path = path.with_extension("collection.json");
        let mut output = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut collection = trace_collection::Collection::new(trace_collection::TRACE_BYTE_LIMIT);
        let report = |collection: &trace_collection::Collection, outcome: &str,
                      read_error: Option<&str>, completion_error: Option<&str>,
                      close_error: Option<&str>| -> ProbeResult<()> {
            save(&report_path, &json!({
                "state":outcome,"trace_byte_limit":trace_collection::TRACE_BYTE_LIMIT,
                "received_bytes":collection.received_bytes,
                "confirmed_written_bytes":collection.confirmed_written_bytes,
                "retained_file_bytes":std::fs::metadata(&partial).ok().map(|m| m.len()),
                "chunks":collection.chunks,"eof_received":collection.eof_received,
                "truncated":collection.truncated,"data_loss_occurred":complete.data_loss_occurred,
                "raw_partial_path":partial,"complete_trace_path":path,"read_error":read_error,
                "completion_error":completion_error,"close_error":close_error,
                "callback_or_pixel_qualification":false
            }))
        };
        report(&collection, "collecting", None, None, None)?;
        let read_result: ProbeResult<()> = async {
            loop {
                let chunk = self.client.call(None, &io::ReadParams {
                    size: Some(65_536),
                    ..io::ReadParams::new(handle.clone())
                }).await.map_err(|e| e.to_string())?;
                let appended = collection.append(&mut output, &chunk.data,
                    chunk.base64_encoded.unwrap_or(false), chunk.eof);
                report(&collection, "collecting", appended.as_ref().err().map(String::as_str), None, None)?;
                appended?;
                if chunk.eof { break; }
            }
            Ok(())
        }.await;
        drop(output);
        let completion_result = collection.validate_completion(
            complete.data_loss_occurred,
            serde_json::to_value(complete.trace_format).map_err(|e| e.to_string())? == json!("json"),
            serde_json::to_value(complete.stream_compression).map_err(|e| e.to_string())? == json!("none"),
        );
        // Attempt closing the owned stream even after malformed/oversized input;
        // retain both the original collection error and any close error.
        let close_result = self.client.call(None, &io::CloseParams::new(handle)).await
            .map_err(|e| e.to_string());
        if close_result.is_ok() { self.stream = None; }
        report(&collection, "collected",
            read_result.as_ref().err().map(String::as_str),
            completion_result.as_ref().err().map(String::as_str),
            close_result.as_ref().err().map(String::as_str))?;
        read_result?;
        completion_result?;
        close_result?;
        let trace: Value = match std::fs::File::open(&partial).map_err(|e| e.to_string())
            .and_then(|file| serde_json::from_reader(file).map_err(|e| format!("incomplete trace JSON: {e}"))) {
            Ok(trace) => trace,
            Err(error) => {
                report(&collection, "invalid_json", Some(&error), None, None)?;
                return Err(error);
            }
        };
        report(&collection, "complete_json", None, None, None)?;
        std::fs::rename(&partial, path).map_err(|e| e.to_string())?;
        Ok(trace)
    }

    async fn inspect(&mut self, worker_id: &str, epoch: u64, output: &Path) -> ProbeResult<Value> {
        let worker = self
            .client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(target::TargetID(worker_id.into()))
                },
            )
            .await
            .map_err(|e| e.to_string())?
            .session_id
            .0;
        self.worker = Some(worker.clone());
        let global = self
            .client
            .call(
                Some(&worker),
                &runtime::EvaluateParams {
                    object_group: Some("offscreen-raf-observation".into()),
                    throw_on_side_effect: Some(true),
                    return_by_value: Some(false),
                    await_promise: Some(false),
                    silent: Some(true),
                    ..runtime::EvaluateParams::new("globalThis".into())
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        ensure!(
            global.exception_details.is_none(),
            "worker global read was refused"
        );
        let globals = properties(
            &self.client,
            &worker,
            global.result.object_id.ok_or("missing global handle")?,
        )
        .await?;
        let state = globals
            .get("__offscreenState")
            .and_then(|p| p.object_id.clone())
            .ok_or("missing state object")?;
        let context = globals
            .get("__offscreenContext")
            .and_then(|p| p.object_id.clone())
            .ok_or("missing canvas context")?;
        let clock_before = self.clock(epoch, 150).await?;
        let before = read_state(&self.client, &worker, state.clone()).await?;
        let pixel = self.client.call(Some(&worker), &runtime::CallFunctionOnParams {
            object_id: Some(context), throw_on_side_effect: Some(true), return_by_value: Some(true),
            await_promise: Some(false), silent: Some(true),
            ..runtime::CallFunctionOnParams::new("function() { const pixel = OffscreenCanvasRenderingContext2D.prototype.getImageData.call(this, 0, 0, 1, 1).data; return [pixel[0], pixel[1], pixel[2], pixel[3]]; }".into())
        }).await.map_err(|e| e.to_string())?;
        let after = read_state(&self.client, &worker, state).await?;
        let clock_after = self.clock(epoch, 150).await?;
        let result = json!({"before":before,"after":after,"clock_before":clock_before,"clock_after":clock_after,
            "pixel_guard":{"expected_refusal":true,"observed_refusal":pixel.exception_details.is_some(),
                "exception":pixel.exception_details,"value":pixel.result.value,"qualifies_presentation":false},
            "read_is_not_general_microtask_suppression":true});
        save(output, &result)?;
        ensure!(
            before == after && clock_before == clock_after,
            "observation contamination: state/native clock changed during inspection"
        );
        let expected_callbacks: Vec<_> = [50, 100, 150]
            .into_iter()
            .map(|ms| json!({"stamp":ms,"date":epoch+ms,"perf":ms}))
            .collect();
        ensure!(
            before
                == json!({"updates":3,"startedDate":epoch,"lastDate":epoch+150,"lastPerf":150,
            "rafTime":150,"armedDate":epoch+25,"callbacks":expected_callbacks}),
            "callback log/count/timestamp mismatch; possible observation contamination: {before}"
        );
        if pixel.exception_details.is_none() {
            ensure!(
                pixel.result.value == Some(json!([255, 0, 0, 255])),
                "unexpected final backing pixel"
            );
        }
        Ok(result)
    }

    async fn measure(
        &mut self,
        g: &mut Generation,
        directory: &Path,
        epoch: u64,
    ) -> ProbeResult<()> {
        let producers = self.start_trace(directory, "measurement", None).await?;
        let mut ledger = Vec::new();
        let mut now = 0;
        for (label, step, wall, expected_callbacks) in [
            ("arm", 25, 0, 0),
            ("wall25", 0, 350, 0),
            ("before50", 24, 0, 0),
            ("at50", 1, 0, 1),
            ("wall50", 0, 350, 0),
            ("before100", 49, 0, 0),
            ("at100", 1, 0, 1),
            ("at150", 50, 0, 1),
        ] {
            let begin = self.mark(format!("{label}.begin"), epoch, now).await?;
            if step > 0 {
                g.advance(VirtualDuration::from_ms(step))
                    .await
                    .map_err(|e| e.to_string())?;
                now += step;
            }
            if wall > 0 {
                tokio::time::sleep(Duration::from_millis(wall)).await;
            }
            let end = self.mark(format!("{label}.end"), epoch, now).await?;
            ledger.push(json!({"phase":label,"step_ms":step,"wall_ms":wall,"expected_callbacks":expected_callbacks,"begin":begin,"end":end}));
            save(&directory.join("phase-ledger.json"), &json!(ledger))?;
        }
        let inventory = self
            .client
            .call(
                Some(&self.root),
                &emulation::FrontcapGetProcessWorkersParams::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let root_clock = self.clock(epoch, 150).await?;
        save(
            &directory.join("native-inventory.json"),
            &json!({"inventory":inventory,"clock":root_clock}),
        )?;
        ensure!(
            inventory.renderer_id
                == root_clock["rendererId"]
                    .as_str()
                    .ok_or("root clock lacks renderer identity")?,
            "worker/root renderer mismatch"
        );
        ensure!(
            inventory.workers.len() == 1
                && inventory.workers[0].thread_started
                && !inventory.workers[0].creator_is_worker,
            "expected one native enrolled root worker"
        );
        let worker_id = inventory.workers[0].target_id.clone();
        let url = inventory.workers[0].script_url.clone();
        ensure!(
            url.starts_with("blob:https://canvas.example.test/"),
            "unexpected worker script URL: {url}"
        );
        let producer_check = self.check_producers(directory, "measurement", producers).await;
        let trace = self
            .finish_trace(&directory.join("measurement-trace.json"))
            .await?;
        producer_check?;
        let phase_markers = ledger.iter()
            .flat_map(|phase| [&phase["begin"]["marker"], &phase["end"]["marker"]])
            .map(|marker| marker.as_str().ok_or_else(|| "phase marker lacks identity".to_string()))
            .collect::<ProbeResult<Vec<_>>>()?;
        trace_producers(&trace, &worker_id, &url, producers, &phase_markers)?;
        let native = qualify_trace(&trace, &json!(ledger), &worker_id, &url)?;
        save(&directory.join("native-callback-report.json"), &native)?;

        // A second trace marks inspection separately. Its callbacks cannot be
        // silently counted as callbacks from the controlled measurement.
        self.start_trace(directory, "inspection", Some(producers)).await?;
        let begin = self.mark("inspection.begin".into(), epoch, 150).await?;
        let inspection = self
            .inspect(&worker_id, epoch, &directory.join("inspection.json"))
            .await;
        let end = self.mark("inspection.end".into(), epoch, 150).await?;
        save(
            &directory.join("inspection-ledger.json"),
            &json!({"begin":begin,"end":end}),
        )?;
        let producer_check = self.check_producers(directory, "inspection", producers).await;
        let inspection_trace = self
            .finish_trace(&directory.join("inspection-trace.json"))
            .await?;
        producer_check?;
        trace_producers(&inspection_trace, &worker_id, &url, producers, &["inspection.begin", "inspection.end"])?;
        let inspection_callbacks = worker_invocations(&inspection_trace, &worker_id, &url)?;
        let events = trace_events(&inspection_trace)?;
        ensure!(
            marker_time(events, "inspection.begin")?.partial_cmp(&marker_time(events, "inspection.end")?)
                == Some(std::cmp::Ordering::Less),
            "ambiguous inspection marker order"
        );
        save(
            &directory.join("inspection-callbacks.json"),
            &json!({"count":inspection_callbacks.len(),
            "ids":inspection_callbacks.iter().map(|c| c.id).collect::<Vec<_>>(),
            "observation_contamination":!inspection_callbacks.is_empty()}),
        )?;
        ensure!(
            inspection_callbacks.is_empty(),
            "observation contamination: worker callbacks occurred during the separate inspection period"
        );
        let inspection = inspection?;
        save(
            &directory.join("result.json"),
            &json!({"callback_time_scope_passed":true,"native":native,"inspection":inspection,
            "worker_raf_is_real":true,"p02_complete":false,"backing_freeze_qualified":false,"presentation_qualified":false}),
        )
    }

    async fn cleanup(&mut self) -> ProbeResult<()> {
        let mut errors = Vec::new();
        if self.tracing {
            if let Err(error) = self.client.call(None, &tracing::EndParams::default()).await {
                errors.push(error.to_string());
            }
            self.tracing = false;
            if let Ok(Ok(event)) =
                tokio::time::timeout(Duration::from_secs(5), self.complete.next()).await
            {
                self.stream = event.stream;
            }
        }
        if let Some(handle) = self.stream.take()
            && let Err(error) = self.client.call(None, &io::CloseParams::new(handle)).await
        {
            errors.push(error.to_string());
        }
        if let Some(worker) = self.worker.as_ref()
            && let Err(error) = self
                .client
                .call(
                    Some(worker),
                    &runtime::ReleaseObjectGroupParams::new("offscreen-raf-observation".into()),
                )
                .await
        {
            errors.push(error.to_string());
        }
        ensure!(errors.is_empty(), "inspector cleanup failed: {errors:?}");
        Ok(())
    }
}

#[tokio::test]
async fn transferred_offscreen_canvas_worker_raf_callbacks_obey_virtual_boundaries() {
    assert!(
        support::controlled_engine_or_check_baseline().await,
        "requires frozen controlled engine"
    );
    let runs = tempfile::tempdir().unwrap();
    let page = format!(
        "<!doctype html><style>body{{margin:0}}canvas{{width:64px;height:64px}}</style><canvas id=surface width=1 height=1></canvas><script>const worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));const canvas=document.querySelector('#surface').transferControlToOffscreen();worker.postMessage({{canvas}},[canvas]);</script>",
        json!(WORKER)
    );
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":"https://canvas.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]).to_string()).unwrap();
    let mut options = support::options(
        runs.path(),
        "offscreen-worker-raf",
        "https://canvas.example.test/".into(),
        rules,
    );
    options.passthrough_origins = Some(vec![]);
    options.profile.frame_interval_us = 50_000;
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    let result: ProbeResult<()> = async {
        let mut observation = Observation::connect(&generation).await?;
        let result = tokio::time::timeout(
            Duration::from_secs(90),
            observation.measure(&mut generation, runs.path(), epoch),
        )
        .await
        .map_err(|_| "worker rAF observation timeout".to_string())
        .and_then(|result| result);
        save(
            &runs.path().join("measurement-outcome.json"),
            &json!({"result":result,"p02_complete":false}),
        )?;
        let cleanup = tokio::time::timeout(Duration::from_secs(15), observation.cleanup())
            .await
            .map_err(|_| "inspector cleanup timeout".to_string())
            .and_then(|result| result);
        cleanup?;
        result
    }
    .await;
    let stopped = generation.stop().await;
    save(
        &runs.path().join("terminal-observation.json"),
        &json!({"result":result,"generation_stop":format!("{stopped:?}"),"p02_complete":false}),
    )
    .unwrap();
    if std::env::var("FRONTCAP_KEEP_WORKSPACE").is_ok_and(|value| value == "1") {
        eprintln!("FRONTCAP_OFFSCREEN_RAF_ARTIFACT {}", runs.keep().display());
    }
    assert!(stopped.is_ok(), "generation cleanup failed: {stopped:?}");
    result.expect("worker rAF callback or measurement boundary failed");
}
