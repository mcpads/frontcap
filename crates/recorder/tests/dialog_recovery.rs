#![cfg(feature = "fault-injection")]
//! A subprocess sets up the internal hold path; recovery uses the real recorder executable.
mod support;
use frontcap_chromium::generation::{Generation, LaunchOptions};
use frontcap_core::{ids::RequestId, journal::{Journal, RequestState}, process::ProcessIdentity, rules::RuleSet};
use frontcap_recorder::{control::{ControlState, DialogAnswerOrigin}, launcher, runtime::{self, RuntimeRecord, RuntimeState}, server::{self, Recorder}};
use frontcap_recorder_protocol::{client::RecorderClient, Answer, Call, DialogResponse, Op, Outcome, Reply, RequestResult};
use std::{path::PathBuf, time::Duration};
fn id(s: &str) -> RequestId { RequestId::new(s).unwrap() }

#[tokio::test]
#[ignore = "subprocess fixture; invoked by the recovery oracle"]
async fn dialog_owner() {
    let dir = PathBuf::from(std::env::var_os("FRONTCAP_DIALOG_TEST_RUNTIME").expect("subprocess runtime"));
    let spec = runtime::load_spec(&dir).unwrap().unwrap();
    let starting = RuntimeRecord {
        launch_id: spec.launch_id.clone(), state: RuntimeState::Starting, recorder: ProcessIdentity::current(),
        browser: None, endpoint: None, socket: runtime::socket_path(&dir),
    };
    runtime::save_runtime(&dir, &starting).unwrap();
    let recorded = dir.clone();
    let g = Generation::launch(LaunchOptions {
        repo_root: spec.repo_root.clone(), runs_root: spec.runs_root.clone(), run: spec.run_id.clone(),
        url: spec.url.clone(), profile: spec.profile.clone(), rules: spec.rules.clone(), passthrough_origins: None,
        watchdog: Duration::from_secs(2), browser_profile: Some(dir.join("profile")), launch_id: Some(spec.launch_id.clone()),
        on_browser: Some(Box::new(move |identity| {
            let mut record = runtime::load_runtime(&recorded).unwrap().unwrap();
            record.browser = Some(identity.clone());
            runtime::save_runtime(&recorded, &record).unwrap();
        })), proxy: None, upstream: None, cell: None,
    }).await.unwrap();
    let control = ControlState::default();
    frontcap_core::state::save_atomic(&runtime::control_path(&dir), &control).unwrap();
    let record = RuntimeRecord {
        launch_id: spec.launch_id.clone(), state: RuntimeState::Ready, recorder: ProcessIdentity::current(),
        browser: ProcessIdentity::of(g.endpoint().browser_pid).unwrap(), endpoint: Some(g.endpoint()), socket: runtime::socket_path(&dir),
    };
    let listener = tokio::net::UnixListener::bind(&record.socket).unwrap();
    let journal = Journal::open(&runtime::journal_path(&dir)).unwrap();
    let rec = Recorder::new(dir.clone(), spec, g, journal, control);
    runtime::save_runtime(&dir, &record).unwrap();
    server::serve(rec, listener).await.unwrap();
    let mut record = runtime::load_runtime(&dir).unwrap().unwrap();
    record.state = RuntimeState::Stopped;
    runtime::save_runtime(&dir, &record).unwrap();
}

struct Fixture {
    _guard: support::Runtime,
    _temp: tempfile::TempDir,
    ws: launcher::Workspace,
    spec: runtime::LaunchSpec,
    dir: PathBuf,
    child: std::process::Child,
    record: RuntimeRecord,
    client: RecorderClient,
    lease: frontcap_recorder_protocol::LeaseToken,
}

impl Fixture {
    async fn launch(fault: Option<String>) -> Self {
        Self::launch_with_rules(fault, RuleSet::default()).await
    }

    async fn launch_with_rules(fault: Option<String>, rules: RuleSet) -> Self {
        Self::launch_configured(fault, rules, false).await
    }

    async fn launch_configured(fault: Option<String>, rules: RuleSet, popups: bool) -> Self {
        Self::launch_with_faults(fault, None, rules, popups).await
    }

    async fn launch_with_faults(fault: Option<String>, interception: Option<&str>, rules: RuleSet, popups: bool) -> Self {
        let (_temp, ws) = support::workspace();
        let addr = support::serve();
        let mut spec = support::spec(&ws, "launch", &format!("http://{addr}/observe.html"), rules);
        if popups { spec.profile.popups = frontcap_core::profile::PopupPolicy::Controlled; }
        let dir = ws.runtime_dir(&spec.launch_id);
        std::fs::create_dir_all(&dir).unwrap();
        runtime::save_spec(&dir, &spec).unwrap();
        let _guard = support::Runtime(dir.clone());
        let output = std::fs::File::create(dir.join("owner.log")).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--ignored", "--exact", "dialog_owner", "--nocapture"])
            .env("FRONTCAP_DIALOG_TEST_RUNTIME", &dir).stdout(output.try_clone().unwrap()).stderr(output);
        if let Some(fault) = fault { command.env("FRONTCAP_FAULT", fault); }
        if let Some(point) = interception { command.env("FRONTCAP_INTERCEPTION_FAULT", point); }
        let mut child = command.spawn().unwrap();
        let record = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                if let Some(record) = runtime::load_runtime(&dir).unwrap() && record.state == RuntimeState::Ready { break record }
                assert!(child.try_wait().unwrap().is_none(), "owner failed: {}", std::fs::read_to_string(dir.join("owner.log")).unwrap());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let (mut client, _) = RecorderClient::connect(&record.socket).await.unwrap();
        let Reply::Attached { lease, .. } = client.call(Call::Reattach { request_id: id("attach") }).await.unwrap() else { panic!() };
        client.call(Call::Mutate { lease: Some(lease.clone()), request_id: id("policy"), op: Op::DialogRules {
            rules: serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap(),
        }}).await.unwrap();
        client.call(Call::Mutate { lease: Some(lease.clone()), request_id: id("ready"), op: Op::Advance { by_us: 1_000 } }).await.unwrap();
        Self { _guard, _temp, ws, spec, dir, child, record, client, lease }
    }

    async fn mutate(&mut self, request: &str, op: Op) -> Result<Reply, frontcap_recorder_protocol::client::ClientError> {
        self.client.call(Call::Mutate { lease: Some(self.lease.clone()), request_id: id(request), op }).await
    }

    async fn restart(&mut self) {
        self.record.recorder.kill().unwrap();
        self.child.wait().unwrap();
        assert!(self.record.browser.as_ref().unwrap().is_alive());
        launcher::recover(&self.ws, &self.spec.launch_id, &support::recorder_bin(), &[]).await.unwrap_or_else(|e| panic!("{e:?}; {}", std::fs::read_to_string(self.dir.join("recorder.log")).unwrap_or_default()));
        let attached = launcher::reattach(&self.ws, &self.spec.launch_id, id("replacement")).await.unwrap();
        self.client = attached.client;
        self.lease = attached.lease;
    }

    async fn evaluated(&mut self, expression: &str) -> serde_json::Value {
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = self.mutate(&format!("read-{}", frontcap_core::canonical::sha256_hex(expression)), Op::Evaluate { expression: expression.into() }).await.unwrap() else { panic!() };
        value
    }

    async fn stop(&mut self) {
        self.mutate("stop", Op::Stop).await.unwrap();
        self.child.wait().unwrap();
        support::wait_until_exited(&self.dir).await;
    }
}

#[tokio::test]
async fn synchronous_requests_recover_before_the_first_binding() {
    for popup in [false, true] {
        for held in [false, true] {
            for point in ["before_request_selection:https://api.example.test/prefix", "blocking_before_selection", "blocking_before_binding"] {
                eprintln!("unbound recovery: popup={popup}, held={held}, point={point}");
                let script = "setTimeout(()=>{fetch('https://api.example.test/prefix').then(r=>r.text()).then(v=>REPORT('prefix:'+v));const x=new XMLHttpRequest();x.open('GET','https://api.example.test/first',false);try{x.send();REPORT(x.responseText)}catch(e){REPORT('aborted')};const y=new XMLHttpRequest();y.open('GET','https://api.example.test/next',false);y.send();REPORT(y.responseText)},10)";
                let action = if held { serde_json::json!({"type":"hold"}) } else { serde_json::json!({"type":"mock","status":200,"body":"first","latency_ms":3}) };
                let rules = RuleSet::from_json(&serde_json::json!([
                    {"id":"popup","match":{"url":"*popup-unbound.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":format!("<!doctype html><script>{}</script>",script.replace("REPORT", "(v=>opener.postMessage(v,'*'))"))}},
                    {"id":"first","match":{"url":"https://api.example.test/first"},"action":action},
                    {"id":"order","match":{"url":"https://api.example.test/*"},"action":{"type":"sequence","steps":[{"status":200,"body":"one"},{"status":200,"body":"two"}],"after_last":"fail"}}
                ]).to_string()).unwrap();
                let mut f = Fixture::launch_with_faults(None, Some(point), rules, popup).await;
                let expression = if popup {
                    "window.answers=[];addEventListener('message',e=>answers.push(e.data));window.child=window.open('https://popup.example.test/popup-unbound.html');true".into()
                } else {
                    format!("window.answers=[];{};true", script.replace("REPORT", "answers.push.bind(answers)"))
                };
                f.mutate("install", Op::Evaluate { expression }).await.unwrap();
                assert!(f.mutate("parent", Op::Advance { by_us: 50_000 }).await.is_err());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while f.child.try_wait().unwrap().is_none() { tokio::time::sleep(Duration::from_millis(10)).await; }
                }).await.unwrap_or_else(|_| panic!("fault did not terminate recorder: {}", std::fs::read_to_string(runtime::journal_path(&f.dir)).unwrap()));
                let control: ControlState = frontcap_core::state::load(&runtime::control_path(&f.dir)).unwrap().unwrap();
                assert!(control.suspension.is_none());
                let path = f.spec.runs_root.join(f.spec.run_id.as_str()).join("interceptions.jsonl");
                let bindings = frontcap_chromium::bindings::BindingJournal::read(&path).unwrap();
                assert!(!bindings.iter().any(|entry| matches!(entry,
                    frontcap_chromium::bindings::BindingEntry::Bound(b) if b.url == "https://api.example.test/first")), "fault must precede binding");
                if point == "blocking_before_binding" {
                    assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[
                        ("FRONTCAP_INTERCEPTION_FAULT".into(), "initial_after_binding".into()),
                        ("FRONTCAP_INTERCEPTION_DELAY".into(), "/prefix@1000".into()),
                    ]).await.is_err());
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.unwrap();
                }
                launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
                    &[("FRONTCAP_INTERCEPTION_DELAY".into(), "/prefix@1000".into())]).await
                    .unwrap_or_else(|e| panic!("popup={popup}, held={held}, {point}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
                let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
                f.client = attached.client;
                f.lease = attached.lease;
                assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
                let values = f.evaluated("answers").await;
                let values = values.as_array().unwrap();
                assert_eq!(values.len(), 3, "{values:?}");
                assert!(values.contains(&serde_json::json!(if held { "aborted" } else { "first" })));
                assert!(values.contains(&serde_json::json!("two")));
                assert!(values.contains(&serde_json::json!("prefix:one")));
                let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
                assert_eq!(status.t_us.0, 51_000);
                assert!(status.suspension.is_none());
                let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
                for suffix in ["/prefix", "/first", "/next"] {
                    let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                        frontcap_core::event::EventKind::Request(r) if r.url.ends_with(suffix) => Some(&r.id), _ => None,
                    }).collect();
                    assert_eq!(requests.len(), 1);
                    assert_eq!(events.iter().filter(|e| matches!(&e.kind, frontcap_core::event::EventKind::Response(r) if &r.id == requests[0])).count(), 1);
                }
                let (first, issued) = events.iter().find_map(|e| match &e.kind {
                    frontcap_core::event::EventKind::Request(r) if r.url.ends_with("/first") => {
                        assert_eq!(r.context.is_some(), popup);
                        Some((&r.id, e.t_us.0))
                    }, _ => None,
                }).unwrap();
                let answered = events.iter().find_map(|e| match &e.kind {
                    frontcap_core::event::EventKind::Response(r) if &r.id == first => Some(e.t_us.0), _ => None,
                }).unwrap();
                assert_eq!(answered, issued + if held { 0 } else { 3_000 });
                let next = events.iter().find_map(|e| match &e.kind {
                    frontcap_core::event::EventKind::Request(r) if r.url.ends_with("/next") => Some(e.t_us.0), _ => None,
                }).unwrap();
                assert_eq!(next, answered);
                f.stop().await;
            }
        }
    }
}

#[tokio::test]
async fn response_stops_survive_initial_synchronous_request_recovery() {
    initial_synchronous_stop(frontcap_core::event::ProgressEvent::Response).await;
}

#[tokio::test]
async fn observer_stops_survive_initial_synchronous_request_recovery() {
    use frontcap_core::event::ProgressEvent;
    for event in [ProgressEvent::Focus, ProgressEvent::Navigation, ProgressEvent::Exception] {
        initial_synchronous_stop(event).await;
    }
}

async fn initial_synchronous_stop(event: frontcap_core::event::ProgressEvent) {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    let action = match event {
        ProgressEvent::Response => "",
        ProgressEvent::Focus => "const input=document.createElement('input');document.body.append(input);input.focus();",
        ProgressEvent::Navigation => "history.pushState({},'','?arrived');",
        ProgressEvent::Exception => "throw new Error('after synchronous responses');",
        _ => unreachable!(),
    };
    for fault in [None, Some("blocking_before_selection"), Some("blocking_before_binding"), Some("repeated")] {
        eprintln!("initial synchronous stop: {event:?}, fault={fault:?}");
        let rules = RuleSet::from_json(r#"[
            {"id":"sync","match":{"url":"https://api.example.test/*"},"action":{"type":"mock","status":200,"body":"sync","latency_ms":3}}
        ]"#).unwrap();
        let initial = fault.map(|p| if p == "repeated" { "blocking_before_binding" } else { p });
        let mut f = Fixture::launch_with_faults(None, initial, rules, false).await;
        f.mutate("install", Op::Evaluate { expression: ["window.answers=[];window.later=false;setTimeout(()=>{for(const url of ['first','second']){const x=new XMLHttpRequest();x.open('GET','https://api.example.test/'+url,false);x.send();answers.push(x.responseText)}", action, "},10);setTimeout(()=>later=true,30);true"].concat() }).await.unwrap();
        let op = Op::Next { event, max_us: 50_000 };
        let reply = f.mutate("parent", op.clone()).await;
        let reply = if fault.is_some() {
            assert!(reply.is_err());
            tokio::time::timeout(Duration::from_secs(5), async {
                while f.child.try_wait().unwrap().is_none() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.expect("named fault must terminate recorder");
            if fault == Some("repeated") {
                assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
                    &[("FRONTCAP_INTERCEPTION_FAULT".into(), "initial_after_binding".into())]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
            }
            launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[]).await
                .unwrap_or_else(|e| panic!("{fault:?}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
            f.client = attached.client;
            f.lease = attached.lease;
            assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
            f.mutate("parent", op.clone()).await.unwrap()
        } else { reply.unwrap() };
        if event == ProgressEvent::Navigation && fault == Some("repeated") {
            runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.kill().unwrap();
            launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[]).await.unwrap();
            let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("third-recorder")).await.unwrap();
            f.client = attached.client;
            f.lease = attached.lease;
            assert_eq!(serde_json::to_value(f.mutate("parent", op).await.unwrap()).unwrap(), serde_json::to_value(&reply).unwrap());
        }
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = reply else { panic!("{fault:?}: {reply:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: found, ref triggers } if found == event && triggers.len() == if event == ProgressEvent::Response { 2 } else { 1 }), "{fault:?}: {result:?}");
        assert_eq!((start.t_us.0, end.t_us.0), (1_000, 17_000), "{fault:?}");
        assert_eq!(f.evaluated("({answers,later})").await, serde_json::json!({"answers":["sync","sync"],"later":false}));
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = f.mutate("no-next-event", Op::Next { event, max_us: 20_000 }).await.unwrap() else { panic!() };
        assert_eq!(result, ClockOutcome::EventTimedOut { event });
        assert_eq!((start.t_us.0, end.t_us.0), (17_000, 37_000));
        assert_eq!(f.evaluated("later").await, serde_json::json!(true));
        if event == ProgressEvent::Navigation {
            let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
            assert_eq!(events.iter().filter(|e| matches!(&e.kind, frontcap_core::event::EventKind::Navigation(n) if n.url.ends_with("?arrived"))).count(), 1);
        }
        f.stop().await;
    }
}

#[tokio::test]
async fn interception_stops_survive_unbound_synchronous_prefix_recovery() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for fault in [None, Some("before_request_selection:https://api.example.test/prefix"), Some("blocking_before_binding"), Some("repeated")] {
        let rules = RuleSet::from_json(r#"[
            {"id":"sync","match":{"url":"https://api.example.test/sync"},"action":{"type":"mock","status":200,"body":"sync","latency_ms":3}},
            {"id":"order","match":{"url":"https://api.example.test/*"},"action":{"type":"sequence","steps":[{"status":200,"body":"prefix"},{"status":200,"body":"after"}],"after_last":"fail"}}
        ]"#).unwrap();
        let initial = fault.map(|p| if p == "repeated" { "before_request_selection:https://api.example.test/prefix" } else { p });
        let mut f = Fixture::launch_with_faults(None, initial, rules, false).await;
        f.mutate("install", Op::Evaluate { expression: "window.answers=[];window.later=false;setTimeout(()=>{fetch('https://api.example.test/prefix').then(r=>r.text()).then(v=>answers.push(v));const x=new XMLHttpRequest();x.open('GET','https://api.example.test/sync',false);x.send();answers.push(x.responseText);fetch('https://api.example.test/after').then(r=>r.text()).then(v=>answers.push(v))},10);setTimeout(()=>later=true,20);true".into() }).await.unwrap();
        let op = Op::Next { event: ProgressEvent::Interception, max_us: 50_000 };
        let reply = f.mutate("parent", op.clone()).await;
        let reply = if fault.is_some() {
            assert!(reply.is_err());
            tokio::time::timeout(Duration::from_secs(5), async {
                while f.child.try_wait().unwrap().is_none() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.expect("named fault must terminate recorder");
            if fault == Some("repeated") {
                assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
                    &[("FRONTCAP_INTERCEPTION_FAULT".into(), "initial_after_binding".into())]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
            }
            launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
                &[("FRONTCAP_INTERCEPTION_DELAY".into(), "/prefix@1000".into())]).await
                .unwrap_or_else(|e| panic!("{fault:?}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
            f.client = attached.client;
            f.lease = attached.lease;
            assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
            f.mutate("parent", op).await.unwrap()
        } else { reply.unwrap() };
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = reply else { panic!("{fault:?}: {reply:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Interception, ref triggers } if triggers.len() == 2), "{fault:?}: {result:?}");
        assert_eq!((start.t_us.0, end.t_us.0), (1_000, 14_000), "{fault:?}");
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.held.len(), 2, "{fault:?}");
        assert_eq!(f.evaluated("({answers,later})").await, serde_json::json!({"answers":["sync"],"later":false}));
        f.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
        assert_eq!(f.evaluated("({values:answers,done:later})").await, serde_json::json!({"values":["sync","prefix","after"],"done":true}));
        f.stop().await;
    }
}

#[tokio::test]
async fn synchronous_hold_recovers_before_suspension_is_saved() {
    for popup in [false, true] {
        for point in ["before_suspension", "after_suspension", "repeated"] {
            let script = "setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','https://api.example.test/held',false);try{x.send()}catch(e){};RESULT},10)";
            let body = format!("<!doctype html><script>{}</script>", script.replace("RESULT", "opener.postMessage('aborted','*')"));
            let rules = RuleSet::from_json(&serde_json::json!([
                {"id":"popup","match":{"url":"*popup-held.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
                {"id":"sync","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}
            ]).to_string()).unwrap();
            let fault = if point == "repeated" { "before_suspension" } else { point };
            let mut f = Fixture::launch_configured(Some(format!("parent@{fault}")), rules, popup).await;
            let expression = if popup {
                "window.answers=[];addEventListener('message',e=>answers.push(e.data));window.child=window.open('https://popup.example.test/popup-held.html');true".into()
            } else {
                format!("window.answers=[];{};true", script.replace("RESULT", "answers.push('aborted')"))
            };
            f.mutate("install", Op::Evaluate { expression }).await.unwrap();
            f.mutate("sibling", Op::Evaluate { expression: "fetch('https://api.example.test/sibling').catch(()=>{});true".into() }).await.unwrap();
            assert!(f.mutate("parent", Op::Advance { by_us: 50_000 }).await.is_err());
            f.child.wait().unwrap();
            let control: ControlState = frontcap_core::state::load(&runtime::control_path(&f.dir)).unwrap().unwrap();
            assert_eq!(control.suspension.is_some(), point == "after_suspension");
            assert_eq!(control.inflight.as_ref().unwrap().request_id, id("parent"));
            assert!(matches!(Journal::open(&runtime::journal_path(&f.dir)).unwrap().state(&id("parent")), Some(RequestState::Dispatched)));
            let delay = ("FRONTCAP_INTERCEPTION_DELAY".into(), "/held@1000".into());
            if point == "repeated" {
                assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
                    &[delay.clone(), ("FRONTCAP_FAULT".into(), "parent@after_recovered_suspension".into())]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
            }
            launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[delay]).await
                .unwrap_or_else(|e| panic!("popup={popup}, {point}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
            f.client = attached.client;
            f.lease = attached.lease;
            assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
            assert_eq!(f.evaluated("answers").await, serde_json::json!(["aborted"]));
            let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
            assert_eq!(status.t_us.0, 51_000);
            assert!(status.suspension.is_none());
            assert_eq!(status.held.len(), 1, "the asynchronous sibling remains held");
            let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
            let (ordinal, issued) = events.iter().find_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Request(r) if r.url.ends_with("/held") => Some((&r.id, e.t_us)), _ => None,
            }).unwrap();
            let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Response(r) if &r.id == ordinal => Some((e.t_us, r.source)), _ => None,
            }).collect();
            assert_eq!(responses, vec![(issued, frontcap_core::event::ResponseSource::Fail)]);
            let journal = Journal::open(&runtime::journal_path(&f.dir)).unwrap();
            assert_eq!(journal.dependents(&id("parent")).len(), 1);
            assert!(matches!(journal.state(&id("parent")), Some(RequestState::Completed(_))));
            f.stop().await;
        }
    }
}

#[tokio::test]
async fn synchronous_request_cleanup_survives_repeated_recorder_loss() {
    for point in ["before_dispatch", "after_dispatch", "after_effect", "after_result", "after_complete"] {
        let rules = RuleSet::from_json(r#"[{"id":"sync","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
        let mut f = Fixture::launch_with_rules(None, rules).await;
        f.mutate("install", Op::Evaluate { expression: "window.sibling='pending';fetch('https://api.example.test/sibling').catch(()=>sibling='aborted');window.answers=[];setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','https://api.example.test/held',false);try{x.send();answers.push('answered');}catch(e){answers.push('aborted');}},10);true".into() }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap() else { panic!("expected a suspended synchronous request") };
        let request = operation.request.unwrap();
        let cleanup = id(&format!("mcp-held-cleanup-{}", frontcap_core::canonical::sha256_hex(&format!("parent:{}", request.ordinal))));
        f.record.recorder.kill().unwrap();
        f.child.wait().unwrap();
        let failed = launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(),
            &[("FRONTCAP_FAULT".into(), format!("{cleanup}@{point}"))]).await;
        assert!(failed.is_err(), "{point}: fault did not interrupt recovery");
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(f.record.browser.as_ref().unwrap().is_alive());
        launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[]).await
            .unwrap_or_else(|e| panic!("{point}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
        let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
        f.client = attached.client;
        f.lease = attached.lease;
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
        assert_eq!(f.evaluated("answers").await, serde_json::json!(["aborted"]), "{point}");
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.t_us.0, 51_000, "{point}: preserve the parent's absolute grant");
        assert!(status.suspension.is_none());
        assert_eq!(status.held.len(), 1, "{point}: retain the unrelated asynchronous hold");
        f.mutate("sibling", Op::FailHeld { ordinal: status.held[0].clone(), reason: frontcap_core::rules::FailReason::Aborted }).await.unwrap();
        f.mutate("settle", Op::Advance { by_us: 1_000 }).await.unwrap();
        assert_eq!(f.evaluated("sibling").await, "aborted");
        let journal = Journal::open(&runtime::journal_path(&f.dir)).unwrap();
        assert_eq!(journal.dependents(&id("parent")), std::slice::from_ref(&cleanup), "one journaled cleanup");
        assert!(matches!(journal.state(&cleanup), Some(RequestState::Completed(_))));
        let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
        let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
            frontcap_core::event::EventKind::Response(r) if r.id == request.ordinal => Some((e.t_us, r.source)), _ => None,
        }).collect();
        assert_eq!(responses, vec![(request.t_us, frontcap_core::event::ResponseSource::Fail)], "{point}: one native failure");
        f.stop().await;
    }
}

#[tokio::test]
async fn synchronous_request_answer_is_reconciled_after_recorder_loss() {
    for point in ["before_dispatch", "after_dispatch", "after_effect"] {
        let rules = RuleSet::from_json(r#"[{"id":"sync","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}}]"#).unwrap();
        let mut f = Fixture::launch_with_rules(Some(format!("answer@{point}")), rules).await;
        f.mutate("install", Op::Evaluate { expression: "window.answers=[];setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','https://api.example.test/held',false);try{x.send();answers.push(x.responseText);}catch(e){answers.push('aborted');}},10);true".into() }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap() else { panic!() };
        let request = operation.request.unwrap();
        let response = frontcap_core::rules::MockSpec { status: 200,
            headers: [("access-control-allow-origin".into(), "*".into())].into(),
            body: "answered".into(), body_base64: None, latency_ms: 0 };
        assert!(f.mutate("answer", Op::Respond { ordinal: request.ordinal.clone(), response }).await.is_err());
        f.child.wait().unwrap();
        launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[("FRONTCAP_INTERCEPTION_DELAY".into(), "/held@1000".into())]).await
            .unwrap_or_else(|e| panic!("{point}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
        let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
        f.client = attached.client;
        f.lease = attached.lease;
        let applied = point == "after_effect";
        assert_eq!(f.evaluated("answers").await, serde_json::json!([if applied { "answered" } else { "aborted" }]), "{point}");
        let journal = Journal::open(&runtime::journal_path(&f.dir)).unwrap();
        let Some(RequestState::Completed(value)) = journal.state(&id("answer")) else { panic!("{point}: unresolved answer") };
        let answer: Answer = serde_json::from_value(value.clone()).unwrap();
        assert!(match answer {
            Answer::Ok(Reply::Done { outcome: Outcome::Ack, .. }) => applied,
            Answer::Ok(Reply::Done { outcome: Outcome::NotApplied, .. }) => !applied,
            _ => false,
        }, "{point}: {answer:?}");
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
        let Some(RequestState::Completed(parent)) = journal.state(&id("parent")) else { panic!("{point}: unresolved parent") };
        assert!(matches!(serde_json::from_value::<Answer>(parent.clone()).unwrap(),
            Answer::Ok(Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. }) if t_us.0 == 51_000));
        assert_eq!(journal.dependents(&id("parent")).len(), if applied { 1 } else { 2 });
        let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
        let responses: Vec<_> = events.iter().filter(|e| matches!(&e.kind,
            frontcap_core::event::EventKind::Response(r) if r.id == request.ordinal)).collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].t_us, request.t_us, "{point}: keep the answer instant");
        f.stop().await;
    }
}

#[tokio::test]
async fn synchronous_child_request_recovers_without_repeating_the_frame() {
    for cross_origin in [false, true] {
        let rules = RuleSet::from_json(&serde_json::json!([
            {"id":"sync","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}},
            {"id":"child","match":{"url":"https://child.example.test/frame"},"action":{"type":"mock","status":200,
                "headers":{"content-type":"text/html"},"body":include_str!("fixtures/synchronous-child.html")}}
        ]).to_string()).unwrap();
        let mut f = Fixture::launch_with_rules(None, rules).await;
        let url = if cross_origin {
            "https://child.example.test/frame".into()
        } else { f.spec.url.replace("observe.html", "synchronous-child.html") };
        f.mutate("install", Op::Evaluate { expression: format!(
            "window.answers=[];addEventListener('message',e=>{{if(e.data.source==='synchronous-child')answers.push([e.origin,e.data.outcome]);}});const frame=document.createElement('iframe');frame.src={};document.body.append(frame);true",
            serde_json::to_string(&url).unwrap()) }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap() else { panic!("expected child suspension, cross-origin={cross_origin}") };
        let request = operation.request.unwrap();
        f.restart().await;
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
        let origin = url.rsplit_once('/').unwrap().0;
        assert_eq!(f.evaluated("answers").await, serde_json::json!([[origin, "aborted"]]));
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.t_us.0, 51_000);
        assert!(status.suspension.is_none());
        assert_eq!(f.evaluated("document.querySelectorAll('iframe').length").await, 1);
        let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
        let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
            frontcap_core::event::EventKind::Response(r) if r.id == request.ordinal => Some((e.t_us, r.source)), _ => None,
        }).collect();
        assert_eq!(responses, vec![(request.t_us, frontcap_core::event::ResponseSource::Fail)]);
        f.stop().await;
    }
}

#[tokio::test]
async fn consecutive_synchronous_holds_recover_across_admission_and_answer_loss() {
    for point in [None, Some("after_next_blocking_binding"), Some("after_next_blocking_hold"), Some("after_effect"), Some("agent_after_effect")] {
        let rules = RuleSet::from_json(r#"[{"id":"sync","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
        let mut f = Fixture::launch_with_rules((point == Some("agent_after_effect")).then(|| "answer@after_effect".into()), rules).await;
        f.mutate("install", Op::Evaluate { expression: "window.answers=[];setTimeout(()=>{for(let i=0;i<3;i++){const x=new XMLHttpRequest();x.open('POST','https://api.example.test/held/'+i,false);try{x.send('body-'+i);answers.push('answered');}catch(e){answers.push('aborted');}}},10);true".into() }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap() else { panic!() };
        let first = operation.request.unwrap();
        let agent_answered = point == Some("agent_after_effect");
        if agent_answered {
            assert!(f.mutate("answer", Op::Respond { ordinal: first.ordinal.clone(),
                response: frontcap_core::rules::MockSpec { status: 200, headers: Default::default(), body: "ok".into(), body_base64: None, latency_ms: 0 } }).await.is_err());
        } else { f.record.recorder.kill().unwrap(); }
        f.child.wait().unwrap();
        if let Some(point) = point.filter(|_| !agent_answered) {
            let fault = if point == "after_effect" {
                let second = format!("req-{}", first.ordinal.trim_start_matches("req-").parse::<u64>().unwrap() + 1);
                format!("mcp-held-cleanup-{}@after_effect", frontcap_core::canonical::sha256_hex(&format!("parent:{second}")))
            } else { format!("parent@{point}") };
            assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[("FRONTCAP_FAULT".into(), fault)]).await.is_err());
            tokio::time::timeout(Duration::from_secs(5), async {
                while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();
        }
        launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[]).await
            .unwrap_or_else(|e| panic!("{point:?}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
        let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
        f.client = attached.client;
        f.lease = attached.lease;
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
        assert_eq!(f.evaluated("answers").await, serde_json::json!([if agent_answered { "answered" } else { "aborted" },"aborted","aborted"]), "{point:?}");
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.t_us.0, 51_000);
        assert!(status.suspension.is_none());
        let journal = Journal::open(&runtime::journal_path(&f.dir)).unwrap();
        assert_eq!(journal.dependents(&id("parent")).len(), 3);
        assert!(journal.dependents(&id("parent")).iter().all(|id| matches!(journal.state(id), Some(RequestState::Completed(_)))));
        let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
        let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
            frontcap_core::event::EventKind::Request(r) if r.url.starts_with("https://api.example.test/held/") => Some((e.t_us,r)), _ => None,
        }).collect();
        assert_eq!(requests.len(), 3);
        for (index, (at, request)) in requests.iter().enumerate() {
            assert!(request.url.ends_with(&format!("/{index}")));
            assert_eq!(*at, first.t_us);
            assert_eq!(request.body.as_ref().and_then(|b| b.text.as_deref()), Some(format!("body-{index}").as_str()));
            let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Response(r) if r.id == request.id => Some((e.t_us,r.source)), _ => None,
            }).collect();
            let source = if agent_answered && index == 0 { frontcap_core::event::ResponseSource::Agent } else { frontcap_core::event::ResponseSource::Fail };
            assert_eq!(responses, vec![(first.t_us, source)], "{point:?}");
        }
        f.stop().await;
    }
}

#[tokio::test]
async fn consecutive_mixed_rules_recover_decisions_and_response_instants() {
    for (replay, fault) in [false, true].into_iter().flat_map(|replay|
        [None, Some("blocking_after_binding"), Some("blocking_after_dispatch"), Some("blocking_after_effect"), Some("tail_after_effect")].map(|fault| (replay, fault))) {
        let cassette = tempfile::NamedTempFile::new().unwrap();
        let mut rules_json = String::from(r#"[
            {"id":"mock","match":{"url":"https://api.example.test/mock"},"action":{"type":"mock","status":200,"body":"mock","latency_ms":2}},
            {"id":"sequence","match":{"url":"https://api.example.test/sequence"},"action":{"type":"sequence","steps":[{"status":200,"body":"first","latency_ms":3},{"status":200,"body":"second","latency_ms":5}],"after_last":"fail"}},
            {"id":"hold","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}
        ]"#);
        if replay {
            use frontcap_core::cassette::{Key, Latency, Line, Response};
            let text: String = [("first",3),("second",5)].into_iter().map(|(body, virtual_ms)| {
                Line { key: Key::of("GET", "https://api.example.test/sequence", None),
                    response: Response { status: 200, headers: [("access-control-allow-origin".into(), "*".into())].into(),
                        body_base64: frontcap_core::encode::base64(body.as_bytes()) },
                    latency: Latency { virtual_ms, wall_ms: 0 }, requires_progress: false }.to_line()+"\n"
            }).collect();
            std::fs::write(cassette.path(), text).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&rules_json).unwrap();
            value[1]["action"] = serde_json::json!({"type":"replay","cassette":cassette.path(),"after_last":"fail"});
            rules_json = value.to_string();
        }
        let mut f = Fixture::launch_with_rules(None, RuleSet::from_json(&rules_json).unwrap()).await;
        drop(cassette); // Recovery must use the frozen bundle input, not the original path.
        let automatic_tail = fault.is_none() || fault == Some("tail_after_effect");
        let script = "window.answers=[];setTimeout(()=>{for(const path of ['start','mock','sequence','sequence','sequence','end']){const x=new XMLHttpRequest();x.open('GET','https://api.example.test/'+path,false);try{x.send();answers.push(x.responseText);}catch(e){answers.push('aborted');}}},10);true";
        f.mutate("install", Op::Evaluate { expression: if automatic_tail { script.replace("'end'", "'mock'") } else { script.into() } }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap() else { panic!() };
        let selected = operation.request.unwrap().ordinal.trim_start_matches("req-").parse::<u64>().unwrap() + if fault == Some("tail_after_effect") { 5 } else { 2 };
        f.record.recorder.kill().unwrap();
        f.child.wait().unwrap();
        if let Some(fault) = fault {
            let fault = if fault == "tail_after_effect" { "blocking_after_effect" } else { fault };
            assert!(launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[("FRONTCAP_INTERCEPTION_FAULT".into(), format!("{fault}:req-{selected}"))]).await.is_err());
            tokio::time::timeout(Duration::from_secs(5), async {
                while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();
        }
        launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[]).await
            .unwrap_or_else(|e| panic!("{fault:?}: {e}; {}", std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
        let attached = launcher::reattach(&f.ws, &f.spec.launch_id, id("replacement")).await.unwrap();
        f.client = attached.client;
        f.lease = attached.lease;
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser, f.record.browser);
        assert_eq!(f.evaluated("answers").await, serde_json::json!(["aborted","mock","first","second","aborted",if automatic_tail { "mock" } else { "aborted" }]), "{fault:?}");
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.t_us.0, 51_000);
        assert!(status.suspension.is_none());
        let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
        let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
            frontcap_core::event::EventKind::Request(r) if r.url.starts_with("https://api.example.test/") => Some((e.t_us.0,r)), _ => None,
        }).collect();
        assert_eq!(requests.len(), 6);
        for ((issued, request), (expected_issue, expected_answer, status)) in requests.iter().zip([
            (11_000,11_000,0),(11_000,13_000,200),(13_000,16_000,200),(16_000,21_000,200),(21_000,21_000,0),if automatic_tail { (21_000,23_000,200) } else { (21_000,21_000,0) },
        ]) {
            assert_eq!(*issued, expected_issue);
            let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Response(r) if r.id == request.id => Some((e.t_us.0,r.status)), _ => None,
            }).collect();
            assert_eq!(responses, vec![(expected_answer,status)], "{fault:?}: {}",request.url);
        }
        for (index, (_, request)) in requests.iter().enumerate().filter(|(i, _)| (2..4).contains(i)) {
            assert_eq!(request.entry, replay.then_some((index - 2) as u64));
            let expected = if replay { frontcap_core::event::ResponseSource::Replay } else { frontcap_core::event::ResponseSource::Mock };
            assert!(events.iter().any(|e| matches!(&e.kind, frontcap_core::event::EventKind::Response(r) if r.id == request.id && r.source == expected)));
        }
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, frontcap_core::event::EventKind::Taint(t)
            if t.source == frontcap_core::event::TaintSource::CassetteMiss)).count(), usize::from(replay));
        f.stop().await;
    }
}

#[tokio::test]
async fn consecutive_external_requests_recover_once_through_the_proxy() {
    for (recording, fault) in [false, true].into_iter().flat_map(|recording| {
        [None, Some("blocking_after_binding"), Some("blocking_after_dispatch"), Some("blocking_after_completed"),
            Some("blocking_before_cassette"), Some("blocking_after_capture"), Some("blocking_after_response"), Some("after_rename")]
            .into_iter().filter(move |fault| recording || !matches!(*fault, Some("blocking_before_cassette" | "blocking_after_capture" | "blocking_after_response" | "after_rename")))
            .map(move |fault| (recording,fault))
    }) {
        external_recovery_case(recording, fault).await;
    }
}

#[tokio::test]
async fn initial_external_requests_recover_without_reissuing_or_losing_recordings() {
    for recording in [false, true] {
        for (initial, fault) in [
            ("blocking_before_selection", None),
            ("blocking_before_binding", None),
            ("blocking_before_binding", Some("blocking_after_dispatch")),
            ("blocking_before_binding", Some("blocking_after_effect")),
            ("blocking_before_binding", Some("blocking_after_completed")),
            ("blocking_before_binding", Some("blocking_before_capture")),
            ("blocking_before_binding", Some("blocking_after_capture")),
            ("blocking_before_binding", Some("blocking_after_response")),
            ("blocking_before_binding", Some("after_rename")),
        ] {
            if !recording && matches!(fault, Some("blocking_before_capture" | "blocking_after_capture" | "blocking_after_response" | "after_rename")) { continue; }
            eprintln!("initial external: recording={recording}, initial={initial}, recovery={fault:?}");
            external_recovery(recording, fault, Some(initial)).await;
        }
    }
}

async fn external_recovery_case(recording: bool, fault: Option<&str>) {
    external_recovery(recording, fault, None).await;
}

async fn external_recovery(recording: bool, fault: Option<&str>, initial: Option<&str>) {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let api = server.server_addr().to_ip().unwrap();
    let count = hits.clone();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = req.respond(tiny_http::Response::from_string("reply")
                .with_header(tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
                .with_header(tiny_http::Header::from_bytes("Date", "Tue, 27 Oct 2026 00:00:00 GMT").unwrap()));
        }
    });
    let (_tmp, ws) = support::workspace();
    let action = if recording { serde_json::json!({"type":"record","cassette":"external"}) }
        else { serde_json::json!({"type":"passthrough"}) };
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"external","match":{"url":format!("http://{api}/*")},"action":action},
        {"id":"mock","match":{"url":"https://api.example.test/mock"},"action":{"type":"mock","status":200,"body":"mock","latency_ms":2}},
        {"id":"hold","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}
    ]).to_string()).unwrap();
    let req = launcher::LaunchRequest { rules, ..support::launch_request("external", &format!("http://{}/observe.html",support::serve())) };
    let env = initial.map(|point| vec![("FRONTCAP_INTERCEPTION_FAULT".into(), point.into())]).unwrap_or_default();
    let mut attached = launcher::launch(&ws, req, &support::recorder_bin(), &env).await.unwrap();
    let dir = ws.runtime_dir(&attached.launch_id);
    let _guard = support::Runtime(dir.clone());
    let record = runtime::load_runtime(&dir).unwrap().unwrap();
    assert!(record.endpoint.as_ref().unwrap().proxy.is_some());
    macro_rules! mutate { ($name:expr,$op:expr) => {
        attached.client.call(Call::Mutate { lease: Some(attached.lease.clone()), request_id: id($name), op: $op }).await.unwrap()
    }; }
    mutate!("ready", Op::Advance { by_us: 1_000 });
    let start = if initial.is_some() { "" } else { "'https://api.example.test/start'," };
    mutate!("install", Op::Evaluate { expression: format!("window.answers=[];setTimeout(()=>{{for(const url of [{start}'http://{api}/value','http://{api}/value','https://api.example.test/mock','https://api.example.test/end']){{const x=new XMLHttpRequest();x.open('POST',url,false);try{{x.send('body');answers.push(x.responseText);}}catch(e){{answers.push('aborted');}}}}}},10);true") });
    let ordinal = if initial.is_some() {
        assert!(attached.client.call(Call::Mutate { lease: Some(attached.lease.clone()), request_id: id("parent"),
            op: Op::Advance { by_us: 50_000 } }).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), async {
            while record.recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("initial fault must terminate the recorder");
        let spec = runtime::load_spec(&dir).unwrap().unwrap();
        let bindings = frontcap_chromium::bindings::BindingJournal::read(&spec.runs_root.join(spec.run_id.as_str()).join("interceptions.jsonl")).unwrap();
        assert!(!bindings.iter().any(|entry| matches!(entry,
            frontcap_chromium::bindings::BindingEntry::Bound(b) if b.rule.as_str() == "external")));
        bindings.iter().filter_map(|entry| match entry {
            frontcap_chromium::bindings::BindingEntry::Bound(b) => Some(b.ordinal.trim_start_matches("req-").parse::<u64>().unwrap()), _ => None,
        }).max().unwrap_or(0) + 1
    } else {
        let Reply::Suspended { operation } = mutate!("parent", Op::Advance { by_us: 50_000 }) else { panic!() };
        record.recorder.kill().unwrap();
        operation.request.unwrap().ordinal.trim_start_matches("req-").parse::<u64>().unwrap()+1
    };
    if let Some(fault) = fault {
        let env = if fault == "after_rename" { ("FRONTCAP_STATE_FAULT".into(), "external.jsonl@after_rename".into()) }
            else { ("FRONTCAP_INTERCEPTION_FAULT".into(),format!("{fault}:req-{ordinal}")) };
        assert!(launcher::recover(&ws, &attached.launch_id, &support::recorder_bin(), &[env]).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime::load_runtime(&dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
    }
    launcher::recover(&ws, &attached.launch_id, &support::recorder_bin(), &[]).await
        .unwrap_or_else(|e| panic!("record={recording}, fault={fault:?}: {e}; {}",std::fs::read_to_string(dir.join("recorder.log")).unwrap_or_default()));
    attached = launcher::reattach(&ws, &attached.launch_id, id("replacement")).await.unwrap();
    assert_eq!(runtime::load_runtime(&dir).unwrap().unwrap().browser, record.browser);
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = mutate!("answers", Op::Evaluate { expression:"answers".into() }) else { panic!() };
    let interrupted_io = fault == Some("blocking_after_effect");
    let first_failed = interrupted_io && value[usize::from(initial.is_none())] == "aborted";
    let mut expected = vec![if first_failed { "aborted" } else { "reply" }, "reply", "mock", "aborted"];
    if initial.is_none() { expected.insert(0, "aborted"); }
    assert_eq!(value, serde_json::json!(expected));
    let sent = hits.load(Ordering::SeqCst);
    assert!(sent <= 2 && sent >= if first_failed { 1 } else { 2 }, "upstream requests sent: {sent}");
    let Reply::Status { status } = attached.client.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.t_us.0,51_000);
    let spec = runtime::load_spec(&dir).unwrap().unwrap();
    let bundle = spec.runs_root.join(spec.run_id.as_str());
    let events = frontcap_core::log::BehaviorLog::read(&bundle.join("trace.jsonl")).unwrap();
    let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        frontcap_core::event::EventKind::Request(r) if r.rule.as_ref().is_some_and(|r|r.as_str()=="external") => Some((e.t_us.0,r)), _=>None,
    }).collect();
    assert_eq!(requests.len(),2);
    for (index, (at,request)) in requests.into_iter().enumerate() {
        assert_eq!(at,11_000);
        let responses:Vec<_> = events.iter().filter_map(|e| match &e.kind {
            frontcap_core::event::EventKind::Response(r) if r.id==request.id => Some((e.t_us.0,r.status,r.source)),_=>None,
        }).collect();
        let expected = if index == 0 && first_failed { (11_000,0,frontcap_core::event::ResponseSource::Fail) }
            else { (11_000,200,frontcap_core::event::ResponseSource::Passthrough) };
        assert_eq!(responses,vec![expected]);
    }
    if recording {
        let lines = frontcap_core::cassette::parse(&std::fs::read_to_string(bundle.join("cassettes/external.jsonl")).unwrap()).unwrap();
        assert_eq!(lines.len(),if first_failed { 1 } else { 2 });
        if lines.len() == 2 { assert_eq!(lines[0],lines[1],"identical responses remain two distinct consumable lines"); }
        assert_eq!(lines[0].key,frontcap_core::cassette::Key::of("POST",&format!("http://{api}/value"),Some(b"body")));
        assert_eq!(frontcap_core::encode::unbase64(&lines[0].response.body_base64).unwrap(),b"reply");
        assert_eq!(lines[0].latency.virtual_ms,0);
    }
    mutate!("stop",Op::Stop);
    support::wait_until_exited(&dir).await;
}

#[tokio::test]
async fn consecutive_external_response_loss_never_reissues_upstream_requests() {
    for recording in [false, true] {
        external_recovery_case(recording, Some("blocking_after_effect")).await;
    }
    external_recovery_case(true, Some("blocking_before_capture")).await;
}

#[tokio::test]
async fn consecutive_request_prefix_keeps_sequence_slots_across_recorder_loss() {
    for cross_frame in [false, true] {
        for fault in [None, Some("blocking_after_prefix_binding"), Some("blocking_after_binding"), Some("blocking_after_effect")] {
            let rules = RuleSet::from_json(r#"[
                {"id":"sequence","match":{"url":"https://api.example.test/sequence/*"},"action":{"type":"sequence","steps":[
                    {"status":200,"body":"first","latency_ms":3},{"status":200,"body":"second","latency_ms":5},
                    {"status":200,"body":"third","latency_ms":10},{"status":200,"body":"fourth"}],"after_last":"fail"}},
                {"id":"hold","match":{"url":"https://api.example.test/start"},"action":{"type":"hold"}}
            ]"#).unwrap();
            let mut f = Fixture::launch_with_rules(None,rules).await;
            let offset = if cross_frame {
                f.mutate("frame", Op::Evaluate { expression: "window.child=document.createElement('iframe');child.src=location.href;document.body.append(child);true".into() }).await.unwrap();
                f.mutate("frame-ready", Op::Advance { by_us: 1_000 }).await.unwrap();
                assert_eq!(f.evaluated("child.contentDocument.readyState").await, "complete");
                1_000
            } else { 0 };
            f.mutate("install",Op::Evaluate { expression:"window.got={};setTimeout(()=>{let x=new XMLHttpRequest();x.open('GET','https://api.example.test/start',false);try{x.send();}catch(e){}for(const name of ['a','b'])(name==='a'&&window.child?child.contentWindow.eval(\"fetch('https://api.example.test/sequence/a',{method:'POST',body:'a'})\"):fetch('https://api.example.test/sequence/'+name,{method:'POST',body:name})).then(r=>r.text()).then(s=>got[name]=s);x=new XMLHttpRequest();x.open('POST','https://api.example.test/sequence/sync',false);x.send('sync');got.sync=x.responseText;fetch('https://api.example.test/sequence/after',{method:'POST',body:'after'}).then(r=>r.text()).then(s=>got.after=s);},10);true".into() }).await.unwrap();
            let Reply::Suspended { operation } = f.mutate("parent",Op::Advance { by_us:50_000 }).await.unwrap() else { panic!() };
            let first = operation.request.unwrap().ordinal.trim_start_matches("req-").parse::<u64>().unwrap();
            f.record.recorder.kill().unwrap();f.child.wait().unwrap();
            let delay = ("FRONTCAP_INTERCEPTION_DELAY".into(),"/sequence/a@1000".into());
            if let Some(fault) = fault {
                let ordinal = first + if fault == "blocking_after_prefix_binding" { 1 } else { 3 };
                assert!(launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[delay.clone(),
                    ("FRONTCAP_INTERCEPTION_FAULT".into(),format!("{fault}:req-{ordinal}"))]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5),async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
                }).await.unwrap();
            }
            launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[delay]).await
                .unwrap_or_else(|e|panic!("{fault:?}: {e}; {}",std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached = launcher::reattach(&f.ws,&f.spec.launch_id,id("replacement")).await.unwrap();f.client=attached.client;f.lease=attached.lease;
            assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser,f.record.browser);
            assert_eq!(f.evaluated("got").await,serde_json::json!({"a":"first","b":"second","sync":"third","after":"fourth"}),"{fault:?}");
            let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };assert_eq!(status.t_us.0,51_000+offset);
            let events = frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
            let requests:Vec<_>=events.iter().filter_map(|e|match &e.kind {
                frontcap_core::event::EventKind::Request(r) if r.url.contains("/sequence/")=>Some((e.t_us.0,r)),_=>None,
            }).collect();assert_eq!(requests.len(),4);
            for ((at,request),(name,expected_at)) in requests.iter().zip([("a",11_000),("b",11_000),("sync",11_000),("after",21_000)]) {
                assert_eq!(*at,expected_at+offset);assert!(request.url.ends_with(name));
                assert_eq!(request.body.as_ref().and_then(|b|b.text.as_deref()),Some(name));
                let responses:Vec<_>=events.iter().filter_map(|e|match &e.kind {
                    frontcap_core::event::EventKind::Response(r) if r.id==request.id=>Some((e.t_us.0,r.status)),_=>None,
                }).collect();assert_eq!(responses,vec![(match name { "a" => 14_000, "b" => 16_000, _ => 21_000 }+offset,200)],"{fault:?}: {name}");
            }
            let bindings = frontcap_chromium::bindings::BindingJournal::read(
                &f.spec.runs_root.join(f.spec.run_id.as_str()).join("interceptions.jsonl")).unwrap();
            let frame = |suffix: &str| bindings.iter().find_map(|entry| match entry {
                frontcap_chromium::bindings::BindingEntry::Bound(b) if b.url.ends_with(suffix) => b.frame_id.as_ref(),
                _ => None,
            }).expect("native frame identity");
            assert_eq!(frame("/sequence/a") != frame("/sequence/sync"), cross_frame);
            assert_eq!(frame("/sequence/b"), frame("/sequence/sync"));
            f.stop().await;
        }
    }
}

#[tokio::test]
async fn consecutive_request_recovery_defers_worker_issuers_until_the_document_returns() {
    for (workers, nested) in [(1, false), (2, false), (2, true)] {
    for mode in ["live", "recovery", "blocking_after_binding", "blocking_after_effect"] {
        eprintln!("worker prefix: workers={workers}, nested={nested}, mode={mode}");
        let rules = RuleSet::from_json(r#"[
            {"id":"page","match":{"url":"http://*/observe.html"},"action":{"type":"mock","status":200,
             "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html><p>worker recovery</p>"}},
            {"id":"sequence","match":{"url":"https://api.example.test/sequence/*"},"action":{"type":"sequence","steps":[
                {"status":200,"body":"first"},{"status":200,"body":"second"},{"status":200,"body":"third"},{"status":200,"body":"fourth"}],"after_last":"fail"}},
            {"id":"hold","match":{"url":"https://api.example.test/start"},"action":{"type":"hold"}}
        ]"#).unwrap();
        let mut f = Fixture::launch_with_rules(None, rules).await;
        assert_eq!(f.evaluated("crossOriginIsolated").await, true);
        let worker_script = "onmessage=e=>{const words=new Int32Array(e.data.buffer);const index=e.data.index;fetch('https://api.example.test/sequence/worker'+index).then(r=>r.text()).then(s=>postMessage(s));Atomics.store(words,index,1)};postMessage('ready')";
        let nested_script = format!("const child=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));child.onmessage=e=>postMessage(e.data);onmessage=e=>child.postMessage(e.data)", serde_json::to_string(worker_script).unwrap());
        let sources: Vec<_> = (0..workers).map(|index| if nested && index == 0 { nested_script.as_str() } else { worker_script }).collect();
        let install_workers = format!("window.words=new Int32Array(new SharedArrayBuffer({workers}*4));window.got={{}};window.ready=0;window.workers={}.map((source,index)=>{{const worker=new Worker(URL.createObjectURL(new Blob([source],{{type:'text/javascript'}})));worker.onmessage=e=>{{if(e.data==='ready')ready++;else got['worker'+index]=e.data}};return worker}});true", serde_json::to_string(&sources).unwrap());
        f.mutate("worker", Op::Evaluate { expression: install_workers }).await.unwrap();
        f.mutate("worker-ready",Op::Advance { by_us:1_000 }).await.unwrap();
        assert_eq!(f.evaluated("ready").await, workers);
        // Issue in reverse constructor order; document requests must still precede both workers,
        // whose rule consumption follows their stable constructor identities.
        f.mutate("install",Op::Evaluate { expression:"setTimeout(()=>{let x=new XMLHttpRequest();x.open('GET','https://api.example.test/start',false);try{x.send();}catch(e){}for(let index=workers.length-1;index>=0;index--){workers[index].postMessage({buffer:words.buffer,index});while(Atomics.load(words,index)===0){}}x=new XMLHttpRequest();x.open('GET','https://api.example.test/sequence/sync',false);x.send();got.sync=x.responseText;fetch('https://api.example.test/sequence/after').then(r=>r.text()).then(s=>got.after=s)},10);true".into() }).await.unwrap();
        let Reply::Suspended { operation } = f.mutate("parent",Op::Advance { by_us:50_000 }).await.unwrap() else { panic!() };
        let ordinal = operation.request.unwrap().ordinal;
        if mode == "live" {
            f.mutate("answer",Op::FailHeld { ordinal, reason: frontcap_core::rules::FailReason::Aborted }).await.unwrap();
        } else {
            f.record.recorder.kill().unwrap();f.child.wait().unwrap();
            let delay = ("FRONTCAP_INTERCEPTION_DELAY".into(), "/sequence/worker@1000".into());
            if mode != "recovery" {
                let next = ordinal.trim_start_matches("req-").parse::<u64>().unwrap()+1;
                assert!(launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[delay.clone(),
                    ("FRONTCAP_INTERCEPTION_FAULT".into(),format!("{mode}:req-{next}"))]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5),async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
                }).await.unwrap();
            }
            launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[delay]).await
                .unwrap_or_else(|e|panic!("{mode}: {e}; {}",std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached=launcher::reattach(&f.ws,&f.spec.launch_id,id("replacement")).await.unwrap();f.client=attached.client;f.lease=attached.lease;
        }
        tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                if matches!(f.client.call(Call::Result { request_id:id("parent") }).await.unwrap(),Reply::Result { result:RequestResult::Completed { .. }, .. }) { break }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("parent completes after the held answer");
        let expected = if workers == 1 { serde_json::json!({"sync":"first","after":"second","worker0":"third"}) }
            else { serde_json::json!({"sync":"first","after":"second","worker0":"third","worker1":"fourth"}) };
        assert_eq!(f.evaluated("got").await, expected, "workers={workers}, nested={nested}, mode={mode}");
        assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser,f.record.browser);
        let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
        assert_eq!(status.t_us.0,52_000,"{mode}");
        let root=f.spec.runs_root.join(f.spec.run_id.as_str());
        let entries=frontcap_chromium::bindings::BindingJournal::read(&root.join("interceptions.jsonl")).unwrap();
        for index in 0..workers {
            let path = if nested && index == 0 { vec![1, 1] } else { vec![index as u64 + 1] };
            assert!(entries.iter().any(|e|matches!(e,frontcap_chromium::bindings::BindingEntry::Bound(b)
                if b.url.ends_with(&format!("/sequence/worker{index}")) && b.worker_lineage.as_ref().is_some_and(|w|w.constructors==path))));
        }
        let trace=frontcap_core::log::BehaviorLog::read(&root.join("trace.jsonl")).unwrap();
        let requests:Vec<_>=trace.iter().filter_map(|e|match &e.kind {
            frontcap_core::event::EventKind::Request(r) if r.url.contains("/sequence/")=>Some(r),_=>None,
        }).collect();
        let mut expected_order = vec!["sync".to_owned(), "after".to_owned()];
        expected_order.extend((0..workers).map(|index| format!("worker{index}")));
        assert_eq!(requests.iter().map(|r|r.url.rsplit('/').next().unwrap()).collect::<Vec<_>>(),expected_order,"{mode}");
        for request in requests {
            assert_eq!(trace.iter().filter(|e|matches!(&e.kind,frontcap_core::event::EventKind::Response(r) if r.id==request.id)).count(),1);
        }
        f.stop().await;
    }
    }
}

#[tokio::test]
async fn synchronous_popup_requests_recover_in_their_retained_target() {
    for cross_origin in [false, true] {
        for fault in [None, Some("blocking_after_binding"), Some("blocking_after_effect"), Some("agent_before_dispatch"), Some("agent_after_effect")] {
            let body = "<!doctype html><script>setTimeout(()=>{let x=new XMLHttpRequest();x.open('GET','https://api.example.test/start',false);let first='aborted';try{x.send();first=x.responseText}catch(e){}x=new XMLHttpRequest();x.open('GET','https://api.example.test/next',false);x.send();opener.postMessage({source:'popup-request',answer:x.responseText,first},'*')},10)</script>";
            let rules=RuleSet::from_json(&serde_json::json!([
                {"id":"popup","match":{"url":"*popup-held.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
                {"id":"start","match":{"url":"https://api.example.test/start"},"action":{"type":"hold"}},
                {"id":"next","match":{"url":"https://api.example.test/next"},"action":{"type":"mock","status":200,"body":"continued","latency_ms":3}}
            ]).to_string()).unwrap();
            let mut f=Fixture::launch_configured(fault.and_then(|p|p.strip_prefix("agent_")).map(|p|format!("answer@{p}")),rules,true).await;
            let url=if cross_origin { "https://popup.example.test/popup-held.html".to_owned() } else { f.spec.url.replace("observe.html","popup-held.html") };
            f.mutate("install",Op::Evaluate { expression:format!("window.answers=[];addEventListener('message',e=>{{if(e.data.source==='popup-request')answers.push([e.origin,e.data.first,e.data.answer])}});window.child=window.open({});true",serde_json::to_string(&url).unwrap()) }).await.unwrap();
            let Reply::Suspended { operation }=f.mutate("parent",Op::Advance { by_us:50_000 }).await.unwrap() else { panic!("popup hold") };
            let request=operation.request.unwrap();
            let at=request.t_us.0;
            let ordinal=request.ordinal;
            if fault.is_some_and(|p|p.starts_with("agent_")) {
                assert!(f.mutate("answer",Op::Respond { ordinal:ordinal.clone(),response:frontcap_core::rules::MockSpec {
                    status:200,headers:Default::default(),body:"answered".into(),body_base64:None,latency_ms:0,
                } }).await.is_err());
            } else { f.record.recorder.kill().unwrap(); }
            f.child.wait().unwrap();
            if let Some(point)=fault.filter(|p|!p.starts_with("agent_")) {
                let next=ordinal.trim_start_matches("req-").parse::<u64>().unwrap()+1;
                assert!(launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[
                    ("FRONTCAP_INTERCEPTION_FAULT".into(),format!("{point}:req-{next}"))]).await.is_err());
                tokio::time::timeout(Duration::from_secs(5),async {
                    while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() { tokio::time::sleep(Duration::from_millis(10)).await; }
                }).await.unwrap();
            }
            launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&[]).await
                .unwrap_or_else(|e|panic!("cross={cross_origin}, {fault:?}: {e}; {}",std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
            let attached=launcher::reattach(&f.ws,&f.spec.launch_id,id("replacement")).await.unwrap();f.client=attached.client;f.lease=attached.lease;
            assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser,f.record.browser);
            assert_eq!(f.evaluated("answers").await,serde_json::json!([[url.rsplit_once('/').unwrap().0,if fault==Some("agent_after_effect") {"answered"} else {"aborted"},"continued"]]));
            let Reply::Status { status }=f.client.call(Call::Status).await.unwrap() else { panic!() };
            assert_eq!(status.t_us.0,51_000);assert!(status.suspension.is_none());
            let root=f.spec.runs_root.join(f.spec.run_id.as_str());
            let entries=frontcap_chromium::bindings::BindingJournal::read(&root.join("interceptions.jsonl")).unwrap();
            let bound=entries.iter().find_map(|e|match e {frontcap_chromium::bindings::BindingEntry::Bound(b) if b.ordinal==ordinal=>Some(b),_=>None}).unwrap();
            assert_ne!(bound.target_id,f.record.endpoint.as_ref().unwrap().target_id);
            let events=frontcap_core::log::BehaviorLog::read(&root.join("trace.jsonl")).unwrap();
            assert_eq!(events.iter().filter(|e|matches!(&e.kind,frontcap_core::event::EventKind::Context(c) if c.event==frontcap_core::event::ContextEvent::Opened)).count(),1);
            assert_eq!(events.iter().filter(|e|matches!(&e.kind,frontcap_core::event::EventKind::Response(r) if r.id==ordinal)).count(),1);
            let requests:Vec<_>=events.iter().filter_map(|e|match &e.kind {
                frontcap_core::event::EventKind::Request(r) if r.url.starts_with("https://api.example.test/")=>Some((e.t_us.0,r)),_=>None,
            }).collect();
            assert_eq!(requests.len(),2);
            for (index,(issued,r)) in requests.iter().enumerate() {
                assert_eq!(*issued,at);assert_eq!(r.context.as_deref(),Some("popup-1"));
                let responses:Vec<_>=events.iter().filter_map(|e|match &e.kind {
                    frontcap_core::event::EventKind::Response(response) if response.id==r.id=>Some((e.t_us.0,response.status)),_=>None,
                }).collect();
                let expected=if index==0 {(at,if fault==Some("agent_after_effect") {200} else {0})} else {(at+3_000,200)};
                assert_eq!(responses,vec![expected]);
            }
            if let Some(point)=fault.and_then(|p|p.strip_prefix("agent_")) {
                let journal=Journal::open(&runtime::journal_path(&f.dir)).unwrap();
                let Some(RequestState::Completed(value))=journal.state(&id("answer")) else { panic!("unresolved popup answer") };
                let answer:Answer=serde_json::from_value(value.clone()).unwrap();
                assert!(matches!(answer,Answer::Ok(Reply::Done { outcome:Outcome::Ack,.. }))==(point=="after_effect"),"{answer:?}");
            }
            f.stop().await;
        }
    }
}

#[tokio::test]
async fn synchronous_recovery_follows_a_task_between_page_and_popup_targets() {
    for starts_in_popup in [false,true] {
        for fault in [None,Some("blocking_after_prefix_binding"),Some("blocking_after_binding"),Some("blocking_after_effect")] {
            recover_task_between_page_and_popup(starts_in_popup,fault,None).await;
        }
    }
}

/// The engine reports that a main thread awaits its synchronous response only once the renderer has
/// read that response. Meanwhile the thread cannot answer a command, so recovery must not send it the
/// popup prefix's virtual-time vote release.
#[tokio::test]
async fn synchronous_recovery_sends_no_vote_command_to_a_thread_awaiting_its_response() {
    for fault in [None,Some("blocking_after_prefix_binding")] {
        recover_task_between_page_and_popup(false,fault,Some(1_000)).await;
    }
}

async fn recover_task_between_page_and_popup(starts_in_popup:bool,fault:Option<&str>,arrival_report_lag_ms:Option<u64>) {
    let body="<!doctype html><script>window.calls=0;window.held=()=>{calls++;let x=new XMLHttpRequest();x.open('GET','https://api.example.test/start',false);try{x.send()}catch(e){}};window.begin=()=>fetch('https://api.example.test/sequence/prefix').then(r=>r.text());window.step=name=>{calls++;let x=new XMLHttpRequest();x.open('GET','https://api.example.test/sequence/'+name,false);x.send();return x.responseText}</script>";
    let rules=RuleSet::from_json(&serde_json::json!([
        {"id":"popup","match":{"url":"*mixed-target.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
        {"id":"start","match":{"url":"https://api.example.test/start"},"action":{"type":"hold"}},
        {"id":"sequence","match":{"url":"https://api.example.test/sequence/*"},"action":{"type":"sequence","steps":[
            {"status":200,"body":"first","latency_ms":2},{"status":200,"body":"second","latency_ms":3},
            {"status":200,"body":"third","latency_ms":4},{"status":200,"body":"fourth"},{"status":200,"body":"fifth"}],"after_last":"fail"}}
    ]).to_string()).unwrap();
    let mut f=Fixture::launch_configured(None,rules,true).await;
    let url=f.spec.url.replace("observe.html","mixed-target.html");
    f.mutate("open",Op::Evaluate {expression:format!("window.child=window.open({});true",serde_json::to_string(&url).unwrap())}).await.unwrap();
    f.mutate("popup-ready",Op::Advance {by_us:10_000}).await.unwrap();
    assert_eq!(f.evaluated("typeof child.step").await,"function","{}",f.evaluated("[child.location.href,child.document.readyState,child.document.documentElement.outerHTML]").await);
    let first=if starts_in_popup {"child.held();"} else {"let x=new XMLHttpRequest();x.open('GET','https://api.example.test/start',false);try{x.send()}catch(e){}"};
    f.mutate("install",Op::Evaluate {expression:format!("window.got={{}};setTimeout(()=>{{{first}child.begin().then(s=>got.prefix=s);got.popup1=child.step('popup1');let y=new XMLHttpRequest();y.open('GET','https://api.example.test/sequence/root',false);y.send();got.root=y.responseText;got.popup2=child.step('popup2');fetch('https://api.example.test/sequence/after').then(r=>r.text()).then(s=>got.after=s)}},10);true")}).await.unwrap();
    let Reply::Suspended {operation}=f.mutate("parent",Op::Advance {by_us:50_000}).await.unwrap() else {panic!()};
    let first=operation.request.unwrap().ordinal.trim_start_matches("req-").parse::<u64>().unwrap();
    f.record.recorder.kill().unwrap();f.child.wait().unwrap();
    let mut env=vec![("FRONTCAP_INTERCEPTION_DELAY".to_string(),"/sequence/prefix@1000".to_string())];
    if let Some(ms)=arrival_report_lag_ms { env.push(("FRONTCAP_ARRIVAL_REPORT_DELAY".into(),ms.to_string())); }
    if let Some(point)=fault {
        let offset=match point {"blocking_after_prefix_binding"=>1,"blocking_after_binding"=>2,_=>3};
        let mut faulted=env.clone();
        faulted.push(("FRONTCAP_INTERCEPTION_FAULT".into(),format!("{point}:req-{}",first+offset)));
        assert!(launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&faulted).await.is_err());
        tokio::time::timeout(Duration::from_secs(5),async {
            while runtime::load_runtime(&f.dir).unwrap().unwrap().recorder.is_alive() {tokio::time::sleep(Duration::from_millis(10)).await;}
        }).await.unwrap();
    }
    launcher::recover(&f.ws,&f.spec.launch_id,&support::recorder_bin(),&env).await.unwrap_or_else(|e|panic!("popup={starts_in_popup}, {fault:?}: {e}; {}",std::fs::read_to_string(f.dir.join("recorder.log")).unwrap_or_default()));
    let attached=launcher::reattach(&f.ws,&f.spec.launch_id,id("replacement")).await.unwrap();f.client=attached.client;f.lease=attached.lease;
    assert_eq!(f.evaluated("got").await,serde_json::json!({"prefix":"first","popup1":"second","root":"third","popup2":"fourth","after":"fifth"}));
    assert_eq!(f.evaluated("child.calls").await,if starts_in_popup {3} else {2});
    assert_eq!(runtime::load_runtime(&f.dir).unwrap().unwrap().browser,f.record.browser);
    let Reply::Status {status}=f.client.call(Call::Status).await.unwrap() else {panic!()};assert_eq!(status.t_us.0,61_000);
    let trace=frontcap_core::log::BehaviorLog::read(&f.spec.runs_root.join(f.spec.run_id.as_str()).join("trace.jsonl")).unwrap();
    let requests:Vec<_>=trace.iter().filter_map(|e|match &e.kind {frontcap_core::event::EventKind::Request(r) if r.url.contains("/sequence/")=>Some((e.t_us.0,r)),_=>None}).collect();
    assert_eq!(requests.len(),5);
    for ((issued,r),(name,context,at,answered)) in requests.iter().zip([
        ("prefix",Some("popup-1"),21_000,23_000),("popup1",Some("popup-1"),21_000,24_000),
        ("root",None,24_000,28_000),("popup2",Some("popup-1"),28_000,28_000),("after",None,28_000,28_000)]) {
        assert!(r.url.ends_with(name));assert_eq!(*issued,at);assert_eq!(r.context.as_deref(),context);
        let responses:Vec<_>=trace.iter().filter_map(|e|match &e.kind {frontcap_core::event::EventKind::Response(answer) if answer.id==r.id=>Some((e.t_us.0,answer.status)),_=>None}).collect();
        assert_eq!(responses,vec![(answered,200)],"{name}");
    }
    assert_eq!(trace.iter().filter(|e|matches!(&e.kind,frontcap_core::event::EventKind::Context(c) if c.event==frontcap_core::event::ContextEvent::Opened)).count(),1);
    f.stop().await;
}

#[tokio::test]
async fn consecutive_hold_recovery_refuses_unbounded_chains() {
    for automatic in [false, true] {
        let rules = RuleSet::from_json(r#"[{"id":"auto","match":{"url":"https://api.example.test/automatic"},"action":{"type":"mock","status":200,"body":"ok"}},{"id":"sync","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
        let mut f = Fixture::launch_with_rules(None, rules).await;
        let script = if automatic {
            "setTimeout(()=>{let i=0;while(true){const x=new XMLHttpRequest();x.open('GET','https://api.example.test/'+(i++?'automatic':'held'),false);try{x.send();}catch(e){}}},10);true"
        } else {
            "setTimeout(()=>{while(true){const x=new XMLHttpRequest();x.open('GET','https://api.example.test/held',false);try{x.send();}catch(e){}}},10);true"
        };
        f.mutate("install", Op::Evaluate { expression: script.into() }).await.unwrap();
        assert!(matches!(f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap(), Reply::Suspended { .. }));
        f.record.recorder.kill().unwrap();
        f.child.wait().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[])).await.expect("bounded recovery");
        let error = result.expect_err("unsupported continuation cannot succeed");
        let expected = if automatic { "64 consecutive automatic requests" } else { "64 consecutive synchronous holds" };
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!f.record.browser.as_ref().unwrap().is_alive());
        let journal = Journal::open(&runtime::journal_path(&f.dir)).unwrap();
        assert!(matches!(journal.state(&id("parent")), Some(RequestState::Dispatched)));
        assert_eq!(journal.dependents(&id("parent")).len(), if automatic { 1 } else { 64 });
    }
}

async fn recovered_answer(point: Option<&str>, recovery_point: Option<&str>) {
    let Fixture { _guard, _temp, ws, spec, dir, mut child, record, mut client, lease } = Fixture::launch(point.map(|p| format!("answer@{p}"))).await;
    client.call(Call::Mutate { lease: Some(lease.clone()), request_id: id("setup"), op: Op::Evaluate {
        expression: "window.answers=[];setTimeout(()=>{answers.push(confirm('first'));answers.push(prompt('second','default'));},10);true".into(),
    }}).await.unwrap();
    let Reply::Suspended { operation } = client.call(Call::Mutate { lease: Some(lease.clone()), request_id: id("parent"), op: Op::Advance { by_us: 50_000 } }).await.unwrap() else { panic!() };
    let cleanup_id = format!("mcp-dialog-cleanup-{}", frontcap_core::canonical::sha256_hex(&format!("parent:{}", operation.dialog.as_ref().unwrap().id)));
    if point.is_some() {
        let response = client.call(Call::AnswerDialog { lease, request_id: id("answer"), response: DialogResponse {
            parent_request_id: id("parent"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: true, prompt_text: None,
        }}).await;
        assert!(response.is_err(), "fault did not stop recorder: {point:?}");
    } else {
        record.recorder.kill().unwrap();
    }
    child.wait().unwrap();
    assert!(record.browser.as_ref().unwrap().is_alive());
    drop(client);
    if let Some(point) = recovery_point {
        let failed = launcher::recover(&ws, &spec.launch_id, &support::recorder_bin(), &[("FRONTCAP_FAULT".into(), format!("{cleanup_id}@{point}"))]).await;
        assert!(failed.is_err(), "cleanup fault did not fire: {point}");
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime::load_runtime(&dir).unwrap().unwrap().recorder.is_alive() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(record.browser.as_ref().unwrap().is_alive());
    }
    launcher::recover(&ws, &spec.launch_id, &support::recorder_bin(), &[]).await.unwrap_or_else(|e| panic!("{point:?}: {e:?}; {}", std::fs::read_to_string(dir.join("recorder.log")).unwrap_or_default()));
    let attached = launcher::reattach(&ws, &spec.launch_id, id("replacement")).await.unwrap();
    let mut client = attached.client;
    let Reply::Status { status } = client.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.t_us.0, 51_000, "recovery changed the parent's absolute grant: {point:?}");
    assert!(!status.input_cleanup_pending);
    assert!(status.suspension.is_none());
    let Reply::Result { result: RequestResult::Completed { answer }, .. } = client.call(Call::Result { request_id: id("parent") }).await.unwrap() else { panic!() };
    assert!(matches!(*answer, Answer::Ok(Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. }) if t_us.0 == 51_000));
    let applied = matches!(point, Some("after_effect" | "after_result" | "after_complete"));
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = client.call(Call::Mutate {
        lease: Some(attached.lease.clone()), request_id: id("read"), op: Op::Evaluate { expression: "answers".into() },
    }).await.unwrap() else { panic!() };
    assert_eq!(value, serde_json::json!([applied, null]), "{point:?}");
    if point.is_some() {
        let Some(RequestState::Completed(value)) = Journal::read_state(&runtime::journal_path(&dir), &id("answer")).unwrap() else { panic!() };
        let answer: Answer = serde_json::from_value(value).unwrap();
        if applied { assert!(matches!(answer, Answer::Ok(Reply::DialogAnswered { receipt }) if receipt.accept)); }
        else { assert!(matches!(answer, Answer::Ok(Reply::Done { outcome: Outcome::NotApplied, .. }))); }
    }
    let control: ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
    assert!(control.dialog_answers.values().any(|p| p.origin == DialogAnswerOrigin::LeaseCleanup));
    client.call(Call::Mutate { lease: Some(attached.lease), request_id: id("stop"), op: Op::Stop }).await.unwrap();
    support::wait_until_exited(&dir).await;
    let commands = frontcap_core::inputs::read_commands(&status.bundle).unwrap();
    let parent = commands.iter().find(|c| c.request_id == id("parent")).expect("recovered parent input missing");
    let journal = Journal::open(&runtime::journal_path(&dir)).unwrap();
    assert_eq!(parent.dialogs.iter().map(|d| d.request_id.clone()).collect::<Vec<_>>(), journal.dependents(&id("parent")));
    if point.is_some() {
        let first = &parent.dialogs[0];
        assert_eq!(first.request_id, id("answer"));
        assert_eq!(first.attempt.as_ref().unwrap().origin, DialogAnswerOrigin::Agent);
        assert_eq!(matches!(first.result, frontcap_core::inputs::DialogInputOutcome::Answered { .. }), applied);
    }
    let applied_messages: Vec<_> = parent.dialogs.iter().filter_map(|d| match &d.result {
        frontcap_core::inputs::DialogInputOutcome::Answered { receipt } => Some(receipt.dialog.message.as_str()),
        frontcap_core::inputs::DialogInputOutcome::NotApplied => None,
    }).collect();
    assert_eq!(applied_messages, ["first", "second"], "recovery archive duplicated or lost a native answer");
    let trace = frontcap_core::log::BehaviorLog::read(&status.bundle.join("trace.jsonl")).unwrap();
    let dialogs: Vec<_> = trace.iter().filter_map(|line| match &line.kind {
        frontcap_core::event::EventKind::Dialog(dialog) => Some(dialog), _ => None,
    }).collect();
    assert_eq!(dialogs.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(), ["first", "second"], "duplicate or reordered trace: {point:?}/{recovery_point:?}");
    assert_ne!(dialogs[0].native.as_ref().unwrap().id, dialogs[1].native.as_ref().unwrap().id);
    if matches!(point, Some("before_dispatch" | "after_effect")) || recovery_point == Some("before_dispatch") {
        let result = frontcap_recorder::replay::replay(&ws, &status.bundle, frontcap_recorder::replay::ReplayOptions {
            url: None, watchdog_ms: 10_000, repo_root: support::repo_root(), recorder_bin: support::recorder_bin(), env: vec![],
        }).await;
        assert!(matches!(result, Err(frontcap_recorder::replay::ReplayError::NotReplayable { missing, .. }) if missing.iter().any(|m| m == "recorder_fault")),
            "dependent inputs must not erase the run's recorder-fault boundary");
    }
}

#[tokio::test]
async fn held_dialogs_are_dismissed_before_renderer_recovery() { recovered_answer(None, None).await; }

#[tokio::test]
async fn dependent_answers_recover_from_each_durable_boundary() {
    for point in ["before_dispatch", "after_dispatch", "after_effect", "after_result", "after_complete"] {
        recovered_answer(Some(point), None).await;
    }
}

#[tokio::test]
async fn dialog_cleanup_can_resume_after_a_second_recorder_loss() {
    for point in ["before_dispatch", "after_dispatch", "after_effect", "after_result", "after_complete"] {
        recovered_answer(None, Some(point)).await;
    }
}

#[tokio::test]
async fn a_dialog_in_mouse_down_recovers_without_repeating_the_input() {
    let mut f = Fixture::launch(None).await;
    f.mutate("setup", Op::Evaluate { expression: "document.body.innerHTML='<button id=button>Click</button>';window.events=[];button.addEventListener('mousedown',()=>{events.push('down');events.push(confirm('input hold'));});button.addEventListener('mouseup',()=>events.push('up'));button.addEventListener('click',()=>events.push('click'));true".into() }).await.unwrap();
    let action = frontcap_core::action::Action::Click { target: frontcap_core::predicate::Locator::Css { selector: "#button".into() } };
    assert!(matches!(f.mutate("parent", Op::Act { action, then: None, context: None }).await.unwrap(), Reply::Suspended { .. }));
    f.restart().await;
    assert_eq!(f.evaluated("({events,now:Date.now()-946684800000})").await, serde_json::json!({"events":["down",false,"up","click"],"now":1}));
    let Reply::Result { result: RequestResult::Completed { answer }, .. } = f.client.call(Call::Result { request_id: id("parent") }).await.unwrap() else { panic!() };
    assert!(matches!(*answer, Answer::Ok(Reply::Done { outcome: Outcome::Acted { .. }, .. })), "{answer:?}");
    f.stop().await;
}

async fn cancelled_navigation(action: &str, crash: bool) {
    let mut f = Fixture::launch(None).await;
    // Give Back a real cross-document history entry; all three commands then start at 2 ms.
    f.mutate("previous", Op::Act { action: frontcap_core::action::Action::Navigate { url: format!("{}?current", f.spec.url) }, then: None, context: None }).await.unwrap();
    f.mutate("loaded", Op::Advance { by_us: 1_000 }).await.unwrap();
    f.mutate("setup", Op::Evaluate { expression: "document.body.innerHTML='<button id=button>Activate</button>';window.leaves=0;true".into() }).await.unwrap();
    f.mutate("activate", Op::Act { action: frontcap_core::action::Action::Click { target: frontcap_core::predicate::Locator::Css { selector: "#button".into() } }, then: None, context: None }).await.unwrap();
    f.mutate("listener", Op::Evaluate { expression: "window.addEventListener('beforeunload',e=>{leaves++;e.preventDefault();e.returnValue='unsaved';});true".into() }).await.unwrap();
    let action = match action {
        "navigate" => frontcap_core::action::Action::Navigate { url: format!("{}?next", f.spec.url) },
        "reload" => frontcap_core::action::Action::Reload {},
        "back" => frontcap_core::action::Action::Back {},
        _ => unreachable!(),
    };
    let Reply::Suspended { operation } = f.mutate("parent", Op::Act { action, then: None, context: None }).await.unwrap() else { panic!() };
    assert_eq!(operation.dialog.as_ref().unwrap().kind, frontcap_core::event::DialogKind::Beforeunload);
    if crash { f.restart().await; }
    else {
        tokio::time::timeout(Duration::from_secs(5), f.client.call(Call::Release { lease: f.lease.clone(), request_id: id("release") })).await.expect("navigation cancellation stalled").unwrap();
        let Reply::Attached { lease, .. } = f.client.call(Call::Reattach { request_id: id("replacement") }).await.unwrap() else { panic!() };
        f.lease = lease;
    }
    assert_eq!(f.evaluated("({leaves,search:location.search,now:Date.now()-946684800000})").await, serde_json::json!({"leaves":1,"search":"?current","now":2}));
    let Reply::Result { result: RequestResult::Completed { answer }, .. } = f.client.call(Call::Result { request_id: id("parent") }).await.unwrap() else { panic!() };
    assert!(matches!(*answer, Answer::Err(ref e) if e.code == frontcap_recorder_protocol::ErrorCode::NavigationCancelled), "a dismissed navigation must report cancellation: {answer:?}");
    let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
    f.stop().await;
    let trace = frontcap_core::log::BehaviorLog::read(&status.bundle.join("trace.jsonl")).unwrap();
    let relevant: Vec<_> = trace.iter().filter(|line| match &line.kind {
        frontcap_core::event::EventKind::Action(a) => a.request_id == id("parent"),
        frontcap_core::event::EventKind::Dialog(d) => d.kind == frontcap_core::event::DialogKind::Beforeunload,
        _ => false,
    }).collect();
    assert_eq!(relevant.len(), 2, "the navigation attempt and its cancellation must each be recorded once");
    assert!(matches!(relevant[0].kind, frontcap_core::event::EventKind::Action(_)));
    assert!(matches!(relevant[1].kind, frontcap_core::event::EventKind::Dialog(_)));
    assert!(relevant.iter().all(|line| line.t_us.0 == 2_000));
}

#[tokio::test]
async fn a_cancelled_beforeunload_is_not_navigated_again_after_recovery() {
    for action in ["navigate", "reload", "back"] { cancelled_navigation(action, true).await; }
}

#[tokio::test]
async fn beforeunload_dismissal_completes_live_navigation_without_a_commit_timeout() {
    for action in ["navigate", "reload", "back"] { cancelled_navigation(action, false).await; }
}

#[tokio::test]
async fn unbounded_dialog_recovery_fails_and_terminates_the_owned_browser() {
    let mut f = Fixture::launch(None).await;
    f.mutate("setup", Op::Evaluate { expression: "setTimeout(()=>{while(true)confirm('again')},10);true".into() }).await.unwrap();
    assert!(matches!(f.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap(), Reply::Suspended { .. }));
    f.record.recorder.kill().unwrap();
    f.child.wait().unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(30), launcher::recover(&f.ws, &f.spec.launch_id, &support::recorder_bin(), &[])).await.expect("unbounded recovery");
    assert!(recovered.is_err());
    assert!(!f.record.browser.as_ref().unwrap().is_alive());
    let control: ControlState = frontcap_core::state::load(&runtime::control_path(&f.dir)).unwrap().unwrap();
    assert!(control.faults.iter().any(|fault| fault.detail.contains("64 consecutive dialogs") && fault.at_us.0 == 11_000), "{:?}", control.faults);
    assert_eq!(control.dialog_answers.values().filter(|plan| matches!(plan.result, Some(Answer::Ok(Reply::DialogAnswered { .. })))).count(), 64);
    assert!(matches!(Journal::read_state(&runtime::journal_path(&f.dir), &id("parent")).unwrap(), Some(RequestState::Dispatched)), "fatal cleanup must not invent a completed parent");
}

#[tokio::test]
async fn an_accepted_beforeunload_answer_survives_recorder_loss_once() {
    accepted_beforeunload_survives_recorder_loss(Some("answer@after_effect"), None, RuleSet::default()).await;
}

/// Lost after its document request was answered, the accepted navigation has neither its action nor
/// its commit in the trace and no retained request; the browser still has it. The document is mocked:
/// a pass-through exchange would lose its connection with the recorder's proxy.
#[tokio::test]
async fn an_accepted_navigation_lost_after_its_document_request_commits_once() {
    let rules = RuleSet::from_json(&serde_json::json!([{"id": "accepted", "match": {"url": "http://127.0.0.1:*/observe.html?accepted"},
        "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": "<!doctype html><title>accepted</title>"}}]).to_string()).unwrap();
    accepted_beforeunload_survives_recorder_loss(None, Some("after_document_answer:/observe.html?accepted"), rules).await;
}

async fn accepted_beforeunload_survives_recorder_loss(fault: Option<&str>, interception: Option<&str>, rules: RuleSet) {
    let mut f = Fixture::launch_with_faults(fault.map(Into::into), interception, rules, false).await;
    f.mutate("setup", Op::Evaluate { expression: "document.body.innerHTML='<button id=button>Activate</button>';sessionStorage.leaves='0';true".into() }).await.unwrap();
    f.mutate("activate", Op::Act { action: frontcap_core::action::Action::Click { target: frontcap_core::predicate::Locator::Css { selector: "#button".into() } }, then: None, context: None }).await.unwrap();
    f.mutate("listener", Op::Evaluate { expression: "window.addEventListener('beforeunload',e=>{sessionStorage.leaves=String(Number(sessionStorage.leaves)+1);e.preventDefault();e.returnValue='unsaved';});true".into() }).await.unwrap();
    let target = format!("{}?accepted", f.spec.url);
    let Reply::Suspended { operation } = f.mutate("parent", Op::Act { action: frontcap_core::action::Action::Navigate { url: target.clone() }, then: None, context: None }).await.unwrap() else { panic!() };
    let result = f.client.call(Call::AnswerDialog { lease: f.lease.clone(), request_id: id("answer"), response: DialogResponse {
        parent_request_id: id("parent"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: true, prompt_text: None,
    }}).await;
    if fault.is_some() { assert!(result.is_err()); }
    // The recorder is lost at the fault, whether or not the answer's reply came first.
    let lost = tokio::time::timeout(Duration::from_secs(10), async { while f.record.recorder.is_alive() { tokio::time::sleep(Duration::from_millis(20)).await } }).await;
    assert!(lost.is_ok(), "the recorder survived {fault:?} {interception:?}: {result:?}");
    f.restart().await;
    assert_eq!(f.evaluated("({leaves:sessionStorage.leaves,search:location.search,now:Date.now()-946684800000})").await, serde_json::json!({"leaves":"1","search":"?accepted","now":1}));
    let Reply::Result { result: RequestResult::Completed { answer }, .. } = f.client.call(Call::Result { request_id: id("parent") }).await.unwrap() else { panic!() };
    assert!(matches!(*answer, Answer::Ok(Reply::Done { outcome: Outcome::Acted { .. }, .. })), "{answer:?}");
    let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
    f.stop().await;
    let trace = frontcap_core::log::BehaviorLog::read(&status.bundle.join("trace.jsonl")).unwrap();
    use frontcap_core::event::EventKind;
    let commits = trace.iter().filter(|line| matches!(&line.kind, EventKind::Navigation(n) if n.url == target)).count();
    let documents = trace.iter().filter(|line| matches!(&line.kind, EventKind::Request(r) if r.url == target)).count();
    assert_eq!((commits, documents), (1, 1), "the accepted navigation must commit once: {:#?}", trace.iter().filter(|line| matches!(&line.kind,
        EventKind::Action(_) | EventKind::Navigation(_) | EventKind::Request(_) | EventKind::Response(_) | EventKind::Dialog(_))).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_suspended_predicate_wait_recovers_its_condition_and_watch_cleanup() {
    let mut f = Fixture::launch(None).await;
    f.mutate("setup", Op::Evaluate {
        expression: "window.answers=[];setTimeout(()=>{answers.push(confirm('predicate'));document.body.dataset.answered='yes';},10);true".into(),
    }).await.unwrap();
    let op = Op::UntilPredicate {
        predicate: frontcap_core::predicate::Predicate::Attached {
            target: frontcap_core::predicate::Locator::Css { selector: "body[data-answered=yes]".into() },
        }, max_us: 50_000, context: None,
    };
    let Reply::Suspended { operation } = f.mutate("parent", op.clone()).await.unwrap() else { panic!("wait did not suspend") };
    assert_eq!(operation.dialog.as_ref().unwrap().t_us.0, 11_000);
    f.restart().await;
    let answer = f.mutate("parent", op).await.unwrap();
    assert!(matches!(answer, Reply::Done { outcome: Outcome::Clock {
        result: frontcap_core::event::ClockOutcome::ConditionMet { .. }, start_us, t_us, ..
    }, .. } if start_us.0 == 1_000 && t_us.0 == 11_000), "{answer:?}");
    let Reply::Status { status } = f.client.call(Call::Status).await.unwrap() else { panic!() };
    assert!(status.watches.is_empty() && status.suspension.is_none() && status.running.is_none());
    let state: ControlState = frontcap_core::state::load(&runtime::control_path(&f.dir)).unwrap().unwrap();
    assert!(state.predicate_watches.is_empty() && state.watches.list().is_empty());
    assert_eq!(state.dialog_answers.len(), 1);
    assert!(state.dialog_answers.values().all(|p| p.origin == DialogAnswerOrigin::LeaseCleanup));
    assert_eq!(f.evaluated("answers").await, serde_json::json!([false]));
    f.stop().await;
}
