#![cfg(feature = "fault-injection")]
mod support;

use frontcap_core::event::SampleValue;
use frontcap_core::ids::{LaunchId, RequestId, WatchName};
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_recorder::launcher::{self, Workspace};
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use frontcap_recorder_protocol::{Call, LeaseToken, Op, Outcome, Reply, StatusReport};

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

// Fields drop in declaration order: the guard kills the processes before the workspace is deleted.
struct Session {
    _guard: support::Runtime,
    _tmp: tempfile::TempDir,
    ws: Workspace,
    id: LaunchId,
    client: RecorderClient,
    lease: LeaseToken,
}

impl Session {
    async fn launch(page: &str, rules: RuleSet, env: &[(String, String)]) -> Session {
        let addr = support::serve();
        let (_tmp, ws) = support::workspace();
        let mut req = support::launch_request("launch", &format!("http://{addr}/{page}"));
        req.rules = rules;
        let a = launcher::launch(&ws, req, &support::recorder_bin(), env).await.unwrap();
        let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        Session { _guard: guard, _tmp, ws: ws.clone(), id: a.launch_id, client: a.client, lease: a.lease }
    }

    /// `launch` at `url`, which `rules` answer.
    async fn launch_url(url: &str, rules: RuleSet, env: &[(String, String)]) -> Session {
        let (_tmp, ws) = support::workspace();
        let mut req = support::launch_request("launch", url);
        req.rules = rules;
        let a = launcher::launch(&ws, req, &support::recorder_bin(), env).await.unwrap();
        let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        Session { _guard: guard, _tmp, ws: ws.clone(), id: a.launch_id, client: a.client, lease: a.lease }
    }

    /// `launch`, with the windows the page opens enrolled as popup contexts.
    async fn launch_with_popups(page: &str, env: &[(String, String)]) -> Session {
        let addr = support::serve();
        let (_tmp, ws) = support::workspace();
        let mut req = support::launch_request("launch", &format!("http://{addr}/{page}"));
        req.profile.popups = frontcap_core::profile::PopupPolicy::Controlled;
        let a = launcher::launch(&ws, req, &support::recorder_bin(), env).await.unwrap();
        let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        Session { _guard: guard, _tmp, ws: ws.clone(), id: a.launch_id, client: a.client, lease: a.lease }
    }

    /// `launch` with `rules`, its windows enrolled as popup contexts.
    async fn launch_popups_with(page: &str, rules: RuleSet, env: &[(String, String)]) -> Session {
        let addr = support::serve();
        let (_tmp, ws) = support::workspace();
        let mut req = support::launch_request("launch", &format!("http://{addr}/{page}"));
        req.rules = rules;
        req.profile.popups = frontcap_core::profile::PopupPolicy::Controlled;
        let a = launcher::launch(&ws, req, &support::recorder_bin(), env).await.unwrap();
        let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        Session { _guard: guard, _tmp, ws: ws.clone(), id: a.launch_id, client: a.client, lease: a.lease }
    }

    async fn mutate(&mut self, id: &str, op: Op) -> Result<Reply, ClientError> {
        self.client.call(Call::Mutate { lease: Some(self.lease.clone()), request_id: rid(id), op }).await
    }

    async fn status(&mut self) -> StatusReport {
        let Reply::Status { status } = self.client.call(Call::Status).await.unwrap() else { panic!() };
        *status
    }

    async fn recover_and_reattach(&mut self) {
        launcher::recover(&self.ws, &self.id, &support::recorder_bin(), &[]).await.unwrap();
        let a = launcher::reattach(&self.ws, &self.id, rid("reattach")).await.unwrap();
        self.client = a.client;
        self.lease = a.lease;
    }

    /// Whether the page's `selector` has exactly `text`, read through a new watch's initial sample.
    async fn text_is(&mut self, n: &str, selector: &str, text: &str) -> bool {
        let op = Op::WatchAdd {
            name: WatchName::new(n).unwrap(),
            predicate: Predicate::TextEquals { target: Locator::Css { selector: selector.into() }, text: text.into() },
            context: None,
        };
        match self.mutate(n, op).await.unwrap() {
            Reply::Done { outcome: Outcome::Sample { value }, .. } => value == SampleValue::True,
            other => panic!("{other:?}"),
        }
    }

    async fn stop(mut self) {
        self.mutate("stop", Op::Stop).await.unwrap();
        support::wait_until_exited(&self.ws.runtime_dir(&self.id)).await;
    }
}

/// Launches with `FRONTCAP_FAULT=<id>@<point>`, runs `setup`, sends `op` as `id` (the recorder aborts),
/// relaunches the recorder, reattaches, and sends the same request again.
async fn fault_and_repeat(page: &str, rules: RuleSet, setup: Vec<Op>, id: &str, point: &str, op: Op) -> (Reply, Session) {
    let env = [("FRONTCAP_FAULT".to_string(), format!("{id}@{point}"))];
    let mut s = Session::launch(page, rules, &env).await;
    for (i, o) in setup.into_iter().enumerate() {
        s.mutate(&format!("setup-{i}"), o).await.unwrap();
    }
    let died = s.mutate(id, op.clone()).await;
    assert!(died.is_err(), "the recorder must die at {point}: {died:?}");
    s.recover_and_reattach().await;
    let again = s.mutate(id, op).await.expect("a definite answer");
    (again, s)
}

fn latency_rules() -> RuleSet {
    RuleSet::from_json(
        r#"[{"id":"slow","match":{"url":"https://api.example.test/slow"},
             "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"slow","latency_ms":500}}]"#,
    )
    .unwrap()
}

#[tokio::test]
async fn next_event_recovery_keeps_the_arm_trigger_and_absolute_budget() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for (event, setup_us, expected_us) in [(ProgressEvent::Response, 51_000, 600_000), (ProgressEvent::Presentation, 600_000, 601_000)] {
        for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
            let op = Op::Next { event, max_us: 1_000_000 };
            let (again, mut s) = fault_and_repeat("latency.html", latency_rules(), vec![Op::Advance { by_us: setup_us }], "next", point, op).await;
            let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = again else { panic!("{event:?}@{point}: {again:?}") };
            assert!(matches!(result, ClockOutcome::Event { event: found, ref triggers } if found == event && !triggers.is_empty()), "{event:?}@{point}: {result:?}");
            assert_eq!(start.t_us.0, setup_us, "the original arm, not the reconnect boundary");
            assert_eq!(end.t_us.0, expected_us, "{event:?}@{point}");
            assert_eq!(s.status().await.t_us.0, expected_us);
            assert!(s.text_is("delivered-once", "#out", "slow@600").await);
            let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = s.mutate("no-next-response", Op::Next { event: ProgressEvent::Response, max_us: 100_000 }).await.unwrap() else { panic!() };
            assert_eq!(result, ClockOutcome::EventTimedOut { event: ProgressEvent::Response });
            assert_eq!(end.t_us.0 - start.t_us.0, 100_000);
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn navigation_event_recovery_keeps_the_committed_stop() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for navigation in ["history.pushState({}, '', '?arrived')", "location.href = '/latency.html?arrived'"] {
        for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
            let setup = vec![
                Op::Advance { by_us: 51_000 },
                Op::Evaluate { expression: format!("window.late=false; setTimeout(() => {{ {navigation}; }},199); setTimeout(() => {{window.late=true;}},200)") },
            ];
            let op = Op::Next { event: ProgressEvent::Navigation, max_us: 1_000_000 };
            let (again, mut s) = fault_and_repeat("latency.html", latency_rules(), setup, "next", point, op).await;
            let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = again else { panic!("{navigation}@{point}: {again:?}") };
            assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Navigation, .. }));
            assert_eq!(start.t_us.0, 51_000);
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("state", Op::Evaluate { expression: "({search:location.search,late:window.late===true,now:Date.now()-946684800000,appStarted:window.appStarted===true})".into() }).await.unwrap() else { panic!() };
            assert_eq!(end.t_us.0, 250_000, "{navigation}@{point}: {result:?}, {value}");
            assert_eq!(value, serde_json::json!({"search":"?arrived","late":false,"now":250,"appStarted":navigation.starts_with("history")}), "{navigation}@{point}");
            let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } = s.mutate("no-next-navigation", Op::Next { event: ProgressEvent::Navigation, max_us: 100_000 }).await.unwrap() else { panic!() };
            assert_eq!(result, ClockOutcome::EventTimedOut { event: ProgressEvent::Navigation });
            assert_eq!(end.t_us.0, 350_000);
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("resumed", Op::Evaluate { expression: "window.appStarted===true".into() }).await.unwrap() else { panic!() };
            assert_eq!(value, true, "the next explicit grant releases the committed loader: {navigation}@{point}");
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn advance_killed_before_dispatch_is_not_applied() {
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "m-1", "before_dispatch", Op::Advance { by_us: 1_000_000 }).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{again:?}");
    assert_eq!(s.status().await.t_us.0, 0);
    s.stop().await;
}

#[tokio::test]
async fn advance_killed_after_effect_completes_to_the_goal() {
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "m-1", "after_effect", Op::Advance { by_us: 1_000_000 }).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. } if t_us.0 == 1_000_000), "{again:?}");
    assert_eq!(s.status().await.t_us.0, 1_000_000, "never advanced twice");
    s.stop().await;
}

#[tokio::test]
async fn advance_killed_after_complete_returns_the_recorded_result() {
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "m-1", "after_complete", Op::Advance { by_us: 1_000_000 }).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. } if t_us.0 == 1_000_000), "{again:?}");
    assert_eq!(s.status().await.t_us.0, 1_000_000);
    s.stop().await;
}

#[tokio::test]
async fn watch_add_killed_after_effect_is_completed_once() {
    let name = WatchName::new("late").unwrap();
    let op = Op::WatchAdd { name: name.clone(), predicate: Predicate::Attached { target: Locator::Css { selector: ".late".into() } }, context: None };
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "w-1", "after_effect", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Sample { .. }, .. }), "{again:?}");
    assert_eq!(s.status().await.watches, vec![name.clone()]);
    let until = s.mutate("u-1", Op::Until { watch: name, max_us: 2_000_000 }).await.unwrap();
    assert!(matches!(until, Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. } if t_us.0 == 1_234_000), "{until:?}");
    s.stop().await;
}

#[tokio::test]
async fn click_killed_between_press_and_release_clicks_once() {
    let op = Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    // The mouse moves to the button (substep 0), presses (1) and releases (2).
    let (again, mut s) = fault_and_repeat("count.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "c-1", "after_substep_1", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("once", "#count", "1").await, "clicked exactly once");
    s.stop().await;
}

#[tokio::test]
async fn click_killed_after_effect_is_not_dispatched_again() {
    let op = Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    let (again, mut s) = fault_and_repeat("count.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "c-1", "after_effect", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("once", "#count", "1").await, "clicked exactly once");
    s.stop().await;
}

#[tokio::test]
async fn fault_during_latency_delivers_once() {
    let (again, mut s) = fault_and_repeat("latency.html", latency_rules(), vec![], "m-1", "after_effect", Op::Advance { by_us: 300_000 }).await;
    assert!(matches!(again, Reply::Done { .. }), "{again:?}");
    s.mutate("m-2", Op::Advance { by_us: 1_000_000 }).await.unwrap();
    assert!(s.text_is("out", "#out", "slow@600").await, "delivered once at issue (100) + 500");
    s.stop().await;
}

fn rules(json: &str) -> RuleSet {
    RuleSet::from_json(json).unwrap()
}

/// The page's `#out`, read through a new watch that reports whether it equals `text`.
async fn out_is(s: &mut Session, n: &str, text: &str) -> bool {
    s.text_is(n, "#out", text).await
}

/// Request records of `url` in the bundle's trace.
async fn requests_to(s: &mut Session, url: &str) -> usize {
    let trace = bundle_trace(s).await;
    std::fs::read_to_string(trace).unwrap().lines().filter(|l| l.contains("\"request\"") && l.contains(url)).count()
}

#[tokio::test]
async fn a_held_request_survives_a_recorder_fault_and_answers_once() {
    let hold = rules(r#"[{"id":"hold","match":{"url":"https://api.example.test/hold"},"action":{"type":"hold"}}]"#);
    let (again, mut s) = fault_and_repeat("adopt.html?at100=a/hold", hold, vec![], "m-1", "after_effect", Op::Advance { by_us: 300_000 }).await;
    assert!(matches!(again, Reply::Done { .. }), "{again:?}");
    let held = s.status().await.held;
    assert_eq!(held.len(), 1, "the hold is adopted under its ordinal: {held:?}");
    let response = frontcap_core::rules::MockSpec { status: 200, headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(), body: "held".into(), body_base64: None, latency_ms: 0 };
    s.mutate("r-1", Op::Respond { ordinal: held[0].clone(), response }).await.unwrap();
    s.mutate("m-2", Op::Advance { by_us: 100_000 }).await.unwrap();
    assert!(out_is(&mut s, "out", "a:held@300").await, "answered once, when the agent responded");
    assert!(s.status().await.held.is_empty());
    s.stop().await;
}

#[tokio::test]
async fn a_sequence_step_is_not_consumed_twice_across_a_fault() {
    let seq = rules(r#"[{"id":"seq","match":{"url":"https://api.example.test/seq"},"action":{"type":"sequence","steps":[
        {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"first","latency_ms":500},
        {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"second"}],"after_last":"fail"}}]"#);
    let (again, mut s) = fault_and_repeat("adopt.html?at100=a/seq&at700=b/seq", seq, vec![], "m-1", "after_effect", Op::Advance { by_us: 300_000 }).await;
    assert!(matches!(again, Reply::Done { .. }), "{again:?}");
    s.mutate("m-2", Op::Advance { by_us: 1_000_000 }).await.unwrap();
    assert!(out_is(&mut s, "out", "a:first@600,b:second@700").await, "the delayed step is delivered at its deadline and the next request gets the next step");
    s.stop().await;
}

#[tokio::test]
async fn identical_requests_keep_their_ordinals_across_a_fault() {
    let hold = rules(r#"[{"id":"hold","match":{"url":"https://api.example.test/same"},"action":{"type":"hold"}}]"#);
    let (again, mut s) = fault_and_repeat("adopt.html?at100=a/same/x,b/same/x", hold, vec![], "m-1", "after_effect", Op::Advance { by_us: 300_000 }).await;
    assert!(matches!(again, Reply::Done { .. }), "{again:?}");
    let mut held = s.status().await.held;
    held.sort_by_key(|o| o.trim_start_matches("req-").parse::<u64>().unwrap());
    assert_eq!(held.len(), 2, "{held:?}");
    let response = frontcap_core::rules::MockSpec { status: 200, headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(), body: "two".into(), body_base64: None, latency_ms: 0 };
    s.mutate("r-1", Op::Respond { ordinal: held[1].clone(), response }).await.unwrap();
    s.mutate("m-2", Op::Advance { by_us: 50_000 }).await.unwrap();
    assert!(out_is(&mut s, "out", "b:two@300").await, "the later-issued request, not the first with the same URL and body, is answered");
    s.stop().await;
}

#[tokio::test]
async fn a_second_fault_before_progress_adopts_again_once() {
    let env = [("FRONTCAP_FAULT".to_string(), "m-1@after_effect".to_string())];
    let mut s = Session::launch("latency.html", latency_rules(), &env).await;
    assert!(s.mutate("m-1", Op::Advance { by_us: 300_000 }).await.is_err());
    launcher::recover(&s.ws, &s.id, &support::recorder_bin(), &[("FRONTCAP_FAULT".to_string(), "w-1@after_effect".to_string())]).await.unwrap();
    let a = launcher::reattach(&s.ws, &s.id, rid("reattach")).await.unwrap();
    (s.client, s.lease) = (a.client, a.lease);
    let watch = Op::WatchAdd { name: WatchName::new("w").unwrap(), predicate: Predicate::Attached { target: Locator::Css { selector: "#out".into() } }, context: None };
    assert!(s.mutate("w-1", watch).await.is_err(), "the relaunched recorder dies too");
    s.recover_and_reattach().await;
    s.mutate("m-2", Op::Advance { by_us: 1_000_000 }).await.unwrap();
    assert!(s.text_is("out", "#out", "slow@600").await, "delivered once at its deadline");
    assert_eq!(requests_to(&mut s, "https://api.example.test/slow").await, 1, "its request record is written once");
    s.stop().await;
}

fn held_answer() -> frontcap_core::rules::MockSpec {
    frontcap_core::rules::MockSpec { status: 200, headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(), body: "held".into(), body_base64: None, latency_ms: 0 }
}

/// Holds `adopt.html`'s request, then sends the agent's answer as `r-1`, the recorder dying at `point`.
async fn respond_killed_at(point: &str) -> (Reply, Session, String) {
    let hold = rules(r#"[{"id":"hold","match":{"url":"https://api.example.test/hold"},"action":{"type":"hold"}}]"#);
    let env = [("FRONTCAP_FAULT".to_string(), format!("r-1@{point}"))];
    let mut s = Session::launch("adopt.html?at100=a/hold", hold, &env).await;
    s.mutate("m-1", Op::Advance { by_us: 300_000 }).await.unwrap();
    let ordinal = s.status().await.held.pop().unwrap();
    let respond = Op::Respond { ordinal: ordinal.clone(), response: held_answer() };
    assert!(s.mutate("r-1", respond.clone()).await.is_err(), "the recorder dies at {point}");
    s.recover_and_reattach().await;
    let again = s.mutate("r-1", respond).await.expect("a definite answer");
    (again, s, ordinal)
}

#[tokio::test]
async fn a_respond_killed_before_the_engine_answered_is_not_applied_and_the_hold_stays() {
    let (again, mut s, ordinal) = respond_killed_at("after_dispatch").await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{again:?}");
    assert_eq!(s.status().await.held, vec![ordinal.clone()], "the hold survives for the agent's next answer");
    s.mutate("r-2", Op::Respond { ordinal, response: held_answer() }).await.unwrap();
    s.mutate("m-2", Op::Advance { by_us: 17_000 }).await.unwrap();
    assert!(out_is(&mut s, "out", "a:held@300").await);
    s.stop().await;
}

#[tokio::test]
async fn a_respond_killed_after_the_engine_answered_completes_once() {
    let (again, mut s, _) = respond_killed_at("after_engine").await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Ack, .. }), "{again:?}");
    assert!(s.status().await.held.is_empty());
    s.mutate("m-2", Op::Advance { by_us: 17_000 }).await.unwrap();
    assert!(out_is(&mut s, "out", "a:held@300").await, "answered once");
    s.stop().await;
}

#[tokio::test]
async fn two_controllers_recovering_at_once_resume_a_click_once() {
    let op = Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    let env = [("FRONTCAP_FAULT".to_string(), "c-1@after_substep_1".to_string())];
    let mut s = Session::launch("count.html", RuleSet::default(), &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    assert!(s.mutate("c-1", op.clone()).await.is_err());
    let bin = support::recorder_bin();
    let (a, b) = tokio::join!(launcher::recover(&s.ws, &s.id, &bin, &[]), launcher::recover(&s.ws, &s.id, &bin, &[]));
    a.unwrap();
    b.unwrap();
    let attached = launcher::reattach(&s.ws, &s.id, rid("reattach")).await.unwrap();
    s.client = attached.client;
    s.lease = attached.lease;
    let again = s.mutate("c-1", op).await.unwrap();
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("once", "#count", "1").await, "clicked exactly once");
    let journal = std::fs::read_to_string(s.ws.runtime_dir(&s.id).join("journal.jsonl")).unwrap();
    let completions = journal.lines().filter(|l| l.contains("\"c-1\"") && l.contains("completed")).count();
    assert_eq!(completions, 1, "{journal}");
    s.stop().await;
}

fn late_watch() -> (WatchName, Op) {
    let name = WatchName::new("late").unwrap();
    let op = Op::WatchAdd { name: name.clone(), predicate: Predicate::Attached { target: Locator::Css { selector: ".late".into() } }, context: None };
    (name, op)
}

#[tokio::test]
async fn watch_add_killed_before_dispatch_is_not_applied() {
    let (_, op) = late_watch();
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "w-1", "before_dispatch", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{again:?}");
    assert!(s.status().await.watches.is_empty());
    s.stop().await;
}

#[tokio::test]
async fn watch_add_killed_after_complete_returns_the_recorded_sample() {
    let (name, op) = late_watch();
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "w-1", "after_complete", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Sample { .. }, .. }), "{again:?}");
    assert_eq!(s.status().await.watches, vec![name]);
    s.stop().await;
}

#[tokio::test]
async fn watch_remove_killed_after_effect_is_completed_once() {
    let (name, add) = late_watch();
    let remove = Op::WatchRemove { name: name.clone() };
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![add], "w-2", "after_effect", remove).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Sample { .. }, .. }), "{again:?}");
    assert!(s.status().await.watches.is_empty());
    // The page no longer reports the watch: an until on a new name is unaffected, and re-adding works.
    let (_, add_again) = late_watch();
    assert!(matches!(s.mutate("w-3", add_again).await.unwrap(), Reply::Done { outcome: Outcome::Sample { .. }, .. }));
    s.stop().await;
}

#[tokio::test]
async fn click_killed_before_dispatch_is_not_applied() {
    let op = Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    let (again, mut s) = fault_and_repeat("count.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "c-1", "before_dispatch", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{again:?}");
    let result = s.client.call(Call::Result { request_id: rid("c-1") }).await.unwrap();
    assert!(matches!(result, Reply::Result { result: frontcap_recorder_protocol::RequestResult::NotApplied, .. }), "{result:?}");
    assert!(s.text_is("none", "#count", "0").await, "never clicked");
    s.stop().await;
}

#[tokio::test]
async fn click_killed_after_complete_returns_the_recorded_result() {
    let op = Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    let (again, mut s) = fault_and_repeat("count.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "c-1", "after_complete", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("once", "#count", "1").await, "clicked exactly once");
    s.stop().await;
}

#[tokio::test]
async fn until_killed_after_effect_answers_the_instant_the_condition_held() {
    let (name, add) = late_watch();
    let op = Op::Until { watch: name, max_us: 5_000_000 };
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![add], "u-1", "after_effect", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Clock { t_us, .. }, .. } if t_us.0 == 1_234_000), "{again:?}");
    assert_eq!(s.status().await.t_us.0, 1_234_000, "no time passed after the condition held");
    s.stop().await;
}

#[tokio::test]
async fn settle_killed_after_effect_completes_with_a_settle_result() {
    let op = Op::Settle { idle_us: 250_000, max_us: 1_000_000 };
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "s-1", "after_effect", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Settled { .. }, .. }), "{again:?}");
    assert!(s.status().await.t_us.0 <= 1_000_000);
    s.stop().await;
}

#[tokio::test]
async fn refs_survive_a_recorder_relaunch() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    let env = [("FRONTCAP_FAULT".to_string(), "m-1@after_effect".to_string())];
    let mut s = Session::launch("count.html", RuleSet::default(), &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    let Reply::Observed { result: Observation::Snapshot(snap) } = s.client.call(Call::Observe { op: ObserveOp::Snapshot { scope: None, depth: None, max_nodes: None, geometry: false, unfold: true }, context: None }).await.unwrap() else { panic!() };
    let ok = snap.nodes.iter().find(|n| n.role == "button").expect("the button").r#ref.clone();
    assert!(s.mutate("m-1", Op::Advance { by_us: 10_000 }).await.is_err(), "the recorder dies");
    s.recover_and_reattach().await;
    let clicked = s.mutate("c-1", Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Ref { id: ok } }, context: None }).await.unwrap();
    assert!(matches!(clicked, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{clicked:?}");
    assert!(s.text_is("once", "#count", "1").await);
    s.stop().await;
}

/// What a request's answer and evidence look like, without the sequence numbers a relaunch shifts.
#[derive(Debug, PartialEq)]
struct Shape {
    outcome: String,
    end_us: u64,
    actions: usize,
    settle_ends: usize,
}

fn shape_of(reply: &Reply, trace: &std::path::Path, request: &str) -> Shape {
    use frontcap_core::event::{ClockOp, ClockPhase, EventKind};
    let Reply::Done { outcome, .. } = reply else { panic!("{reply:?}") };
    let outcome = match outcome {
        Outcome::Acted { start, settle, end, .. } => {
            format!("acted {}→{} dom+{} {settle:?}", start.t_us.0, end.t_us.0, end.dom_mutations.saturating_sub(start.dom_mutations))
        }
        Outcome::Settled { result, end, .. } => format!("settled {result:?} at {}", end.t_us.0),
        other => format!("{other:?}"),
    };
    let events = frontcap_core::log::BehaviorLog::read(trace).unwrap();
    Shape {
        outcome,
        end_us: events.iter().rev().find(|e| !matches!(e.kind, EventKind::Taint(_) | EventKind::Controller(_))).unwrap().t_us.0,
        actions: events.iter().filter(|e| matches!(&e.kind, EventKind::Action(a) if a.request_id.as_str() == request)).count(),
        settle_ends: events.iter().filter(|e| matches!(&e.kind, EventKind::Clock(c) if c.op == ClockOp::Settle && c.phase == ClockPhase::End)).count(),
    }
}

async fn bundle_trace(s: &mut Session) -> std::path::PathBuf {
    s.status().await.bundle.join("trace.jsonl")
}

#[tokio::test]
async fn faulted_requests_answer_and_record_as_unfaulted_ones() {
    let click = || Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None };
    let act = || Op::Act { action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, then: Some(frontcap_recorder_protocol::SettleSpec { idle_us: 250_000, max_us: 1_000_000 }), context: None };
    let settle = || Op::Settle { idle_us: 250_000, max_us: 1_000_000 };
    let q = || Locator::Css { selector: "#q".into() };
    let typing = || Op::Act { action: frontcap_core::action::Action::Type { target: q(), text: "hey".into() }, then: None, context: None };
    let filling = || Op::Act { action: frontcap_core::action::Action::Fill { target: q(), value: "full".into() }, then: None, context: None };
    let cases: Vec<(&str, Op, &str, &str)> = vec![
        ("click", click(), "after_effect", "count.html"),
        ("act", act(), "after_effect", "count.html"),
        ("act", act(), "after_substep_2", "count.html"),
        ("act", act(), "after_action", "count.html"),
        ("settle", settle(), "after_effect", "count.html"),
        ("advance", Op::Advance { by_us: 300_000 }, "after_effect", "count.html"),
        ("type", typing(), "after_effect", "type.html"),
        ("fill", filling(), "after_substep_1", "type.html"),
    ];
    for (name, op, point, page) in cases {
        let mut plain = Session::launch(page, RuleSet::default(), &[]).await;
        plain.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        let reply = plain.mutate("p-1", op.clone()).await.unwrap();
        let expected = shape_of(&reply, &bundle_trace(&mut plain).await, "p-1");
        plain.stop().await;

        let env = [("FRONTCAP_FAULT".to_string(), format!("p-1@{point}"))];
        let mut faulted = Session::launch(page, RuleSet::default(), &env).await;
        faulted.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        assert!(faulted.mutate("p-1", op.clone()).await.is_err(), "{name}: the recorder dies at {point}");
        faulted.recover_and_reattach().await;
        let reply = faulted.mutate("p-1", op).await.unwrap();
        let got = shape_of(&reply, &bundle_trace(&mut faulted).await, "p-1");
        assert_eq!(got, expected, "{name} killed at {point}");
        faulted.stop().await;
    }
}

/// A settle interrupted part-way resumes against its original budget and quiet window, so it ends where
/// the uninterrupted one does instead of granting the budget again from the relaunch.
#[tokio::test]
async fn a_settle_interrupted_part_way_keeps_its_budget() {
    let target = || Locator::Css { selector: "#ok".into() };
    let act = || Op::Act { action: frontcap_core::action::Action::Click { target: target() }, then: Some(frontcap_recorder_protocol::SettleSpec { idle_us: 250_000, max_us: 1_000_000 }), context: None };
    let settle = || Op::Settle { idle_us: 250_000, max_us: 1_000_000 };
    for (name, op) in [("act", act()), ("settle", settle())] {
        let setup = |s: &'static str| (s, Op::Act { then: None, action: frontcap_core::action::Action::Click { target: target() }, context: None });
        let mut plain = Session::launch("busy-click.html", RuleSet::default(), &[]).await;
        plain.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        if name == "settle" {
            let (id, click) = setup("start");
            plain.mutate(id, click).await.unwrap();
        }
        let reply = plain.mutate("p-1", op.clone()).await.unwrap();
        let expected = shape_of(&reply, &bundle_trace(&mut plain).await, "p-1");
        plain.stop().await;

        let env = [("FRONTCAP_FAULT".to_string(), "p-1@settle_step_2".to_string())];
        let mut faulted = Session::launch("busy-click.html", RuleSet::default(), &env).await;
        faulted.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        if name == "settle" {
            let (id, click) = setup("start");
            faulted.mutate(id, click).await.unwrap();
        }
        assert!(faulted.mutate("p-1", op.clone()).await.is_err(), "{name}: the recorder dies in the settle");
        faulted.recover_and_reattach().await;
        let reply = faulted.mutate("p-1", op).await.unwrap();
        let got = shape_of(&reply, &bundle_trace(&mut faulted).await, "p-1");
        assert_eq!(got.outcome, expected.outcome, "{name} killed part-way through its settle");
        faulted.stop().await;
    }
}

fn act(action: frontcap_core::action::Action) -> Op {
    Op::Act { action, then: None, context: None }
}

/// `type` is one substep per key event: a fault in the middle resumes after the last key the page
/// received, so every character arrives exactly once.
#[tokio::test]
async fn type_killed_midway_types_every_character_once() {
    use frontcap_core::action::Action;
    let op = act(Action::Type { target: Locator::Css { selector: "#q".into() }, text: "abcdef".into() });
    // Focus, then a down/up pair per character: substep 5 is the key-down of "c".
    let (again, mut s) = fault_and_repeat("type.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "t-1", "after_substep_5", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("value", "#value", "abcdef").await, "every character once");
    assert!(s.text_is("keys", "#keys", "abcdef").await, "every key-down once");
    s.stop().await;
}

/// A navigation's evidence is the document it replaced: interrupted after dispatch, its outcome needs
/// engine patch 4 and is answered as a recorder fault.
/// A navigation whose dispatch was journaled but never sent (no paused document request, no commit) is
/// sent by the recovery: the page navigates exactly once.
#[tokio::test]
async fn a_navigation_interrupted_after_dispatch_navigates_once() {
    use frontcap_core::action::Action;
    let env = [("FRONTCAP_FAULT".to_string(), "n-1@after_dispatch".to_string())];
    let mut s = Session::launch("loads.html", RuleSet::default(), &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    assert!(s.mutate("n-1", act(Action::Reload {})).await.is_err(), "the recorder dies after dispatch");
    s.recover_and_reattach().await;
    let again = s.mutate("n-1", act(Action::Reload {})).await.expect("a definite answer");
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    s.mutate("m-1", Op::Advance { by_us: 17_000 }).await.unwrap();
    assert!(s.text_is("reloaded", "#loads", "loads:2").await, "reloaded once");
    s.stop().await;
}

/// The recorder can be lost after it answered the navigation's document request and before the commit
/// reaches the trace: the action is not recorded yet and no request is retained. The browser has the
/// navigation, so the recovery completes it instead of sending it again. The document is mocked: a
/// pass-through exchange would lose its connection with the recorder's proxy.
#[tokio::test]
async fn a_navigation_lost_after_its_document_request_navigates_once() {
    use frontcap_core::action::Action;
    use frontcap_core::event::EventKind;
    let env = [("FRONTCAP_INTERCEPTION_FAULT".to_string(), "after_document_answer:/loads.html?again".to_string())];
    let page = format!("http://{}/loads.html", support::serve());
    let counted = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/loads.html")).unwrap();
    let rules = RuleSet::from_json(&serde_json::json!([{"id": "again", "match": {"url": "http://127.0.0.1:*/loads.html?again"},
        "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": counted}}]).to_string()).unwrap();
    let mut s = Session::launch_url(&page, rules, &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    let target = format!("{page}?again");
    let navigate = act(Action::Navigate { url: target.clone() });
    assert!(s.mutate("n-1", navigate.clone()).await.is_err(), "the recorder dies after the document request");
    s.recover_and_reattach().await;
    let again = s.mutate("n-1", navigate).await.expect("a definite answer");
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    s.mutate("m-1", Op::Advance { by_us: 17_000 }).await.unwrap();
    assert!(s.text_is("loaded", "#loads", "loads:2").await, "navigated once");
    let trace = frontcap_core::log::BehaviorLog::read(&bundle_trace(&mut s).await).unwrap();
    let documents = trace.iter().filter(|e| matches!(&e.kind, EventKind::Request(r) if r.url == target)).count();
    let commits = trace.iter().filter(|e| matches!(&e.kind, EventKind::Navigation(n) if n.url == target)).count();
    assert_eq!((documents, commits), (1, 1), "{:#?}", trace.iter().filter(|e| matches!(&e.kind,
        EventKind::Action(_) | EventKind::Navigation(_) | EventKind::Request(_) | EventKind::Response(_))).collect::<Vec<_>>());
    s.stop().await;
}

/// A popup's close is recorded before it is sent: interrupted before that, the recovery sends it; after
/// the window closed, the recovery finds it gone. Either way its unload handlers run once.
#[tokio::test]
async fn a_popup_close_interrupted_by_recorder_loss_closes_it_once() {
    use frontcap_core::action::Action;
    for point in ["after_dispatch", "after_close"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("x-1@{point}"))];
        let mut s = Session::launch_with_popups("popup-close.html", &env).await;
        s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        s.mutate("c-1", act(Action::Click { target: Locator::Css { selector: "#open".into() } })).await.unwrap();
        s.mutate("setup-2", Op::Advance { by_us: 50_000 }).await.unwrap();
        let close = Op::Act { action: Action::Close {}, then: None, context: Some("popup-1".into()) };
        assert!(s.mutate("x-1", close.clone()).await.is_err(), "the recorder dies at {point}");
        s.recover_and_reattach().await;
        let again = s.mutate("x-1", close).await.expect("a definite answer");
        assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{point}: {again:?}");
        s.mutate("m-1", Op::Advance { by_us: 17_000 }).await.unwrap();
        assert!(s.text_is("closed", "#status", "closings:1").await, "{point}: closed once");
        s.stop().await;
    }
}

/// Once the action itself has completed, an interrupted settle after it resumes as a clock request.
#[tokio::test]
async fn a_navigation_whose_settle_was_interrupted_resumes_the_settle() {
    use frontcap_core::action::Action;
    let op = Op::Act { action: Action::Reload {}, then: Some(frontcap_recorder_protocol::SettleSpec { idle_us: 100_000, max_us: 1_000_000 }), context: None };
    let (again, mut s) = fault_and_repeat("type.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "n-2", "settle_step_1", op).await;
    let Reply::Done { outcome: Outcome::Acted { settle: Some(settle), .. }, .. } = &again else { panic!("{again:?}") };
    assert!(matches!(settle, frontcap_core::clock::SettleOutcome::Quiescent { .. }), "{settle:?}");
    assert!(s.text_is("fresh", "#value", "").await);
    s.stop().await;
}

#[tokio::test]
async fn wait_actionable_killed_after_effect_answers_the_instant_it_held() {
    use frontcap_core::action::Action;
    let op = act(Action::WaitActionable { target: Locator::Css { selector: ".late".into() }, max_us: 5_000_000 });
    let (again, mut s) = fault_and_repeat("appear.html", RuleSet::default(), vec![], "w-1", "after_effect", op).await;
    let Reply::Done { outcome: Outcome::Acted { waited: Some(frontcap_core::clock::WaitOutcome::Actionable { t }), .. }, .. } = &again else { panic!("{again:?}") };
    assert!((1_234_000..=1_250_667).contains(&t.0), "{t:?}");
    assert_eq!(s.status().await.t_us, *t);
    s.stop().await;
}

/// Inserted text (a character outside the key table) is a substep with its own receipt: a fault right
/// after it resumes with the next character, so nothing is inserted twice.
#[tokio::test]
async fn type_killed_after_inserted_text_inserts_it_once() {
    use frontcap_core::action::Action;
    let op = act(Action::Type { target: Locator::Css { selector: "#q".into() }, text: "a한😀b".into() });
    // Focus, a down/up, then the inserted 한 (substep 3).
    let (again, mut s) = fault_and_repeat("type.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "t-1", "after_substep_3", op).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Acted { .. }, .. }), "{again:?}");
    assert!(s.text_is("value", "#value", "a한😀b").await, "every character once");
    s.stop().await;
}

/// What the arguments and the page decide alone is refused before the request is dispatched: a `back`
/// with no earlier entry never reaches the fault point after dispatch.
#[tokio::test]
async fn back_without_history_is_refused_before_dispatch() {
    use frontcap_core::action::Action;
    let env = [("FRONTCAP_FAULT".to_string(), "n-1@after_dispatch".to_string())];
    let mut s = Session::launch("type.html", RuleSet::default(), &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    match s.mutate("n-1", act(Action::Back {})).await {
        Err(ClientError::Recorder(e)) => assert_eq!(e.code, frontcap_recorder_protocol::ErrorCode::InvalidArguments, "{e:?}"),
        other => panic!("refused, with the recorder alive: {other:?}"),
    }
    s.stop().await;
}

/// Recording is recorder state: started, and the recorder killed after the change, a relaunched recorder
/// goes on recording; a screenshot reads the last frame through the recorder.
#[tokio::test]
async fn recording_survives_a_recorder_fault_and_screenshots_read_the_last_frame() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    let (again, mut s) = fault_and_repeat("color.html", RuleSet::default(), vec![Op::Advance { by_us: 10_000 }], "r-1", "after_effect", Op::RecordStart).await;
    assert!(matches!(again, Reply::Done { outcome: Outcome::Ack, .. }), "{again:?}");
    s.mutate("a-1", Op::Advance { by_us: 50_000 }).await.unwrap();
    let Reply::Observed { result: Observation::Screenshot(shot) } = s.client.call(Call::Observe { op: ObserveOp::Screenshot {}, context: None }).await.unwrap() else { panic!() };
    assert_eq!(shot.t_us.0, 60_000);
    let Reply::Done { outcome: Outcome::Recorded(rec), .. } = s.mutate("r-2", Op::RecordStop).await.unwrap() else { panic!() };
    // A recorded frame is at the instant its state was drawn: the tick at 51 ms on the patched engine.
    let patched = browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some();
    let drawn = if patched { 51_000 } else { 60_000 };
    assert!(rec.frames.iter().any(|f| f.t_us.0 == drawn), "{rec:?}");
    let bundle = s.status().await.bundle;
    s.mutate("stop", Op::Stop).await.unwrap();
    support::wait_until_exited(&s.ws.runtime_dir(&s.id)).await;
    // A recorder fault makes the run not replayable (trace-bundle.md, Re-execution inputs).
    let manifest: frontcap_core::bundle::Manifest = serde_json::from_str(&std::fs::read_to_string(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!((manifest.replayable, manifest.missing_inputs), (false, vec!["recorder_fault".to_string()]));
}

/// The existing surface must be readable immediately after a real process fault,
/// including a pending image whose paint predates the recovered clock boundary.
#[tokio::test]
async fn presentation_survives_a_recorder_fault_without_progress_or_redraw() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    for pending in [false, true] {
        let env = [("FRONTCAP_FAULT".to_string(), "advance@after_effect".to_string())];
        let mut s = Session::launch("presentation_recovery.html", RuleSet::default(), &env).await;
        s.mutate("paint", Op::Advance { by_us: 201_000 }).await.unwrap();
        if pending {
            s.mutate("change", act(frontcap_core::action::Action::Click { target: Locator::Css { selector: "#change".into() } })).await.unwrap();
        }
        let Reply::Observed { result: Observation::Screenshot(before) } = s.client.call(Call::Observe { op: ObserveOp::Screenshot {}, context: None }).await.unwrap() else { panic!() };
        assert_eq!(before.current, !pending);
        let pixels = std::fs::read(&before.path).unwrap();
        assert!(s.mutate("advance", Op::Advance { by_us: 5_000 }).await.is_err());
        s.recover_and_reattach().await;
        assert_eq!(s.status().await.t_us.0, 206_000);
        for _ in 0..2 {
            let Reply::Observed { result: Observation::Screenshot(after) } = s.client.call(Call::Observe { op: ObserveOp::Screenshot {}, context: None }).await.unwrap() else { panic!() };
            assert_eq!(after.current, !pending);
            assert_eq!(after.document, before.document);
            assert_eq!(after.t_us.0, if pending { before.t_us.0 } else { 206_000 });
            assert_eq!(std::fs::read(&after.path).unwrap(), pixels);
            assert_eq!(s.status().await.t_us.0, 206_000);
        }
        s.stop().await;
    }
}

/// Criterion 7 for a drag: killed after a held move, the repeat goes on from the next step — no move
/// is sent twice, and the release comes.
#[tokio::test]
async fn drag_killed_mid_way_moves_each_step_once_and_releases() {
    use frontcap_core::action::{Action, Offset};
    let op = Op::Act { then: None, action: Action::Drag { target: Locator::Css { selector: "#handle".into() }, to: None, offset: Some(Offset { dx: 100, dy: 0 }), steps: Some(4), interval_us: 0 }, context: None };
    // Steps: hover, press, four frozen moves, release; 3 is the second move.
    let (again, mut s) = fault_and_repeat("drag.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "d-1", "after_substep_3", op).await;
    assert!(matches!(&again, Reply::Done { outcome: Outcome::Acted { end, .. }, .. } if end.t_us.0 == 50_000), "{again:?}");
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } =
        s.mutate("e-1", Op::Evaluate { expression: "JSON.stringify(log.filter(e => e[0] !== 'pointermove' || e[3] === 1).map(e => e[0] + ':' + e[1]))".into() }).await.unwrap()
    else {
        panic!()
    };
    let events: Vec<String> = serde_json::from_str(value.as_str().unwrap()).unwrap();
    assert_eq!(events, ["pointerdown:40", "pointermove:65", "pointermove:90", "pointermove:115", "pointermove:140", "pointerup:140"]);
    s.stop().await;
}

/// Killed after explicit progress: the repeat finishes no interval twice and moves each step once.
#[tokio::test]
async fn drag_killed_after_explicit_progress_moves_each_step_once() {
    use frontcap_core::action::{Action, Offset};
    let op = Op::Act { then: None, action: Action::Drag { target: Locator::Css { selector: "#handle".into() }, to: None, offset: Some(Offset { dx: 100, dy: 0 }), steps: Some(4), interval_us: 17_000 }, context: None };
    // Steps: hover, press, then (explicit advance, move) four times, release.
    for point in ["before_substep_2", "after_substep_2", "before_substep_4", "after_substep_4"] {
        let (again, mut s) = fault_and_repeat("drag.html", RuleSet::default(), vec![Op::Advance { by_us: 50_000 }], "d-1", point, op.clone()).await;
        assert!(matches!(&again, Reply::Done { outcome: Outcome::Acted { end, .. }, .. } if end.t_us.0 == 118_000), "{again:?}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } =
            s.mutate("e-1", Op::Evaluate { expression: "JSON.stringify(log.filter(e => e[0] !== 'pointermove' || e[3] === 1).map(e => e[0] + ':' + e[1]))".into() }).await.unwrap()
        else {
            panic!()
        };
        let events: Vec<String> = serde_json::from_str(value.as_str().unwrap()).unwrap();
        assert_eq!(events, ["pointerdown:40", "pointermove:65", "pointermove:90", "pointermove:115", "pointermove:140", "pointerup:140"]);
        s.stop().await;
    }
}

/// A drag in a popup, by its context with granted intervals or by a reference frozen: killed mid-way, the
/// repeat reads the window's receipts, finishes no interval twice and moves each step once there.
#[tokio::test]
async fn a_popup_drag_killed_mid_way_moves_each_step_once_in_its_window() {
    use frontcap_core::action::{Action, Offset};
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    let drag = |target: Locator, interval_us: u64, context: Option<&str>| Op::Act {
        then: None,
        action: Action::Drag { target, to: None, offset: Some(Offset { dx: 100, dy: 0 }), steps: Some(4), interval_us },
        context: context.map(str::to_string),
    };
    let cases = [("before_substep_2", true), ("after_substep_2", true), ("before_substep_4", true), ("after_substep_4", true), ("after_substep_3", false)];
    for (point, granted) in cases {
        let env = [("FRONTCAP_FAULT".to_string(), format!("d-1@{point}"))];
        let mut s = Session::launch_with_popups("popup-drag.html", &env).await;
        s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        s.mutate("c-1", act(Action::Click { target: Locator::Css { selector: "#open".into() } })).await.unwrap();
        s.mutate("setup-2", Op::Advance { by_us: 50_000 }).await.unwrap();
        let handle = Locator::Css { selector: "#handle".into() };
        let op = if granted {
            drag(handle, 17_000, Some("popup-1"))
        } else {
            let Reply::Observed { result: Observation::Query(node) } =
                s.client.call(Call::Observe { op: ObserveOp::Query { target: handle, props: vec![] }, context: Some("popup-1".into()) }).await.unwrap()
            else {
                panic!()
            };
            drag(Locator::Ref { id: node.r#ref }, 0, None)
        };
        assert!(s.mutate("d-1", op.clone()).await.is_err(), "the recorder dies at {point}");
        s.recover_and_reattach().await;
        let again = s.mutate("d-1", op).await.expect("a definite answer");
        let end_us = if granted { 168_000 } else { 100_000 };
        assert!(matches!(&again, Reply::Done { outcome: Outcome::Acted { end, .. }, .. } if end.t_us.0 == end_us), "{point}: {again:?}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } =
            s.mutate("e-1", Op::Evaluate { expression: "JSON.stringify(w.log.filter(e => e[0] !== 'pointermove' || e[3] === 1).map(e => e[0] + ':' + e[1]))".into() }).await.unwrap()
        else {
            panic!()
        };
        let events: Vec<String> = serde_json::from_str(value.as_str().unwrap()).unwrap();
        assert_eq!(events, ["pointerdown:40", "pointermove:65", "pointermove:90", "pointermove:115", "pointermove:140", "pointerup:140"], "{point}");
        s.stop().await;
    }
}

/// An HTML drag killed mid-way: the engine's drag ended with the recorder's session, so the repeat is a
/// definite recorder fault, the button is released, and the page works on.
#[tokio::test]
async fn an_html_drag_killed_mid_way_is_a_fault_that_releases_the_button() {
    use frontcap_core::action::Action;
    let op = Op::Act { then: None, action: Action::Drag { target: Locator::Css { selector: "#src".into() }, to: Some(Locator::Css { selector: "#dst".into() }), offset: None, steps: None, interval_us: 0 }, context: None };
    let env = [("FRONTCAP_FAULT".to_string(), "d-1@after_substep_6".to_string())];
    let mut s = Session::launch("drag.html", RuleSet::default(), &env).await;
    s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
    assert!(s.mutate("d-1", op.clone()).await.is_err());
    s.recover_and_reattach().await;
    match s.mutate("d-1", op).await {
        Err(ClientError::Recorder(e)) => assert_eq!(e.code, frontcap_recorder_protocol::ErrorCode::RecorderFault, "{e:?}"),
        other => panic!("{other:?}"),
    }
    let click = Op::Act { then: None, action: Action::Click { target: Locator::Css { selector: "#b".into() } }, context: None };
    s.mutate("c-1", click).await.unwrap();
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } =
        s.mutate("e-1", Op::Evaluate { expression: "log.filter(e => e[0] === 'click-b').length".into() }).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(value, serde_json::json!(1), "no drag holds the pointer");
    s.stop().await;
}

/// The browser keeps its proxy flags across a recorder fault: the relaunched recorder's proxy is on
/// the same port with the same authority, and the page's requests go on through it.
#[tokio::test]
async fn the_proxy_comes_back_with_the_recorder() {
    let (again, mut s) = fault_and_repeat("count.html", RuleSet::default(), vec![], "a-1", "after_effect", Op::Advance { by_us: 50_000 }).await;
    assert!(matches!(again, Reply::Done { .. }), "{again:?}");
    s.mutate("e-1", Op::Evaluate { expression: "window.st = 0; fetch('/appear.html').then((r) => { window.st = r.status; }); 0".into() }).await.unwrap();
    s.mutate("a-2", Op::Advance { by_us: 100_000 }).await.unwrap();
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("e-2", Op::Evaluate { expression: "window.st".into() }).await.unwrap() else { panic!() };
    assert_eq!(value, serde_json::json!(200), "a same-origin request after the relaunch");
    let trace = std::fs::read_to_string(s.status().await.bundle.join("trace.jsonl")).unwrap();
    assert!(!trace.contains("transport_bypass"), "{trace}");
    s.stop().await;
}

#[tokio::test]
async fn state_interventions_resume_without_repeating_custom_element_effects() {
    use frontcap_core::intervention::{StateChange, StateField};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    for point in ["after_dispatch", "state_before_write_0", "state_after_write_0", "state_after_receipt_0", "state_saved_0", "state_after_write_1", "state_after_receipt_1", "state_saved_1", "after_effect"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("state-change@{point}"))];
        let mut s = Session::launch("state-counter.html", RuleSet::default(), &env).await;
        s.mutate("load", Op::Advance { by_us: 50_000 }).await.unwrap();
        let target = Locator::Css { selector: "#widget".into() };
        let field = StateField::Attribute { name: "data-touch".into() };
        let Reply::Observed { result: Observation::State(read) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: target.clone(), field: field.clone() }, context: None }).await.unwrap() else { panic!() };
        let changes = ["first", "second"].into_iter().map(|value| StateChange { target: target.clone(), field: field.clone(), value: value.into(), expected: None }).collect();
        let op = Op::StateApply { boundary: read.boundary, changes, context: None, progress_us: None };
        assert!(s.mutate("state-change", op.clone()).await.is_err(), "fault must fire: {point}");
        s.recover_and_reattach().await;
        let result = s.mutate("state-change", op.clone()).await.unwrap();
        let Reply::Done { outcome: Outcome::StateApplied(applied), .. } = &result else { panic!("{point}: {result:?}") };
        assert!(applied.completed, "{point}: {applied:?}");
        assert_eq!(applied.receipts.len(), 2);
        assert_eq!(s.mutate("state-change", op).await.unwrap(), result);
        let Reply::Observed { result: Observation::State(calls) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: Locator::Css { selector: "#calls".into() }, field: StateField::Text }, context: None }).await.unwrap() else { panic!() };
        assert_eq!(calls.value, "2", "setter repeated at {point}");
        let trace = std::fs::read_to_string(s.ws.runs().join(launcher::run_id_of(&s.id).as_str()).join("trace.jsonl")).unwrap();
        assert_eq!(trace.lines().filter(|line| line.contains("\"kind\":\"intervention\"")).count(), 1, "duplicate intervention at {point}");
        s.stop().await;
    }
}

/// An app provider's write whose promise was pending when the recorder died settles in the relaunched
/// one by its token, and is not written again: at the same instant, or, given progress, by the frame tick
/// its timer ran by.
#[tokio::test]
async fn an_asynchronous_provider_write_settles_after_a_recorder_fault() {
    use frontcap_core::intervention::{StateChange, StateField};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    for (provider, value, progress_us, point) in [
        ("notes", serde_json::json!({"text": "ship it"}), None, "state_after_write_0"),
        ("notes", serde_json::json!({"text": "ship it"}), None, "state_after_receipt_0"),
        ("setting", serde_json::json!({"level": 3}), Some(500_000), "state_after_write_0"),
    ] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("write@{point}"))];
        let mut s = Session::launch("state-async.html", RuleSet::default(), &env).await;
        s.mutate("load", Op::Advance { by_us: 50_000 }).await.unwrap();
        let target = Locator::Document { selector: None };
        let field = StateField::App { provider: provider.into() };
        let Reply::Observed { result: Observation::State(read) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: target.clone(), field: field.clone() }, context: None }).await.unwrap() else { panic!() };
        let op = Op::StateApply { boundary: read.boundary, changes: vec![StateChange { target, field, value: value.clone(), expected: None }], context: None, progress_us };
        assert!(s.mutate("write", op.clone()).await.is_err(), "fault must fire: {provider} {point}");
        s.recover_and_reattach().await;
        let result = s.mutate("write", op).await.unwrap();
        let Reply::Done { outcome: Outcome::StateApplied(applied), .. } = &result else { panic!("{point}: {result:?}") };
        assert!(applied.completed, "{provider} {point}: {applied:?}");
        assert_eq!(applied.receipts[0].after, Some(value), "{provider} {point}");
        match progress_us {
            None => assert_eq!(applied.boundary.t_us.0, 50_000, "settled at its instant: {point}"),
            Some(_) => assert!((150_000..=167_000).contains(&applied.boundary.t_us.0), "by the tick after its timer: {applied:?}"),
        }
        assert!(s.text_is("one-write", "#writes", "1").await, "the provider wrote once: {provider} {point}");
        s.stop().await;
    }
}

/// A cookie write cut short by a recorder fault completes in the relaunched recorder: the browser's store
/// holds the cookie once, read back as written.
#[tokio::test]
async fn a_cookie_write_completes_after_a_recorder_fault() {
    use frontcap_core::intervention::{StateChange, StateField};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    for point in ["state_before_write_0", "state_after_write_0", "state_after_receipt_0"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("cookie@{point}"))];
        let mut s = Session::launch("count.html", RuleSet::default(), &env).await;
        s.mutate("load", Op::Advance { by_us: 50_000 }).await.unwrap();
        let target = Locator::Document { selector: None };
        let field = StateField::Cookie { name: "session".into(), path: None, domain: None, partition: None };
        let Reply::Observed { result: Observation::State(read) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: target.clone(), field: field.clone() }, context: None }).await.unwrap() else { panic!() };
        assert_eq!(read.value, serde_json::Value::Null);
        let op = Op::StateApply { boundary: read.boundary, changes: vec![StateChange { target, field, value: serde_json::json!({"value": "ada"}), expected: None }], context: None, progress_us: None };
        assert!(s.mutate("cookie", op.clone()).await.is_err(), "fault must fire: {point}");
        s.recover_and_reattach().await;
        let result = s.mutate("cookie", op).await.unwrap();
        let Reply::Done { outcome: Outcome::StateApplied(applied), .. } = &result else { panic!("{point}: {result:?}") };
        assert!(applied.completed, "{point}: {applied:?}");
        assert_eq!(applied.receipts[0].after, Some(serde_json::json!({"value": "ada", "secure": false, "http_only": false, "same_site": "lax"})), "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn state_recovery_preserves_a_rejected_suffix_after_an_earlier_effect() {
    use frontcap_core::intervention::{StateChange, StateField, ExpectedValue};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    let env = [("FRONTCAP_FAULT".to_string(), "partial-state@state_after_receipt_1".into())];
    let mut s = Session::launch("state-counter.html", RuleSet::default(), &env).await;
    s.mutate("load", Op::Advance { by_us: 50_000 }).await.unwrap();
    let calls = Locator::Css { selector: "#calls".into() };
    let Reply::Observed { result: Observation::State(read) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: calls.clone(), field: StateField::Text }, context: None }).await.unwrap() else { panic!() };
    let op = Op::StateApply { boundary: read.boundary, changes: vec![
        StateChange { target: Locator::Css { selector: "#widget".into() }, field: StateField::Attribute { name: "data-touch".into() }, value: "first".into(), expected: None },
        StateChange { target: calls.clone(), field: StateField::Text, value: "overwritten".into(), expected: Some(ExpectedValue { value: "0".into() }) },
    ], context: None, progress_us: None };
    assert!(s.mutate("partial-state", op.clone()).await.is_err());
    s.recover_and_reattach().await;
    let Reply::Done { outcome: Outcome::StateApplied(applied), .. } = s.mutate("partial-state", op).await.unwrap() else { panic!() };
    assert!(!applied.completed);
    assert!(applied.receipts[0].verified);
    assert!(!applied.receipts[1].attempted);
    assert!(s.text_is("one-call", "#calls", "1").await);
    s.stop().await;
}

#[tokio::test]
async fn storage_writes_recover_after_the_first_setter() {
    use frontcap_core::intervention::{StateChange, StateField, StorageArea};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    let env = [("FRONTCAP_FAULT".to_string(), "storage-write@state_after_write_0".into())];
    let mut s = Session::launch("state-counter.html", RuleSet::default(), &env).await;
    s.mutate("load", Op::Advance { by_us: 50_000 }).await.unwrap();
    let target = Locator::Css { selector: "#widget".into() };
    let Reply::Observed { result: Observation::StorageScope(scope) } = s.client.call(Call::Observe { op: ObserveOp::StorageScope { target: target.clone() }, context: None }).await.unwrap() else { panic!() };
    let fields: Vec<_> = [StorageArea::Local, StorageArea::Session].into_iter().map(|area| StateField::Storage { area, scope: scope.scope.clone(), key: "cart".into() }).collect();
    let op = Op::StateApply { boundary: scope.boundary, changes: fields.iter().map(|field| StateChange { target: target.clone(), field: field.clone(), value: "prepared".into(), expected: None }).collect(), context: None, progress_us: None };
    assert!(s.mutate("storage-write", op.clone()).await.is_err());
    s.recover_and_reattach().await;
    let Reply::Done { outcome: Outcome::StateApplied(applied), .. } = s.mutate("storage-write", op).await.unwrap() else { panic!() };
    assert!(applied.completed);
    assert_eq!(applied.receipts.len(), 2);
    for field in fields {
        let Reply::Observed { result: Observation::State(read) } = s.client.call(Call::Observe { op: ObserveOp::StateRead { target: target.clone(), field }, context: None }).await.unwrap() else { panic!() };
        assert_eq!(read.value, "prepared");
    }
    s.stop().await;
}

async fn pointer_state(s: &mut Session) -> frontcap_core::pointer::PointerState {
    let Reply::Observed { result: frontcap_recorder_protocol::Observation::Input(state) } = s.client.call(Call::Observe { op: frontcap_recorder_protocol::ObserveOp::InputState, context: None }).await.unwrap() else { panic!() };
    *state
}

#[tokio::test]
async fn pointer_button_ownership_survives_a_recorder_fault() {
    use frontcap_core::{action::Action, pointer::PointerButton};
    for (button, mask, name) in [(PointerButton::Right, 2, "right"), (PointerButton::Middle, 4, "middle")] {
        let env = [("FRONTCAP_FAULT".to_string(), "begin@after_substep_0".into())];
        let mut s = Session::launch("pointer.html", RuleSet::default(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let state = pointer_state(&mut s).await;
        let op = Op::Act { then: None, action: Action::PointerBegin { target: Locator::Css { selector: "#pad".into() }, point: [40, 40], button, pointer: Default::default(), boundary: state.boundary }, context: None };
        assert!(s.mutate("begin", op.clone()).await.is_err());
        s.recover_and_reattach().await;
        assert!(matches!(s.mutate("begin", op).await.unwrap(), Reply::Done { outcome: Outcome::Pointer { .. }, .. }));
        let state = pointer_state(&mut s).await;
        assert!(state.held.is_none());
        assert_eq!(state.boundary.t_us.0, 51_000);
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("events", Op::Evaluate { expression: "buttons.filter(e => ['pointerdown', 'pointerup', 'pointercancel', 'click', 'auxclick'].includes(e[0])).map(e => [e[0],e[2],e[3],e[4]])".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!([["pointerdown",mask,true,51],["pointercancel",0,true,51]]), "{name}");
        s.stop().await;
    }
}

#[tokio::test]
async fn wheel_faults_preserve_one_native_delivery_and_one_default_scroll() {
    use frontcap_core::action::Action;
    for point in ["before_substep_0", "after_substep_0", "after_action", "after_effect"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("wheel@{point}"))];
        let mut s = Session::launch("wheel.html", RuleSet::default(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let state = pointer_state(&mut s).await;
        let op = Op::Act { then: None, action: Action::Wheel { target: Locator::Css { selector: "#content".into() }, point: [40, 40], delta: [0, 80], boundary: state.boundary }, context: None };
        assert!(s.mutate("wheel", op.clone()).await.is_err(), "{point} must abort");
        s.recover_and_reattach().await;
        let reply = s.mutate("wheel", op).await.unwrap();
        assert!(matches!(reply, Reply::Done { outcome: Outcome::Pointer { .. }, .. }), "{point}: {reply:?}");
        assert_eq!(pointer_state(&mut s).await.boundary.t_us.0, 51_000);
        s.mutate("frame", Op::Advance { by_us: 34_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("result", Op::Evaluate { expression: "[wheels.length, document.querySelector('#inner').scrollTop, document.querySelector('#outer').scrollTop]".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!([1, 80, 0]), "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn pointer_faults_resolve_once_and_cancel_before_a_replacement_lease() {
    use frontcap_core::action::Action;
    for phase in ["begin", "move", "end", "cancel"] {
        for point in ["after_substep_0", "after_action", "after_effect"] {
            let env = [("FRONTCAP_FAULT".to_string(), format!("{phase}@{point}"))];
            let mut s = Session::launch("drag.html", RuleSet::default(), &env).await;
            s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
            let state = pointer_state(&mut s).await;
            let begin = Op::Act { then: None, action: Action::PointerBegin { button: Default::default(), target: Locator::Css { selector: "#handle".into() }, point: [40, 40], pointer: Default::default(), boundary: state.boundary }, context: None };
            let op = if phase == "begin" { begin } else {
                s.mutate("begin", begin).await.unwrap();
                let state = pointer_state(&mut s).await;
                let handle = state.held.unwrap().handle;
                if phase == "move" { Op::Act { then: None, action: Action::PointerMove { handle, point: [140, 40], boundary: state.boundary }, context: None } }
                else if phase == "cancel" { Op::Act { then: None, action: Action::PointerCancel { handle }, context: None } }
                else { Op::Act { then: None, action: Action::PointerEnd { handle }, context: None } }
            };
            assert!(s.mutate(phase, op.clone()).await.is_err(), "{phase}@{point} must abort");
            s.recover_and_reattach().await;
            let again = s.mutate(phase, op).await.unwrap();
            assert!(matches!(again, Reply::Done { outcome: Outcome::Pointer { .. }, .. }), "{phase}@{point}: {again:?}");
            let current = pointer_state(&mut s).await;
            assert!(current.held.is_none(), "the old lease cannot leave input held: {phase}@{point}");
            assert_eq!(current.boundary.t_us.0, 51_000);
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("log", Op::Evaluate { expression: "JSON.stringify(log.filter(e => e[0] === 'pointerdown' || e[0] === 'pointerup' || e[0] === 'pointercancel').map(e => e[0]))".into() }).await.unwrap() else { panic!() };
            assert_eq!(value, if phase == "end" { "[\"pointerdown\",\"pointerup\"]" } else { "[\"pointerdown\",\"pointercancel\"]" }, "{phase}@{point}");
            if phase == "move" {
                let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("moves", Op::Evaluate { expression: "log.filter(e => e[0] === 'pointermove' && e[3] === 1).length".into() }).await.unwrap() else { panic!() };
                assert_eq!(value, 1);
            }
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn pointer_begin_accepted_before_disconnect_is_finished_and_cleaned_before_reattach() {
    use frontcap_core::action::Action;
    let mut s = Session::launch("drag.html", RuleSet::default(), &[]).await;
    s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
    let state = pointer_state(&mut s).await;
    let op = Op::Act { then: None, action: Action::PointerBegin { button: Default::default(), target: Locator::Css { selector: "#handle".into() }, point: [40, 40], pointer: Default::default(), boundary: state.boundary }, context: None };
    s.client.send_without_waiting(Call::Mutate { lease: Some(s.lease.clone()), request_id: rid("begin"), op }).await.unwrap();
    let runtime = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
    let (client, _) = RecorderClient::connect(&runtime.socket).await.unwrap();
    drop(std::mem::replace(&mut s.client, client));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match s.client.call(Call::Reattach { request_id: rid("replacement") }).await {
            Ok(Reply::Attached { lease, status, .. }) => {
                s.lease = lease;
                assert!(status.pointer.is_none(), "lease publication waits for cleanup");
                assert_eq!(status.t_us.0, 51_000);
                break;
            }
            Err(ClientError::Recorder(e)) if matches!(e.code, frontcap_recorder_protocol::ErrorCode::Busy | frontcap_recorder_protocol::ErrorCode::LeaseHeld) => {
                assert!(tokio::time::Instant::now() < deadline, "cleanup did not finish: {e:?}");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            other => panic!("{other:?}"),
        }
    }
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("log", Op::Evaluate { expression: "JSON.stringify(log.filter(e => e[0] === 'pointerdown' || e[0] === 'pointerup' || e[0] === 'pointercancel').map(e => e[0]))".into() }).await.unwrap() else { panic!() };
    assert_eq!(value, "[\"pointerdown\",\"pointercancel\"]");
    s.stop().await;
}

#[tokio::test]
async fn interrupted_lease_cleanup_cancels_once_before_publishing_replacement_control() {
    use frontcap_core::action::Action;
    let cleanup = format!("mcp-pointer-cleanup-{}", frontcap_core::canonical::sha256_hex("begin"));
    for point in ["before_dispatch", "before_substep_0", "after_substep_0", "after_action", "after_effect", "after_complete"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("{cleanup}@{point}"))];
        let mut s = Session::launch("cancel.html", RuleSet::default(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let state = pointer_state(&mut s).await;
        s.mutate("begin", Op::Act { then: None, action: Action::PointerBegin { target: Locator::Css { selector: "#commit".into() }, point: [40, 40], button: Default::default(), pointer: Default::default(), boundary: state.boundary }, context: None }).await.unwrap();
        assert!(s.client.call(Call::Release { lease: s.lease.clone(), request_id: rid("release") }).await.is_err(), "{point} must abort cleanup");
        s.recover_and_reattach().await;
        let status = s.status().await;
        assert!(status.pointer.is_none(), "replacement control requires native cleanup: {point}");
        assert!(!status.input_cleanup_pending);
        assert_eq!(status.t_us.0, 51_000);
        let Reply::Effects { effects } = s.client.call(Call::Effects { after_seq: 0, before_seq: None }).await.unwrap() else { panic!("no effects") };
        let cleanup_events: Vec<_> = effects.input.iter().filter(|input| input.action == "pointer_cancel")
            .flat_map(|input| &input.events).collect();
        assert_eq!(cleanup_events.len(), 1, "{point}: {cleanup_events:?}");
        assert_eq!((&cleanup_events[0].event, cleanup_events[0].t_us.0, cleanup_events[0].trusted),
            (&"pointercancel".to_string(), 51_000, true), "{point}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("events", Op::Evaluate { expression: "({commits,timer,frame,captured:document.querySelector('#commit').hasPointerCapture(pointer),events:events.filter(e => ['pointercancel','pointerup','click'].includes(e[0])).map(e => [e[0],e[4],e[5]])})".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"commits":0,"timer":false,"frame":false,"captured":false,"events":[["pointercancel",true,51]]}), "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn pointer_document_loss_fails_the_generation_and_ends_the_browser() {
    use frontcap_core::action::Action;
    let mut s = Session::launch("drag.html", RuleSet::default(), &[]).await;
    s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
    let state = pointer_state(&mut s).await;
    let op = Op::Act { then: None, action: Action::PointerBegin { button: Default::default(), target: Locator::Css { selector: "#leave".into() }, point: [420, 40], pointer: Default::default(), boundary: state.boundary }, context: None };
    match s.mutate("begin", op).await {
        Err(ClientError::Recorder(error)) => assert_eq!(error.code, frontcap_recorder_protocol::ErrorCode::RecorderFault),
        other => panic!("{other:?}"),
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let runtime = loop {
        let runtime = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        if !runtime.recorder.is_alive() {
            // The recorder may publish its terminal state between our read and the process probe.
            break frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        }
        assert!(tokio::time::Instant::now() < deadline, "failed recorder remained alive: {runtime:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(matches!(runtime.state, frontcap_recorder::runtime::RuntimeState::Failed { .. }), "{runtime:?}");
    assert!(!runtime.browser.unwrap().is_alive());
}

#[tokio::test]
async fn pointer_cancel_that_loses_its_document_fails_instead_of_claiming_cleanup() {
    use frontcap_core::action::Action;
    let mut s = Session::launch("cancel.html#leave", RuleSet::default(), &[]).await;
    s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
    let state = pointer_state(&mut s).await;
    s.mutate("begin", Op::Act { then: None, action: Action::PointerBegin { target: Locator::Css { selector: "#commit".into() }, point: [40, 40], button: Default::default(), pointer: Default::default(), boundary: state.boundary }, context: None }).await.unwrap();
    let handle = pointer_state(&mut s).await.held.unwrap().handle;
    match s.mutate("cancel", Op::Act { then: None, action: Action::PointerCancel { handle }, context: None }).await {
        Err(ClientError::Recorder(error)) => assert_eq!(error.code, frontcap_recorder_protocol::ErrorCode::RecorderFault),
        other => panic!("{other:?}"),
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let runtime = loop {
        let runtime = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        if !runtime.recorder.is_alive() {
            // Read after exit, so a pre-exit Ready snapshot cannot hide the published failure.
            break frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        }
        assert!(tokio::time::Instant::now() < deadline, "failed recorder remained alive: {runtime:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(matches!(runtime.state, frontcap_recorder::runtime::RuntimeState::Failed { .. }), "{runtime:?}");
    assert!(!runtime.browser.unwrap().is_alive());
}

/// Lease loss releases every held input before another controller can attach: a composition cancelled,
/// a held touch cancelled, and held keys released (other keys under the modifiers still held, then the
/// modifiers), each journaled at the current instant.
#[tokio::test]
async fn lease_loss_releases_held_keys_a_composition_and_a_touch() {
    check_held_input_cleanup(None).await;
}

/// Native event history, modifier state and virtual time must survive a fault inside each cleanup
/// operation, including an intent never dispatched and an effect whose answer was not recorded.
#[tokio::test]
async fn interrupted_held_input_cleanup_releases_once_before_replacement_control() {
    let cleanup_id = |kind: &str, seed: &str| format!("mcp-{kind}-cleanup-{}", frontcap_core::canonical::sha256_hex(seed));
    let composition = cleanup_id("compose", "touch:compose");
    let touch = cleanup_id("pointer", "touch");
    let arrow = cleanup_id("key", &format!("{touch}:ArrowDown"));
    let shift = cleanup_id("key", &format!("{arrow}:Shift"));
    for cleanup in [composition, touch, arrow, shift] {
        for point in ["before_dispatch", "before_substep_0", "after_substep_0", "after_action", "after_effect", "after_complete"] {
            let fault = format!("{cleanup}@{point}");
            eprintln!("held input cleanup: {fault}");
            check_held_input_cleanup(Some(&fault)).await;
        }
    }
}

async fn check_held_input_cleanup(fault: Option<&str>) {
    use frontcap_core::{action::Action, pointer::PointerKind};
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let mut req = support::launch_request("launch", &format!("http://{addr}/held.html"));
    req.profile.touch = true;
    let env = fault.map(|f| vec![("FRONTCAP_FAULT".to_string(), f.to_string())]).unwrap_or_default();
    let a = launcher::launch(&ws, req, &support::recorder_bin(), &env).await.unwrap();
    let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
    let mut s = Session { _guard: guard, _tmp, ws: ws.clone(), id: a.launch_id, client: a.client, lease: a.lease };
    s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
    s.mutate("focus", Op::Act { then: None, action: Action::Click { target: Locator::Css { selector: "#field".into() } }, context: None }).await.unwrap();
    s.mutate("shift", Op::Act { then: None, action: Action::KeyDown { key: "Shift".into() }, context: None }).await.unwrap();
    s.mutate("arrow", Op::Act { then: None, action: Action::KeyDown { key: "ArrowDown".into() }, context: None }).await.unwrap();
    s.mutate("compose", Op::Act { then: None, action: Action::Compose { text: "서".into() }, context: None }).await.unwrap();
    let state = pointer_state(&mut s).await;
    assert_eq!((state.keys.clone(), state.composition.clone()), (vec!["Shift".to_string(), "ArrowDown".to_string()], Some("서".to_string())));
    s.mutate("touch", Op::Act { then: None, action: Action::PointerBegin { target: Locator::Css { selector: "#pad".into() }, point: [40, 140], button: Default::default(), pointer: PointerKind::Touch, boundary: state.boundary }, context: None }).await.unwrap();
    let status = s.status().await;
    assert_eq!((status.keys.clone(), status.composing, status.pointer.is_some()), (vec!["Shift".to_string(), "ArrowDown".to_string()], true, true));
    let released = s.client.call(Call::Release { lease: s.lease.clone(), request_id: rid("release") }).await;
    if let Some(fault) = fault {
        assert!(released.is_err(), "{fault} must abort cleanup: {released:?}");
        s.recover_and_reattach().await;
    } else {
        released.unwrap();
        let a = launcher::reattach(&s.ws, &s.id, rid("reattach")).await.unwrap();
        s.client = a.client;
        s.lease = a.lease;
    }
    let status = s.status().await;
    assert!(status.pointer.is_none() && status.keys.is_empty() && !status.composing && !status.input_cleanup_pending, "{status:?}");
    assert_eq!(status.t_us.0, 51_000);
    let state = pointer_state(&mut s).await;
    assert_eq!(state.composition, None);
    // The composition, then the finger, then the arrow key under the Shift still held, then Shift.
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("events", Op::Evaluate { expression: "seen.filter(e => !e[0].startsWith('key') || ['Shift', 'ArrowDown'].includes(e[1]))".into() }).await.unwrap() else { panic!() };
    assert_eq!(value, serde_json::json!([
        ["keydown", "Shift", true], ["keydown", "ArrowDown", true], ["compositionstart", "", null], ["touchstart", null, true],
        ["compositionend", "", null], ["touchcancel", null, true], ["keyup", "ArrowDown", true], ["keyup", "Shift", false]
    ]));
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("native-state", Op::Evaluate { expression: "({value:document.querySelector('#field').value, timed:cleanupTimer, framed:cleanupFrame, events:nativeSeen.filter(e => ['keyup','compositionend','touchcancel'].includes(e[0])).map(e => [e[0],e[2]]), trusted:nativeSeen.filter(e => ['keyup','touchcancel'].includes(e[0])).every(e => e[1])})".into() }).await.unwrap() else { panic!() };
    assert_eq!(value, serde_json::json!({"value":"", "timed":false, "framed":false, "trusted":true, "events":[
        ["compositionend", 51], ["touchcancel", 51], ["keyup", 51], ["keyup", 51]
    ]}), "{fault:?}");
    s.stop().await;
}

/// Temporary-watch installation/removal and the wait result form one recoverable parent operation.
#[tokio::test]
async fn predicate_wait_recovery_keeps_its_result_budget_and_cleans_the_watch() {
    for (selector, budget, expected, start) in [(".late", 2_000_000, 1_234_000, 0), (".missing", 500_000, 500_000, 0), (".late", 2_000_000, 1_234_000, 1_000)] {
        for point in ["before_dispatch", "after_dispatch", "before_watch_install", "after_watch_install", "before_watch_cleanup", "after_watch_remove", "after_watch_cleanup", "after_complete"] {
            eprintln!("predicate recovery {selector} start={start} {point}");
            let op = Op::UntilPredicate {
                predicate: Predicate::Attached { target: Locator::Css { selector: selector.into() } }, max_us: budget, context: None,
            };
            let setup = if start == 0 { vec![] } else { vec![Op::Advance { by_us: start }] };
            let (answer, mut s) = fault_and_repeat("appear.html", RuleSet::default(), setup, "predicate", point, op.clone()).await;
            if point == "before_dispatch" {
                assert!(matches!(answer, Reply::Done { outcome: Outcome::NotApplied, .. }), "{answer:?}");
                assert_eq!(s.status().await.t_us.0, start);
            } else {
                let Reply::Done { outcome: Outcome::Clock { result, start_us, t_us, .. }, .. } = &answer else { panic!("{answer:?}") };
                assert_eq!(start_us.0, start);
                assert_eq!(t_us.0, expected, "{point}: original grant/result was lost");
                assert_eq!(matches!(result, frontcap_core::event::ClockOutcome::ConditionMet { .. }), selector == ".late");
                assert_eq!(s.mutate("predicate", op).await.unwrap(), answer);
                assert_eq!(s.status().await.t_us.0, expected);
            }
            assert!(s.status().await.watches.is_empty(), "{point}: temporary watch leaked");
            let state: frontcap_recorder::control::ControlState = frontcap_core::state::load(&frontcap_recorder::runtime::control_path(&s.ws.runtime_dir(&s.id))).unwrap().unwrap();
            assert!(state.predicate_watches.is_empty());
            // A separate wait must be able to install and remove its own temporary watch afterward.
            let next = s.mutate("next-predicate", Op::UntilPredicate {
                predicate: Predicate::Attached { target: Locator::Css { selector: "body".into() } }, max_us: 1, context: None,
            }).await.unwrap();
            assert!(matches!(next, Reply::Done { outcome: Outcome::Clock { .. }, .. }), "{next:?}");
            assert!(s.status().await.watches.is_empty());
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn reference_predicate_wait_recovers_installation_and_cleanup_with_the_original_result() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    for point in ["before_watch_install", "after_watch_install", "before_watch_cleanup", "after_watch_remove", "after_watch_cleanup"] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("predicate@{point}"))];
        let mut s = Session::launch("reference-wait.html", RuleSet::default(), &env).await;
        s.mutate("ready", Op::Advance { by_us: 1_000 }).await.unwrap();
        let Reply::Observed { result: Observation::Query(node) } = s.client.call(Call::Observe {
            op: ObserveOp::Query { target: Locator::Css { selector: "#status".into() }, props: vec!["text".into()] }, context: None,
        }).await.unwrap() else { panic!() };
        let op = Op::UntilPredicate {
            predicate: Predicate::TextEquals { target: Locator::Ref { id: node.r#ref }, text: "Ready".into() },
            max_us: 2_000_000,
            context: None,
        };
        assert!(s.mutate("predicate", op.clone()).await.is_err(), "fault did not fire at {point}");
        s.recover_and_reattach().await;
        let answer = s.mutate("predicate", op.clone()).await.unwrap();
        let Reply::Done { outcome: Outcome::Clock { result, start_us, t_us, .. }, .. } = &answer else { panic!("{point}: {answer:?}") };
        assert!(matches!(result, frontcap_core::event::ClockOutcome::ConditionMet { .. }), "{point}: {answer:?}");
        assert_eq!(start_us.0, 1_000);
        assert_eq!(t_us.0, 1_234_000, "{point}: original budget and condition instant survive");
        assert_eq!(s.mutate("predicate", op).await.unwrap(), answer);
        assert!(s.status().await.watches.is_empty(), "{point}: temporary watch leaked");
        let state: frontcap_recorder::control::ControlState = frontcap_core::state::load(&frontcap_recorder::runtime::control_path(&s.ws.runtime_dir(&s.id))).unwrap().unwrap();
        assert!(state.predicate_watches.is_empty());
        s.stop().await;
    }
}

/// The signal command fails while the real browser remains alive. Both live and pre-renderer
/// cleanup must retain that uncertainty, rather than publishing a stopped/completed generation.
#[cfg(unix)]
#[tokio::test]
async fn dialog_cleanup_failure_preserves_failed_termination_evidence() {
    use frontcap_core::journal::{Journal, RequestState};
    use frontcap_recorder::runtime::{self, RuntimeState};
    let signal_dir = tempfile::tempdir().unwrap();
    let signal = signal_dir.path().join("kill");
    support::write_program(&signal, "#!/bin/sh\nprintf 'termination denied by oracle\\n' >&2\nexit 1\n");
    let mut search_paths = vec![signal_dir.path().to_path_buf()];
    search_paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let env = [("PATH".to_string(), std::env::join_paths(search_paths).unwrap().into_string().unwrap())];
    for recovery in [false, true] {
        eprintln!("termination failure during {} cleanup", if recovery { "recovery" } else { "live" });
        let mut s = Session::launch("observe.html", RuleSet::default(), if recovery { &[] } else { &env }).await;
        s.mutate("policy", Op::DialogRules { rules: serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap() }).await.unwrap();
        s.mutate("ready", Op::Advance { by_us: 1_000 }).await.unwrap();
        s.mutate("setup", Op::Evaluate { expression: "setTimeout(()=>{while(true)confirm('again');},10);true".into() }).await.unwrap();
        assert!(matches!(s.mutate("parent", Op::Advance { by_us: 50_000 }).await.unwrap(), Reply::Suspended { .. }));
        let dir = s.ws.runtime_dir(&s.id);
        let before = runtime::load_runtime(&dir).unwrap().unwrap();
        if recovery {
            before.recorder.kill().unwrap();
            let error = launcher::recover(&s.ws, &s.id, &support::recorder_bin(), &env).await.unwrap_err();
            assert!(error.0.message.contains("browser termination failed"), "{error:?}");
        } else {
            // Shutdown can race the error frame; durable failure evidence below is authoritative.
            let result = s.client.call(Call::Release { lease: s.lease.clone(), request_id: rid("release") }).await;
            assert!(result.is_err(), "failed cleanup cannot acknowledge release: {result:?}");
            if let Err(ClientError::Recorder(error)) = result {
                assert!(error.message.contains("browser termination failed"), "{error:?}");
            }
        }
        let failed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let record = runtime::load_runtime(&dir).unwrap().unwrap();
                if matches!(record.state, RuntimeState::Failed { .. }) && !record.recorder.is_alive() { break record; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("failed recorder did not publish its terminal state");
        let RuntimeState::Failed { reason, .. } = &failed.state else { unreachable!() };
        for detail in ["64 consecutive dialogs", "browser termination failed", "termination denied by oracle"] {
            assert!(reason.contains(detail), "{reason}");
        }
        assert_eq!(failed.browser, before.browser);
        assert!(failed.browser.as_ref().unwrap().is_alive(), "the refusal oracle must leave the owned browser alive");
        let control: frontcap_recorder::control::ControlState = frontcap_core::state::load(&runtime::control_path(&dir)).unwrap().unwrap();
        assert!(control.faults.iter().any(|f| f.at_us.0 == 11_000 && f.detail.contains("browser termination failed") && f.affected.contains(&"parent".to_string())), "{:?}", control.faults);
        assert_eq!(Journal::read_state(&runtime::journal_path(&dir), &rid("parent")).unwrap(), Some(RequestState::Dispatched));
        let spec = runtime::load_spec(&dir).unwrap().unwrap();
        assert!(!spec.runs_root.join(spec.run_id.as_str()).join("manifest.json").exists(), "failure is not bundle finalization");
        let evidence = ["runtime.json", "control.json", "journal.jsonl"].map(|name| (name, std::fs::read(dir.join(name)).unwrap()));
        let error = launcher::recover(&s.ws, &s.id, &support::recorder_bin(), &[]).await.unwrap_err();
        assert!(error.0.message.contains("termination denied by oracle"), "failed execution was relaunched: {error:?}");
        let direct = std::process::Command::new(support::recorder_bin()).args(["recover", "--runtime-dir"]).arg(&dir).output().unwrap();
        assert!(!direct.status.success(), "direct recovery bypassed the failed-state boundary");
        for (name, bytes) in evidence {
            assert_eq!(std::fs::read(dir.join(name)).unwrap(), bytes, "refused recovery changed {name}");
        }
        assert!(failed.browser.as_ref().unwrap().is_alive());
        // Termination-only stop must preserve the failed world and reject a reused parent id.
        let conflict = launcher::terminate_failed(&s.ws, &s.id, &rid("parent")).await.unwrap_err();
        assert_eq!(conflict.0.code, frontcap_recorder_protocol::ErrorCode::RequestIdConflict);
        assert!(failed.browser.as_ref().unwrap().is_alive());
        let bundle = launcher::terminate_failed(&s.ws, &s.id, &rid("terminate")).await.unwrap();
        assert_eq!(bundle, spec.runs_root.join(spec.run_id.as_str()));
        assert!(!failed.browser.as_ref().unwrap().is_alive());
        assert_eq!(runtime::load_runtime(&dir).unwrap().unwrap(), failed);
        assert!(!bundle.join("manifest.json").exists());
        let RequestState::Completed(value) = Journal::read_state(&runtime::journal_path(&dir), &rid("terminate")).unwrap().unwrap() else { panic!("termination has no durable result") };
        assert!(matches!(serde_json::from_value::<frontcap_recorder_protocol::Answer>(value).unwrap(),
            frontcap_recorder_protocol::Answer::Ok(Reply::Done { outcome: Outcome::Stopped { finalized: false }, .. })));
        let after = std::fs::read(runtime::journal_path(&dir)).unwrap();
        assert_eq!(launcher::terminate_failed(&s.ws, &s.id, &rid("terminate")).await.unwrap(), bundle);
        assert_eq!(std::fs::read(runtime::journal_path(&dir)).unwrap(), after);
        assert_eq!(Journal::read_state(&runtime::journal_path(&dir), &rid("parent")).unwrap(), Some(RequestState::Dispatched));
    }
}

#[tokio::test]
async fn exception_event_recovery_preserves_the_arm_and_stopped_instant() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
        let op = Op::Next { event: ProgressEvent::Exception, max_us: 1_000_000 };
        let (answer, mut s) = fault_and_repeat("exception-stop.html", RuleSet::default(),
            vec![Op::Advance { by_us: 51_000 }], "next-exception", point, op).await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = answer else { panic!("{point}: {answer:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Exception, ref triggers } if triggers.len() == 1), "{point}: {result:?}");
        assert_eq!(start.t_us.0, 51_000);
        assert_eq!(end.t_us.0, 250_000, "{point}");
        assert!(s.text_is("exception-boundary", "#out", "failed").await);
        let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } = s.mutate("after-exception", Op::Next { event: ProgressEvent::Exception, max_us: 20_000 }).await.unwrap() else { panic!() };
        assert_eq!(result, ClockOutcome::EventTimedOut { event: ProgressEvent::Exception });
        assert_eq!(end.t_us.0, 270_000);
        s.stop().await;
    }
}

#[tokio::test]
async fn exception_recovery_after_child_context_destruction_preserves_one_occurrence() {
    use frontcap_core::{event::EventKind, log::BehaviorLog};
    let (_, mut s) = fault_and_repeat("exception-context.html", RuleSet::default(), vec![],
        "remove-child", "after_clock", Op::Advance { by_us: 400_000 }).await;
    assert!(s.text_is("removed-child", "#out", "removed").await);
    let status = s.status().await;
    let trace = s.ws.runs().join(status.run_id.as_str()).join("trace.jsonl");
    let events = BehaviorLog::read(&trace).unwrap();
    assert_eq!(events.iter().filter(|e| matches!(&e.kind,
        EventKind::Exception(e) if e.text.contains("removed child failure"))).count(), 1);
    s.stop().await;
}

#[tokio::test]
async fn exception_receipts_survive_loss_before_trace_sync_and_acknowledgment() {
    use frontcap_core::{event::{ClockOutcome, EventKind, ProgressEvent}, log::BehaviorLog};
    for (fixture, message) in [("exception-stop.html", "scheduled failure"), ("exception-stop.html?promise", "scheduled failure"), ("exception-context.html", "removed child failure")] {
        for point in ["before_trace", "after_trace_sync", "after_forget"] {
            let env = [("FRONTCAP_EXCEPTION_FAULT".into(), point.into())];
            let mut s = Session::launch(fixture, RuleSet::default(), &env).await;
            let run = s.status().await.run_id;
            s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
            let op = Op::Next { event: ProgressEvent::Exception, max_us: 1_000_000 };
            let first = s.mutate("next-exception", op.clone()).await;
            if point == "after_forget" && first.is_ok() {
                // A stopped request may finish before the next drain acknowledges its receipt.
                assert!(s.mutate("acknowledge", Op::Advance { by_us: 20_000 }).await.is_err());
            } else {
                assert!(first.is_err(), "the recorder must die at {point}: {first:?}");
            }
            let trace = s.ws.runs().join(run.as_str()).join("trace.jsonl");
            let count = || BehaviorLog::read(&trace).unwrap().iter()
                .filter(|e| matches!(&e.kind, EventKind::Exception(e) if e.text.contains(message))).count();
            assert_eq!(count(), usize::from(point != "before_trace"), "{point}");
            s.recover_and_reattach().await;
            let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } =
                s.mutate("next-exception", op).await.unwrap() else { panic!("{point}") };
            assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Exception, ref triggers } if triggers.len() == 1), "{point}: {result:?}");
            assert_eq!(end.t_us.0, 250_000, "{point}");
            assert_eq!(count(), 1, "{point}");
            let Reply::Done { outcome: Outcome::Progress { result, .. }, .. } =
                s.mutate("no-duplicate", Op::Next { event: ProgressEvent::Exception, max_us: 20_000 }).await.unwrap() else { panic!() };
            assert_eq!(result, ClockOutcome::EventTimedOut { event: ProgressEvent::Exception }, "{point}");
            s.stop().await;
        }
    }
}

fn interception_rules() -> RuleSet {
    RuleSet::from_json(&serde_json::to_string(&["first", "other", "second"].map(|name| serde_json::json!({
        "id":name,"match":{"url":format!("https://api.example.test/{name}")},
        "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":name}
    }))).unwrap()).unwrap()
}

#[tokio::test]
async fn interception_event_recovery_preserves_pending_rules_and_the_arm() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
        let (answer, mut s) = fault_and_repeat("interception-stop.html", interception_rules(),
            vec![Op::Advance { by_us: 51_000 }], "inspect", point,
            Op::Next { event: ProgressEvent::Interception, max_us: 199_000 }).await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = answer else { panic!("{point}: {answer:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Interception, ref triggers } if triggers.len() == 2), "{point}: {result:?}");
        assert_eq!((start.t_us.0, end.t_us.0), (51_000, 250_000), "{point}");
        assert_eq!(s.status().await.held_requests.len(), 2);
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("before", Op::Evaluate { expression: "({delivered,later})".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"delivered":[],"later":false}), "{point}");
        s.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("after", Op::Evaluate { expression: "({delivered,later})".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"delivered":["first","other"],"later":true}), "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn an_answer_to_an_interception_recovers_without_applying_its_original_rule() {
    use frontcap_core::{event::ProgressEvent, rules::{FailReason, MockSpec}};
    for fail in [false, true] {
        for point in ["after_dispatch", "after_engine", "after_effect", "after_complete"] {
            let env = [("FRONTCAP_FAULT".into(), format!("answer@{point}"))];
            let mut s = Session::launch("interception-stop.html", interception_rules(), &env).await;
            s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
            s.mutate("inspect", Op::Next { event: ProgressEvent::Interception, max_us: 199_000 }).await.unwrap();
            let held = s.status().await.held_requests;
            let first = held.iter().find(|h| h.url.ends_with("/first")).unwrap().ordinal.clone();
            let op = if fail { Op::FailHeld { ordinal: first.clone(), reason: FailReason::Aborted } } else { Op::Respond { ordinal: first.clone(), response: MockSpec { status: 200,
                headers: [("access-control-allow-origin".into(), "*".into())].into(),
                body: "changed".into(), body_base64: None, latency_ms: 0 } } };
            assert!(s.mutate("answer", op.clone()).await.is_err());
            s.recover_and_reattach().await;
            let answer = s.mutate("answer", op.clone()).await.unwrap();
            if point == "after_dispatch" {
                assert!(matches!(answer, Reply::Done { outcome: Outcome::NotApplied, .. }), "{answer:?}");
                assert_eq!(s.status().await.held_requests.len(), 2);
                assert!(matches!(s.mutate("new-answer", op).await.unwrap(), Reply::Done { outcome: Outcome::Ack, .. }));
            } else {
                assert!(matches!(answer, Reply::Done { outcome: Outcome::Ack, .. }), "{answer:?}");
            }
            s.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("after", Op::Evaluate { expression: "delivered".into() }).await.unwrap() else { panic!() };
            assert_eq!(value, if fail { serde_json::json!(["other"]) } else { serde_json::json!(["changed","other"]) }, "fail={fail}, {point}");
            let run = s.status().await.run_id;
            let events = frontcap_core::log::BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
            let responses: Vec<_> = events.iter().filter_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Response(r) if r.id == first => Some(r),
                _ => None,
            }).collect();
            assert_eq!(responses.len(), 1, "fail={fail}, {point}: {responses:?}");
            assert_eq!(responses[0].status, if fail { 0 } else { 200 });
            assert_eq!(responses[0].source, if fail { frontcap_core::event::ResponseSource::Fail } else { frontcap_core::event::ResponseSource::Agent });
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn intercepted_document_recovery_retains_the_old_context_and_navigation() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    let url = "https://next.example.test/arrived";
    let rules = RuleSet::from_json(&serde_json::json!([{
        "id":"destination","match":{"url":url},"action":{"type":"mock","status":200,
        "headers":{"content-type":"text/html"},"body":"<script>window.arrivedAt=Date.now()-946684800000</script>"}
    }]).to_string()).unwrap();
    for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
        let setup = vec![Op::Advance { by_us: 51_000 }, Op::Evaluate {
            expression: format!("setTimeout(()=>{{location.href={}}},199)", serde_json::json!(url)),
        }];
        let (reply, mut s) = fault_and_repeat("count.html", rules.clone(), setup, "inspect-document", point,
            Op::Next { event: ProgressEvent::Interception, max_us: 1_000_000 }).await;
        let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } = reply else { panic!("{reply:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Interception, ref triggers } if triggers.len() == 1), "{point}: {result:?}");
        assert_eq!(end.t_us.0, 250_000, "{point}");
        assert_eq!(s.status().await.held_requests.len(), 1, "{point}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("old-page", Op::Evaluate {
            expression: "({path:location.pathname,now:Date.now()-946684800000})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"path":"/count.html","now":250}), "{point}");
        let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } = s.mutate("commit-document",
            Op::Next { event: ProgressEvent::Navigation, max_us: 1_000_000 }).await.unwrap() else { panic!() };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Navigation, .. }), "{point}: {result:?}");
        assert_eq!(end.t_us.0, 250_000, "{point}");
        s.mutate("run-document", Op::Advance { by_us: 20_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("new-page", Op::Evaluate {
            expression: "({url:location.href,arrivedAt})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"url":url,"arrivedAt":250}), "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn interception_publication_recovery_keeps_requests_unanswered() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for point in ["before_bind", "after_bind", "after_retention"] {
        let env = [("FRONTCAP_INTERCEPTION_FAULT".into(), point.into())];
        let mut s = Session::launch("interception-stop.html", interception_rules(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let op = Op::Next { event: ProgressEvent::Interception, max_us: 199_000 };
        assert!(s.mutate("inspect", op.clone()).await.is_err(), "{point}");
        s.recover_and_reattach().await;
        let reply = s.mutate("inspect", op).await.unwrap();
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = reply else { panic!("{point}: {reply:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Interception, ref triggers } if triggers.len() == 2), "{point}: {result:?}");
        assert_eq!((start.t_us.0,end.t_us.0), (51_000,250_000), "{point}");
        assert_eq!(s.status().await.held_requests.len(), 2, "{point}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("still-pending", Op::Evaluate {
            expression: "({delivered,later})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"delivered":[],"later":false}), "{point}");
        s.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("delivered", Op::Evaluate {
            expression: "({delivered,later})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"delivered":["first","other"],"later":true}), "{point}");
        s.stop().await;
    }
}

/// Every request of a collected round is bound before any of its answers is delivered: a recorder lost
/// once one answer is out leaves the others bound, so a request that answer caused at the same instant
/// still follows the whole round.
#[tokio::test]
async fn a_round_answered_in_part_before_a_fault_keeps_its_allocation() {
    let page = "https://round.example.test/";
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":page},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>round</p>"}},
        {"id":"start","match":{"url":"https://round.example.test/start"},"action":{"type":"mock","status":200,"body":"go"}},
        {"id":"sequence","match":{"url":"https://round.example.test/sequence"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"1"},{"status":200,"body":"2"},{"status":200,"body":"3"}]}}
    ]).to_string()).unwrap();
    let env = [("FRONTCAP_INTERCEPTION_FAULT".into(), "after_round_answer".into())];
    let mut s = Session::launch_url(page, rules, &env).await;
    // Two workers from one script; the first one's answer makes the page issue `after` at once.
    let worker = "onmessage=e=>fetch(e.data+'/sequence',{method:'POST',body:name}).then(r=>r.text()).then(b=>postMessage(b));";
    s.mutate("setup", Op::Evaluate { expression: format!("window.got={{}};const url=URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}}));\
        window.workers=['one','two'].map(name=>{{const w=new Worker(url,{{name}});w.onmessage=e=>{{got[name]=e.data;\
        if(name==='one')fetch('/sequence',{{method:'POST',body:'after'}}).then(r=>r.text()).then(b=>got.after=b);}};return w;}});true", serde_json::json!(worker)) }).await.unwrap();
    s.mutate("start", Op::Advance { by_us: 1_000_000 }).await.unwrap();
    // The workers issue from `start`'s answer, while the page runs that instant's callbacks.
    let issued = s.mutate("issue", Op::Evaluate { expression: "fetch('/start').then(()=>workers.forEach(w=>w.postMessage(location.origin)));true".into() }).await;
    let died = match issued {
        Ok(_) => s.mutate("serve", Op::Advance { by_us: 0 }).await,
        lost => lost,
    };
    assert!(matches!(died, Err(ClientError::Closed | ClientError::Frame(_))), "expected process loss after one answer: {died:?}");
    s.recover_and_reattach().await;
    s.mutate("serve-again", Op::Advance { by_us: 0 }).await.unwrap();
    let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("answers", Op::Evaluate { expression: "got".into() }).await.unwrap() else { panic!() };
    assert_eq!(value, serde_json::json!({"one":"1","two":"2","after":"3"}));
    let status = s.status().await;
    let trace = s.ws.runs().join(status.run_id.as_str()).join("trace.jsonl");
    let issued: Vec<u64> = frontcap_core::log::BehaviorLog::read(&trace).unwrap().into_iter().filter_map(|e| match e.kind {
        frontcap_core::event::EventKind::Request(r) if r.url.ends_with("/sequence") => Some(e.t_us.0), _ => None,
    }).collect();
    assert_eq!(issued, vec![1_000_000; 3]);
    s.stop().await;
}

#[tokio::test]
async fn document_interception_publication_recovery_preserves_the_old_page() {
    use frontcap_core::{event::{ClockOutcome, EventKind, ProgressEvent}, log::BehaviorLog};
    let url = "https://next.example.test/arrived";
    let rules = RuleSet::from_json(&serde_json::json!([{
        "id":"destination","match":{"url":url},"action":{"type":"mock","status":200,
        "headers":{"content-type":"text/html"},"body":"<script>window.arrivedAt=Date.now()-946684800000</script>"}
    }]).to_string()).unwrap();
    for point in ["before_bind", "after_bind", "after_retention"] {
        let env = [("FRONTCAP_INTERCEPTION_FAULT".into(), point.into())];
        let mut s = Session::launch("count.html", rules.clone(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        s.mutate("schedule-navigation", Op::Evaluate {
            expression: format!("setTimeout(()=>{{location.href={}}},199)", serde_json::json!(url)),
        }).await.unwrap();
        let op = Op::Next { event: ProgressEvent::Interception, max_us: 199_000 };
        let died = s.mutate("inspect", op.clone()).await;
        assert!(matches!(died, Err(ClientError::Closed | ClientError::Frame(_))), "expected process loss at {point}: {died:?}");
        s.recover_and_reattach().await;
        let reply = s.mutate("inspect", op).await.unwrap();
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = reply else { panic!("{point}: {reply:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Interception, ref triggers } if triggers.len() == 1), "{point}: {result:?}");
        assert_eq!((start.t_us.0,end.t_us.0), (51_000,250_000), "{point}");
        let status = s.status().await;
        assert_eq!(status.held_requests.len(), 1, "{point}");
        let ordinal = &status.held_requests[0].ordinal;
        let trace = s.ws.runs().join(status.run_id.as_str()).join("trace.jsonl");
        assert!(!BehaviorLog::read(&trace).unwrap().iter().any(|e| matches!(&e.kind, EventKind::Response(r) if &r.id == ordinal)));
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("old-page", Op::Evaluate {
            expression: "({path:location.pathname,now:Date.now()-946684800000})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"path":"/count.html","now":250}), "{point}");
        s.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("new-page", Op::Evaluate {
            expression: "({url:location.href,arrivedAt})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"url":url,"arrivedAt":250}), "{point}");
        let events = BehaviorLog::read(&trace).unwrap();
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::Request(r) if &r.id == ordinal)).count(), 1, "{point}");
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::Interception(r) if &r.ordinal == ordinal)).count(), 1, "{point}");
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::Response(r) if &r.id == ordinal)).count(), 1, "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn focus_event_recovery_preserves_feedback_and_does_not_reuse_old_transitions() {
    use frontcap_core::event::{ClockOutcome, ProgressEvent};
    for point in ["after_dispatch", "after_clock", "after_effect", "after_complete"] {
        let op = Op::Next { event: ProgressEvent::Focus, max_us: 199_000 };
        let (answer, mut s) = fault_and_repeat("focus-stop.html", RuleSet::default(),
            vec![Op::Advance { by_us: 51_000 }], "next-focus", point, op).await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = answer else { panic!("{point}: {answer:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Focus, ref triggers } if triggers.len() == 2), "{point}: {result:?}");
        assert_eq!(start.t_us.0, 51_000);
        assert_eq!(end.t_us.0, 250_000, "{point}");
        assert!(s.text_is("focus-feedback", "#out", "saved").await);
        let Reply::Done { outcome: Outcome::Progress { result, end, .. }, .. } =
            s.mutate("next-focus-again", Op::Next { event: ProgressEvent::Focus, max_us: 300_000 }).await.unwrap() else { panic!() };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Focus, ref triggers } if triggers.len() == 2), "{point}: {result:?}");
        assert_eq!(end.t_us.0, 400_000);
        assert!(s.text_is("focus-again", "#out", "again").await);
        s.stop().await;
    }
}

#[tokio::test]
async fn dialog_event_recovery_keeps_the_stop_and_restores_configured_answers() {
    use frontcap_core::{event::{ClockOutcome, ProgressEvent}, log::BehaviorLog};
    use frontcap_recorder::runtime;
    use frontcap_recorder_protocol::DialogResponse;
    for point in ["while_open", "after_dispatch", "after_clock", "after_effect", "after_complete"] {
        let env = if point == "while_open" { vec![] } else { vec![("FRONTCAP_FAULT".into(), format!("next-dialog@{point}"))] };
        let mut s = Session::launch("dialog-stop.html", RuleSet::default(), &env).await;
        s.mutate("rules", Op::DialogRules { rules: serde_json::from_str(r#"[{"answer":"accept"}]"#).unwrap() }).await.unwrap();
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let run = s.status().await.run_id;
        let op = Op::Next { event: ProgressEvent::Dialog, max_us: 999_000 };
        let dir = s.ws.runtime_dir(&s.id);
        let owner = runtime::load_runtime(&dir).unwrap().unwrap();
        match s.mutate("next-dialog", op.clone()).await {
            Ok(Reply::Suspended { operation }) => {
                assert_eq!(operation.dialog.as_ref().unwrap().t_us.0, 250_000);
                if point == "while_open" {
                    owner.recorder.kill().unwrap();
                } else {
                    let reply = s.client.call(Call::AnswerDialog {
                        lease: s.lease.clone(), request_id: rid("dismiss-dialog"),
                        response: DialogResponse { parent_request_id: rid("next-dialog"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept: false, prompt_text: None },
                    }).await;
                    assert!(matches!(reply, Ok(Reply::DialogAnswered { .. }) | Err(ClientError::Closed | ClientError::Frame(_))), "{point}: {reply:?}");
                }
            }
            Err(ClientError::Closed | ClientError::Frame(_)) if point == "after_dispatch" => {},
            other => panic!("{point}: {other:?}"),
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while owner.recorder.is_alive() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.expect("the recorder must exit at the injected fault");
        s.recover_and_reattach().await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = s.mutate("next-dialog", op).await.unwrap() else { panic!("{point}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Dialog, ref triggers } if triggers.len() == 1), "{point}: {result:?}");
        assert_eq!(start.t_us.0, 51_000);
        assert_eq!(end.t_us.0, 250_000, "{point}");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("at-stop", Op::Evaluate {
            expression: "({answers,later,now:Date.now()-946684800000})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"answers":[false],"later":false,"now":250}), "{point}");
        let Reply::Done { outcome: Outcome::Progress { result, .. }, .. } =
            s.mutate("no-duplicate", Op::Next { event: ProgressEvent::Dialog, max_us: 20_000 }).await.unwrap() else { panic!() };
        assert_eq!(result, ClockOutcome::EventTimedOut { event: ProgressEvent::Dialog });
        s.mutate("automatic-again", Op::Advance { by_us: 180_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("answers", Op::Evaluate { expression: "answers".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!([false,true]), "{point}");
        let events = BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, frontcap_core::event::EventKind::Dialog(d) if d.message == "Save draft?")).count(), 1, "{point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn beforeunload_event_recovery_preserves_cancelled_and_committed_documents() {
    use frontcap_core::{action::Action, event::{ClockOutcome, ProgressEvent}};
    use frontcap_recorder::runtime;
    use frontcap_recorder_protocol::DialogResponse;
    for answer in [None, Some(false), Some(true)] {
        let env = answer.map(|_| vec![("FRONTCAP_FAULT".into(), "beforeunload-stop@after_clock".into())]).unwrap_or_default();
        let mut s = Session::launch("dialog-navigation-stop.html", RuleSet::default(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        s.mutate("edit", Op::Act { action: Action::Click { target: Locator::Css { selector: "#edit".into() } }, then: None, context: None }).await.unwrap();
        let owner = runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        let op = Op::Next { event: ProgressEvent::Dialog, max_us: 1_000_000 };
        let Reply::Suspended { operation } = s.mutate("beforeunload-stop", op.clone()).await.unwrap() else { panic!("{answer:?}") };
        assert_eq!(operation.dialog.as_ref().unwrap().t_us.0, 250_000);
        if let Some(accept) = answer {
            let result = s.client.call(Call::AnswerDialog { lease: s.lease.clone(), request_id: rid("leave-draft"), response: DialogResponse {
                parent_request_id: rid("beforeunload-stop"), dialog_id: operation.dialog.as_ref().unwrap().id.clone(), accept, prompt_text: None,
            }}).await;
            assert!(matches!(result, Ok(Reply::DialogAnswered { .. }) | Err(ClientError::Closed | ClientError::Frame(_))), "{answer:?}: {result:?}");
        } else { owner.recorder.kill().unwrap(); }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while owner.recorder.is_alive() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.expect("expected recorder exit");
        s.recover_and_reattach().await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = s.mutate("beforeunload-stop", op).await.unwrap() else { panic!("{answer:?}") };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Dialog, ref triggers } if triggers.len() == 1), "{answer:?}: {result:?}");
        assert_eq!(start.t_us.0, 51_000);
        assert_eq!(end.t_us.0, 250_000, "{answer:?}");
        let accept = answer == Some(true);
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("at-stop", Op::Evaluate {
            expression: "({path:location.pathname,later:window.later===true,started:window.appStarted===true,now:Date.now()-946684800000})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"path":if accept {"/dialog-arrived.html"} else {"/dialog-navigation-stop.html"},"later":false,"started":false,"now":250}), "{answer:?}");
        s.mutate("resume", Op::Advance { by_us: 20_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("after-stop", Op::Evaluate {
            expression: "({later:window.later===true,started:window.appStarted===true})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"later":!accept,"started":accept}));
        s.stop().await;
    }
}

#[tokio::test]
async fn cross_origin_dialog_event_recovery_preserves_chained_answers_and_source() {
    use frontcap_core::{event::{ClockOutcome, EventKind, ProgressEvent}, log::BehaviorLog};
    use frontcap_recorder::runtime;
    use frontcap_recorder_protocol::DialogResponse;
    for prompt in [None, Some("한글 🍎")] {
        let env = prompt.map(|_| vec![("FRONTCAP_FAULT".into(), "child-dialog@after_clock".into())]).unwrap_or_default();
        let parent = "<script>window.answers=[];window.later=false;addEventListener('message',e=>{if(e.origin!=='https://editor.other.test')return;answers.push(e.data);if(e.data?.save===false){answers.push(confirm('Parent archive?'));e.source.postMessage('follow-up',e.origin)}});setTimeout(()=>{later=true},251)</script><iframe src='https://editor.other.test/frame'></iframe>";
        let child = "<script>addEventListener('message',e=>{if(e.data==='follow-up')parent.postMessage({followup:prompt('Automatic child label','Automatic label')},'*')});setTimeout(()=>{alert('Draft ready');const save=confirm('Save draft?');const label=prompt('Draft label','Draft');parent.postMessage({save,label},'*')},250);setTimeout(()=>parent.postMessage(confirm('Automatic again'),'*'),400)</script>";
        let mocked: Vec<_> = [("main", "*/child-dialog.html", parent), ("child", "https://editor.other.test/frame", child)].into_iter().map(|(id,url,body)| serde_json::json!({
            "id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}
        })).collect();
        let mut s = Session::launch("child-dialog.html", rules(&serde_json::to_string(&mocked).unwrap()), &env).await;
        s.mutate("rules", Op::DialogRules { rules: serde_json::from_str(r#"[{"answer":"accept"}]"#).unwrap() }).await.unwrap();
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        let run = s.status().await.run_id;
        let owner = runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap();
        let op = Op::Next { event: ProgressEvent::Dialog, max_us: 1_000_000 };
        let Reply::Suspended { operation } = s.mutate("child-dialog", op.clone()).await.unwrap() else { panic!() };
        let source = operation.dialog.as_ref().unwrap().source.clone();
        assert_eq!(source.frame_path, [0]);
        assert_eq!(source.frame_urls.len(), 2);
        assert_eq!(source.frame_urls[1], "https://editor.other.test/frame");
        for (message, accept, request) in [("Draft ready", true, "alert-answer"), ("Save draft?", false, "confirm-answer"), ("Draft label", prompt.is_some(), "prompt-answer")] {
            let opening = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if let Some(held) = s.status().await.suspension
                        && held.dialog.as_ref().unwrap().message == message { break held.dialog.unwrap(); }
                    tokio::task::yield_now().await;
                }
            }).await.expect("next native child dialog must appear");
            assert_eq!(opening.t_us.0, 250_000);
            assert_eq!(opening.source, source);
            if message == "Draft label" && prompt.is_none() {
                owner.recorder.kill().unwrap();
            } else {
                let reply = s.client.call(Call::AnswerDialog { lease: s.lease.clone(), request_id: rid(request), response: DialogResponse {
                    parent_request_id: rid("child-dialog"), dialog_id: opening.id, accept,
                    prompt_text: if message == "Draft label" { prompt.map(str::to_string) } else { None },
                }}).await;
                assert!(matches!(reply, Ok(Reply::DialogAnswered { .. })) || (message == "Draft label" && matches!(reply, Err(ClientError::Closed | ClientError::Frame(_)))), "{prompt:?}: {reply:?}");
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while owner.recorder.is_alive() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.expect("expected recorder exit");
        s.recover_and_reattach().await;
        let Reply::Done { outcome: Outcome::Progress { result, start, end }, .. } = s.mutate("child-dialog", op).await.unwrap() else { panic!() };
        assert!(matches!(result, ClockOutcome::Event { event: ProgressEvent::Dialog, ref triggers } if triggers.len() == 3), "{prompt:?}: {result:?}");
        assert_eq!(start.t_us.0, 51_000);
        assert_eq!(end.t_us.0, 250_000);
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("at-stop", Op::Evaluate {
            expression: "({answers,later,now:Date.now()-946684800000})".into(),
        }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!({"answers":[],"later":false,"now":250}), "{prompt:?}");
        s.mutate("resume", Op::Advance { by_us: 200_000 }).await.unwrap();
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("after-stop", Op::Evaluate { expression: "answers".into() }).await.unwrap() else { panic!() };
        assert_eq!(value, serde_json::json!([{"save":false,"label":prompt},true,{"followup":"Automatic label"},true]), "{prompt:?}");
        let events = BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
        let answers: Vec<_> = events.iter().filter_map(|e| if let EventKind::Dialog(d) = &e.kind { Some(d) } else { None }).collect();
        assert_eq!(answers.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(), ["Draft ready", "Save draft?", "Draft label", "Parent archive?", "Automatic child label", "Automatic again"]);
        assert!(answers[3].native.is_none() && answers[3].observer.is_some(), "parent automatic answer uses its wrapper");
        assert!(answers[4].native.is_some() && answers[4].observer.is_none(), "cross-origin follow-up uses native evidence");
        assert_eq!(answers[2].answer, prompt.map(|p| serde_json::to_string(p).unwrap()).unwrap_or_else(|| "null".into()));
        s.stop().await;
    }
}

#[tokio::test]
async fn rule_publication_faults_resolve_one_installation_without_reapplying() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    for (point, published) in [
        ("before_dispatch", false), ("after_dispatch", false), ("before_rules_publish", false),
        ("after_rules_publish", true), ("after_engine", true), ("after_effect", true), ("after_complete", true),
    ] {
        let env = [("FRONTCAP_FAULT".into(), format!("install@{point}"))];
        let mut s = Session::launch("count.html", RuleSet::default(), &env).await;
        s.mutate("ready", Op::Advance { by_us: 50_000 }).await.unwrap();
        let Reply::Observed { result: Observation::NetworkRules(before) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
        let rules = RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
        let op = Op::SetRules { expected_revision: 0, boundary: before.boundary.clone(), rules };
        assert!(s.mutate("install", op.clone()).await.is_err(), "{point} must abort");
        s.recover_and_reattach().await;
        let reply = s.mutate("install", op).await.unwrap();
        if published {
            assert!(matches!(&reply, Reply::Done { outcome: Outcome::RulesInstalled(receipt), .. } if receipt.revision == 1), "{point}: {reply:?}");
        } else {
            assert!(matches!(reply, Reply::Done { outcome: Outcome::NotApplied, .. }), "{point}: {reply:?}");
        }
        let Reply::Observed { result: Observation::NetworkRules(after) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
        assert_eq!(after.revision, u64::from(published), "{point}");
        assert_eq!(after.boundary.t_us, before.boundary.t_us, "{point}");
        let trace = frontcap_core::log::BehaviorLog::read(&s.ws.runs().join(launcher::run_id_of(&s.id).as_str()).join("trace.jsonl")).unwrap();
        assert_eq!(trace.iter().filter(|e|matches!(e.kind, frontcap_core::event::EventKind::RulesInstalled(_))).count(), 1+usize::from(published), "{point}");
        if !published {
            let next = Op::SetRules { expected_revision: 0, boundary: after.boundary, rules: RuleSet::default() };
            assert!(matches!(s.mutate("new-install", next).await.unwrap(), Reply::Done { outcome: Outcome::RulesInstalled(receipt), .. } if receipt.revision==1));
            let Reply::Observed { result: Observation::NetworkRules(next) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
            assert!(!next.rules.rules.iter().any(|r|r.id.as_str()=="api"), "unpublished prepared rules must not survive the new install");
        }
        s.stop().await;
    }
}

#[tokio::test]
async fn stream_calls_survive_publication_loss_and_source_document_removal() {
    use frontcap_core::{event::{EventKind,StreamDirection,StreamEventKind},log::BehaviorLog};
    for removed in [false,true] {
        for point in ["before_trace","after_close_request","after_trace_sync","after_forget"] {
            let child=format!("<!doctype html><script>const s=new WebSocket('wss://api.example.test/feed');setTimeout(()=>{{s.send('retained-send');s.close();{}}},250)</script>",
                if removed {"frameElement.remove();"} else {""});
            let rules=RuleSet::from_json(&serde_json::json!([
                {"id":"parent","match":{"url":"http://*/stream-receipt.html"},"action":{"type":"mock","status":200,
                 "headers":{"content-type":"text/html"},"body":"<!doctype html><iframe src='/stream-receipt-child.html'></iframe>"}},
                {"id":"child","match":{"url":"http://*/stream-receipt-child.html"},"action":{"type":"mock","status":200,
                 "headers":{"content-type":"text/html"},"body":child}},
                {"id":"feed","match":{"url":"wss://api.example.test/feed"},"action":{"type":"stream_mock","messages":[]}}
            ]).to_string()).unwrap();
            let env=[("FRONTCAP_STREAM_FAULT".into(),point.into())];
            let mut s=Session::launch("stream-receipt.html",rules,&env).await;
            let run=s.status().await.run_id;
            s.mutate("setup",Op::Advance {by_us:51_000}).await.unwrap();
            let first=s.mutate("calls",Op::Advance {by_us:250_000}).await;
            if first.is_ok() && point=="after_forget" {
                assert!(s.mutate("ack",Op::Advance {by_us:20_000}).await.is_err());
            } else {assert!(first.is_err(),"{point}: {first:?}");}
            s.recover_and_reattach().await;
            s.mutate("drain",Op::Advance {by_us:20_000}).await.unwrap();
            let trace=s.ws.runs().join(run.as_str()).join("trace.jsonl");
            let events=BehaviorLog::read(&trace).unwrap();
            let outgoing:Vec<_>=events.iter().filter_map(|event|match &event.kind {
                EventKind::Stream(stream) if stream.direction==StreamDirection::Out=>Some((event.t_us,stream)),_=>None,
            }).collect();
            assert_eq!(outgoing.len(),2,"{point}, removed={removed}: {outgoing:?}");
            assert_eq!(outgoing[0].1.event,StreamEventKind::Message);
            assert_eq!(outgoing[0].1.data.as_deref(),Some("retained-send"));
            assert_eq!(outgoing[1].1.event,StreamEventKind::Close);
            assert_eq!(outgoing[0].1.stream,outgoing[1].1.stream);
            assert!(outgoing.iter().all(|(t,stream)|t.0==250_000 && stream.receipt.is_some()));
            assert_eq!(events.iter().filter(|event|matches!(&event.kind,EventKind::Stream(stream)
                if stream.direction==StreamDirection::In && stream.event==StreamEventKind::Close)).count(),1);
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn stream_admission_recovers_after_open_delivery_before_trace_publication() {
    for point in ["before_stream_binding","after_stream_binding","before_open_delivery","after_open_delivery"] {
    use frontcap_core::{event::{EventKind,StreamEventKind},log::BehaviorLog};
    let rules=RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"http://*/stream-admission.html"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><output id='result'>0/0</output><script>let opens=0,messages=0;setTimeout(()=>{const s=new WebSocket('wss://api.example.test/feed');s.onopen=()=>{opens++;document.querySelector('#result').textContent=opens+'/'+messages};s.onmessage=()=>{messages++;document.querySelector('#result').textContent=opens+'/'+messages}},250)</script>"}},
        {"id":"feed","match":{"url":"wss://api.example.test/feed"},"action":{"type":"stream_mock","messages":[{"at_ms":150,"data":"later"}]}}
    ]).to_string()).unwrap();
    let mut s=Session::launch("stream-admission.html",rules,&[("FRONTCAP_STREAM_FAULT".into(),point.into())]).await;
    let run=s.status().await.run_id;
    s.mutate("setup",Op::Advance {by_us:51_000}).await.unwrap();
    assert!(s.mutate("open",Op::Advance {by_us:250_000}).await.is_err());
    s.recover_and_reattach().await;
    s.mutate("later",Op::Advance {by_us:500_000}).await.unwrap();
    assert!(s.text_is("result","#result","1/1").await,"open must not repeat and the selected future message must survive");
    let events=BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
    let opens:Vec<_>=events.iter().filter(|event|matches!(&event.kind,EventKind::Stream(stream) if stream.event==StreamEventKind::Open)).collect();
    assert_eq!(opens.len(),1,"the already delivered opening must be reconstructed exactly once");
    assert_eq!(opens[0].t_us.0,250_000);
    s.stop().await;
    }
}

#[tokio::test]
async fn stream_opening_receipt_survives_its_callback_removing_the_source_frame() {
    for point in ["before_open_delivery", "after_open_delivery"] {
    use frontcap_core::{event::{EventKind,StreamEventKind},log::BehaviorLog};
    let rules=RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"http://*/stream-removal.html"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><output id='result'>0</output><iframe src='/stream-removal-child.html'></iframe>"}},
        {"id":"child","match":{"url":"http://*/stream-removal-child.html"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>setTimeout(()=>{const s=new WebSocket('wss://api.example.test/feed');s.onopen=()=>{parent.document.querySelector('#result').textContent='1';frameElement.remove()}},250)</script>"}},
        {"id":"feed","match":{"url":"wss://api.example.test/feed"},"action":{"type":"stream_mock","messages":[]}}
    ]).to_string()).unwrap();
    let mut s=Session::launch("stream-removal.html",rules,&[("FRONTCAP_STREAM_FAULT".into(),point.into())]).await;
    let run=s.status().await.run_id;
    s.mutate("setup",Op::Advance {by_us:51_000}).await.unwrap();
    assert!(s.mutate("open",Op::Advance {by_us:250_000}).await.is_err());
    s.recover_and_reattach().await;
    assert!(s.text_is("result","#result","1").await);
    let events=BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
    let opens:Vec<_>=events.iter().filter_map(|event|match &event.kind {
        EventKind::Stream(stream) if stream.event==StreamEventKind::Open=>Some((event.t_us,stream)),_=>None,
    }).collect();
    assert_eq!(opens.len(),1);
    assert_eq!(opens[0].0.0,250_000);
    assert!(opens[0].1.receipt.is_some());
    s.stop().await;
    }
}

#[tokio::test]
async fn early_stream_close_recovers_its_failed_handshake_and_receipt() {
    use frontcap_core::{event::{EventKind,StreamDirection,StreamEventKind},log::BehaviorLog};
    for websocket in [false,true] {
        for point in ["before_trace","after_close_request","after_trace_sync","after_forget"] {
            let url=if websocket {"wss://api.example.test/feed"} else {"https://api.example.test/feed"};
            let constructor=if websocket {"WebSocket"} else {"EventSource"};
            let body=format!("<!doctype html><script>setTimeout(()=>{{const s=new {constructor}('{url}');s.close()}},250)</script>");
            let rules=RuleSet::from_json(&serde_json::json!([
                {"id":"page","match":{"url":"http://*/early-close-fault.html"},"action":{"type":"mock","status":200,
                 "headers":{"content-type":"text/html"},"body":body}},
                {"id":"feed","match":{"url":url},"action":{"type":"stream_mock","messages":[]}}
            ]).to_string()).unwrap();
            let mut s=Session::launch("early-close-fault.html",rules,&[("FRONTCAP_STREAM_FAULT".into(),point.into())]).await;
            let run=s.status().await.run_id;
            s.mutate("setup",Op::Advance {by_us:51_000}).await.unwrap();
            let first=s.mutate("close",Op::Advance {by_us:250_000}).await;
            if point=="after_forget" && first.is_ok() {
                assert!(s.mutate("ack",Op::Advance {by_us:20_000}).await.is_err());
            } else { assert!(first.is_err(),"{point}: {first:?}"); }
            s.recover_and_reattach().await;
            s.mutate("drain",Op::Advance {by_us:20_000}).await.unwrap();
            let events=BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
            let requests:Vec<_>=events.iter().filter_map(|event|match &event.kind {
                EventKind::Request(request) if request.url==url=>Some(request),_=>None,
            }).collect();
            assert_eq!(requests.len(),1,"{point}: {requests:?}");
            let ordinal=&requests[0].id;
            let failed:Vec<_>=events.iter().filter_map(|event|match &event.kind {
                EventKind::Response(response) if &response.id==ordinal=>Some(response),_=>None,
            }).collect();
            assert_eq!(failed.len(),1);
            assert_eq!(failed[0].status,0);
            let streams:Vec<_>=events.iter().filter_map(|event|match &event.kind {
                EventKind::Stream(stream) if &stream.stream==ordinal=>Some((event.t_us,stream)),_=>None,
            }).collect();
            assert_eq!(streams.len(),if websocket {2} else {1},"{point}: {streams:?}");
            assert!(streams.iter().all(|(at,stream)|at.0==250_000 && stream.event==StreamEventKind::Close && stream.receipt.is_some()));
            assert_eq!(streams.iter().filter(|(_,stream)|stream.direction==StreamDirection::Out).count(),1);
            s.stop().await;
        }
    }
}

#[tokio::test]
async fn interrupted_stream_connections_are_not_retried_on_recovery() {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use frontcap_core::{event::{EventKind, StreamEventKind}, log::BehaviorLog};
    for websocket in [false, true] {
    for point in ["before_upstream_connect", "after_upstream_connect", "before_stream_cassette", "after_stream_cassette", "after_open_delivery"] {
        for recording in [false, true] {
            if point.contains("cassette") && !recording { continue; }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let feed = format!("{}://{}/feed", if websocket { "ws" } else { "http" }, listener.local_addr().unwrap());
            let count = Arc::new(AtomicUsize::new(0));
            let accepted = count.clone();
            let server = tokio::spawn(async move {
                let mut sockets = tokio::task::JoinSet::new();
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    accepted.fetch_add(1, Ordering::SeqCst);
                    sockets.spawn(async move {
                        if websocket {
                            use futures_util::StreamExt;
                            if let Ok(mut stream) = tokio_tungstenite::accept_async(socket).await {
                                while stream.next().await.is_some() {}
                            }
                            return;
                        }
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while !request.ends_with(b"\r\n\r\n") {
                            if socket.read_exact(&mut byte).await.is_err() { return; }
                            request.push(byte[0]);
                            assert!(request.len() < 16384);
                        }
                        if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: welcome\n\n").await.is_err() { return; }
                        let _ = socket.read(&mut byte).await;
                    });
                }
            });
            struct StopServer(tokio::task::AbortHandle);
            impl Drop for StopServer { fn drop(&mut self) { self.0.abort(); } }
            let _server = StopServer(server.abort_handle());
            let constructor = if websocket { "WebSocket" } else { "EventSource" };
            let body = format!("<!doctype html><output id='result'>waiting</output><script>setTimeout(()=>{{const s=new {constructor}('{feed}');s.onopen=()=>document.querySelector('#result').textContent='opened';s.onclose=()=>document.querySelector('#result').textContent='lost';s.onerror=()=>{{s.close();document.querySelector('#result').textContent='lost'}}}},250)</script>");
            let action = if recording { serde_json::json!({"type":"record","cassette":"feed"}) }
                else { serde_json::json!({"type":"passthrough"}) };
            let rules = RuleSet::from_json(&serde_json::json!([
                {"id":"page","match":{"url":"http://*/connection-loss.html"},"action":{"type":"mock","status":200,
                 "headers":{"content-type":"text/html"},"body":body}},
                {"id":"feed","match":{"url":feed},"action":action}
            ]).to_string()).unwrap();
            let mut s = Session::launch("connection-loss.html", rules, &[("FRONTCAP_STREAM_FAULT".into(),point.into())]).await;
            let run = s.status().await.run_id;
            s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
            assert!(s.mutate("connect", Op::Advance { by_us: 250_000 }).await.is_err());
            if point.contains("cassette") {
                // Stop recovery again after cassette repair/publication, then recover once more.
                assert!(launcher::recover(&s.ws, &s.id, &support::recorder_bin(),
                    &[("FRONTCAP_STREAM_FAULT".into(), "after_stream_cassette".into())]).await.is_err());
            }
            s.recover_and_reattach().await;
            s.mutate("settle", Op::Advance { by_us: 500_000 }).await.unwrap();
            assert!(s.text_is("lost", "#result", "lost").await);
            assert_eq!(count.load(Ordering::SeqCst), usize::from(point != "before_upstream_connect"), "recovery must not retry an ambiguous connection");
            let events = BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
            assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Request(request) if request.url == feed)).count(), 1);
            let opened = point == "after_open_delivery" || point.contains("cassette");
            assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Stream(stream) if stream.event == StreamEventKind::Error)).count(), usize::from(!opened));
            assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Stream(stream) if stream.event == StreamEventKind::Open)).count(), usize::from(opened));
            if !opened {
                assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Taint(taint) if taint.detail.contains("outcome unknown, connection not retried"))));
            }
            if recording {
                let cassette = std::fs::read_to_string(s.ws.runs().join(run.as_str()).join("cassettes/feed.jsonl")).unwrap();
                let (_, lines) = frontcap_core::cassette::parse_all(&cassette).unwrap();
                let handshakes: Vec<_> = lines.iter().filter(|line| matches!(line.event.as_str(), "open" | "error")).collect();
                assert_eq!(handshakes.len(), 1, "repeated recovery must preserve one cassette handshake");
                assert_eq!(handshakes[0].event, if opened { "open" } else { "error" });
                assert_eq!(handshakes[0].session, 0);
            }
            s.stop().await;
        }
    }
    }
}

#[tokio::test]
async fn replay_stream_selection_survives_repeated_recorder_loss_without_consuming_a_sibling_session() {
    use frontcap_core::{cassette::{Key, StreamEvent}, event::{EventKind, StreamEventKind}, log::BehaviorLog};
    for cross_origin in [false, true] {
    for repeat_last in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let cassette = temp.path().join("feed.jsonl");
        let url = "wss://api.example.test/replayed";
        let mut lines = Vec::new();
        for (session, data) in [(0, "first"), (1, "second")] {
            for (at_us, event, data) in [(0, "open", None), (50_000, "message", Some(data.to_string()))] {
                lines.push(StreamEvent { key: Key::of("GET", url, None), session, at_us,
                    event: event.into(), data, sse_event: None, sse_id: None, code: None }.to_line());
            }
        }
        std::fs::write(&cassette, lines.join("\n") + "\n").unwrap();
        let mut body = format!("<!doctype html><output id='result'></output><script>let messages=[],errors=0;const render=()=>document.querySelector('#result').textContent=messages.join(',')+'|'+errors;for(const at of [250,500,750])setTimeout(()=>{{const s=new WebSocket('{url}');s.onmessage=e=>{{messages.push(e.data);render()}};s.onerror=()=>{{errors++;render()}}}},at)</script>");
        if cross_origin {
            body = "<!doctype html><output id='result'></output><script>let messages=[],errors=0,origins=new Set();onmessage=e=>{origins.add(e.origin);if(e.data.error)errors++;else messages.push(e.data.message);document.querySelector('#result').textContent=messages.sort().join(',')+'|'+errors+'|'+origins.size}</script><iframe src='http://child-a.example.test/replay-owner'></iframe><iframe src='http://child-b.example.test/replay-owner'></iframe><iframe src='http://child-c.example.test/replay-owner'></iframe>".into();
        }
        let child = format!("<!doctype html><script>setTimeout(()=>{{const s=new WebSocket('{url}');s.onmessage=e=>parent.postMessage({{message:e.data}},'*');s.onerror=()=>parent.postMessage({{error:true}},'*')}},250)</script>");
        let rules = RuleSet::from_json(&serde_json::json!([
            {"id":"page","match":{"url":"http://*/replay-selection.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}},
            {"id":"child","match":{"url":"http://*/replay-owner"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child}},
            {"id":"feed","match":{"url":url},"action":{"type":"replay","cassette":cassette,"after_last":if repeat_last {"repeat_last"} else {"fail"}}}
        ]).to_string()).unwrap();
        let fault = [("FRONTCAP_STREAM_FAULT".into(), "after_replay_selection".into())];
        let mut s = Session::launch("replay-selection.html", rules, &fault).await;
        let run = s.status().await.run_id;
        s.mutate("setup", Op::Advance { by_us: 51_000 }).await.unwrap();
        assert!(s.mutate("select", Op::Advance { by_us: 250_000 }).await.is_err());
        assert!(launcher::recover(&s.ws, &s.id, &support::recorder_bin(), &fault).await.is_err());
        s.recover_and_reattach().await;
        s.mutate("siblings", Op::Advance { by_us: 1_000_000 }).await.unwrap();
        let expected = if repeat_last { "first,second,second|0" } else { "first,second|1" };
        let expected = if cross_origin { format!("{expected}|3") } else { expected.into() };
        assert!(s.text_is("result", "#result", &expected).await);
        let events = BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
        let entries: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            EventKind::Request(request) if request.url == url => Some(request.entry), _ => None,
        }).collect();
        assert_eq!(entries, vec![Some(0), Some(1), if repeat_last { Some(1) } else { None }]);
        let first = events.iter().find(|event| matches!(&event.kind, EventKind::Stream(stream)
            if stream.event == StreamEventKind::Message && stream.data.as_deref() == Some("first"))).unwrap();
        assert_eq!(first.t_us.0, 300_000, "recovery must preserve the selected session's time base");
        s.stop().await;
    }
    }
}

#[tokio::test]
async fn source_removal_before_stream_admission_preserves_a_living_sibling() {
    use frontcap_core::{event::{EventKind, StreamEventKind}, log::BehaviorLog};
    // `late`: the browser reports the removed stream's anchor after the page's own events have decided it.
    for (recover, late) in [(false, false), (true, false), (true, true)] {
    for close_first in [false, true] {
        let disappearing = format!("<!doctype html><script>setTimeout(()=>{{const s=new WebSocket('wss://api.example.test/removed');{}frameElement.remove()}},250)</script>", if close_first { "s.close();" } else { "" });
        let rules = RuleSet::from_json(&serde_json::json!([
            {"id":"page","match":{"url":"http://*/unadmitted-removal.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
             "body":"<!doctype html><output id='result'>waiting</output><iframe src='/vanishing'></iframe><iframe src='/living'></iframe>"}},
            {"id":"vanishing","match":{"url":"http://*/vanishing"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":disappearing}},
            {"id":"living","match":{"url":"http://*/living"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
             "body":"<!doctype html><script>setTimeout(()=>{const s=new WebSocket('wss://api.example.test/living');s.onmessage=e=>parent.document.querySelector('#result').textContent=e.data},250)</script>"}},
            {"id":"streams","match":{"url":"wss://api.example.test/*"},"action":{"type":"stream_mock","messages":[{"at_ms":100,"data":"alive"}]}}
        ]).to_string()).unwrap();
        let mut faults = if recover { vec![("FRONTCAP_STREAM_FAULT".into(), "after_stream_abandoned".into())] } else { vec![] };
        if late {
            faults.push(("FRONTCAP_INTERCEPTION_DELAY".into(), "api.example.test/removed@500".into()));
        }
        let mut s = Session::launch("unadmitted-removal.html", rules, &faults).await;
        let run = s.status().await.run_id;
        let advanced = s.mutate("advance", Op::Advance { by_us: 500_000 }).await;
        if recover {
            assert!(advanced.is_err());
            s.recover_and_reattach().await;
            s.mutate("resume", Op::Advance { by_us: 500_000 }).await.unwrap();
        } else { advanced.unwrap(); }
        assert!(s.text_is("alive", "#result", "alive").await);
        let events = BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
        let living = events.iter().find_map(|event| match &event.kind {
            EventKind::Request(request) if request.url == "wss://api.example.test/living" => Some(request.id.clone()), _ => None,
        }).unwrap();
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Stream(stream)
            if stream.stream == living && stream.event == StreamEventKind::Message && stream.data.as_deref() == Some("alive"))).count(), 1);
        let removed: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            EventKind::Request(request) if request.url == "wss://api.example.test/removed" => Some(request), _ => None,
        }).collect();
        assert_eq!(removed.len(), 1);
        let removed = &removed[0].id;
        assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventKind::Response(response)
            if &response.id == removed && response.status == 0)).count(), 1);
        assert!(!events.iter().any(|event| matches!(&event.kind, EventKind::Stream(stream)
            if &stream.stream == removed && stream.event == StreamEventKind::Open)));
        let closes: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            EventKind::Stream(stream) if &stream.stream == removed && stream.event == StreamEventKind::Close
                && stream.direction == frontcap_core::event::StreamDirection::Out => Some(stream), _ => None,
        }).collect();
        assert_eq!(closes.len(), usize::from(close_first));
        assert!(closes.iter().all(|stream| stream.receipt.is_some()));
        s.stop().await;
    }
    }
}

#[tokio::test]
async fn a_replacement_document_does_not_inherit_its_predecessors_stream() {
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"http://*/stream-replacement.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><output id='result'>waiting</output><iframe src='/predecessor'></iframe>"}},
        {"id":"predecessor","match":{"url":"http://*/predecessor"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>setTimeout(()=>{new WebSocket('wss://api.example.test/old');location.replace('/replacement')},250)</script>"}},
        {"id":"replacement","match":{"url":"http://*/replacement"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><script>const s=new WebSocket('wss://api.example.test/new');s.onmessage=e=>parent.document.querySelector('#result').textContent=e.data</script>"}},
        {"id":"old","match":{"url":"wss://api.example.test/old"},"action":{"type":"stream_mock","messages":[{"at_ms":100,"data":"old"}]}},
        {"id":"new","match":{"url":"wss://api.example.test/new"},"action":{"type":"stream_mock","messages":[{"at_ms":100,"data":"new"}]}}
    ]).to_string()).unwrap();
    let mut s = Session::launch("stream-replacement.html", rules, &[]).await;
    s.mutate("advance", Op::Advance { by_us: 750_000 }).await.unwrap();
    assert!(s.text_is("replacement", "#result", "new").await);
    s.stop().await;
}

#[tokio::test]
async fn rule_edits_recover_before_and_after_publication() {
    use frontcap_recorder_protocol::{Observation, ObserveOp};
    for remove in [false, true] {
    for (point, published) in [("before_rules_publish", false), ("after_rules_publish", true)] {
        let env = [("FRONTCAP_FAULT".into(), format!("install@{point}"))];
        let initial = if remove { RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap() } else { RuleSet::default() };
        let mut s = Session::launch("count.html", initial, &env).await;
        s.mutate("ready", Op::Advance { by_us: 50_000 }).await.unwrap();
        let Reply::Observed { result: Observation::NetworkRules(before) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
        let rules = RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
        let edit = if remove { frontcap_core::rules::RuleEdit::Remove { id: frontcap_core::ids::RuleId::new("api").unwrap() } }
            else { frontcap_core::rules::RuleEdit::Add { rule: Box::new(rules.rules[0].clone()), before: None } };
        let op = Op::EditRules { expected_revision: 0, boundary: before.boundary.clone(), edit };
        assert!(s.mutate("install", op.clone()).await.is_err(), "{point} must abort");
        s.recover_and_reattach().await;
        let reply = s.mutate("install", op).await.unwrap();
        if published {
            assert!(matches!(&reply, Reply::Done { outcome: Outcome::RulesInstalled(receipt), .. } if receipt.revision == 1), "{point}: {reply:?}");
        } else {
            assert!(matches!(reply, Reply::Done { outcome: Outcome::NotApplied, .. }), "{point}: {reply:?}");
        }
        let Reply::Observed { result: Observation::NetworkRules(after) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
        assert_eq!(after.revision, u64::from(published), "{point}");
        assert_eq!(after.rules.rules.iter().any(|rule| rule.id.as_str() == "api"), remove != published);
        assert_eq!(after.boundary.t_us, before.boundary.t_us, "{point}");
        let trace = frontcap_core::log::BehaviorLog::read(&s.ws.runs().join(launcher::run_id_of(&s.id).as_str()).join("trace.jsonl")).unwrap();
        assert_eq!(trace.iter().filter(|e|matches!(e.kind, frontcap_core::event::EventKind::RulesInstalled(_))).count(), 1+usize::from(published), "{point}");
        if !published {
            let next = Op::SetRules { expected_revision: 0, boundary: after.boundary, rules: RuleSet::default() };
            assert!(matches!(s.mutate("new-install", next).await.unwrap(), Reply::Done { outcome: Outcome::RulesInstalled(receipt), .. } if receipt.revision==1));
            let Reply::Observed { result: Observation::NetworkRules(next) } = s.client.call(Call::Observe { op: ObserveOp::NetworkRules, context: None }).await.unwrap() else { panic!() };
            assert!(!next.rules.rules.iter().any(|r|r.id.as_str()=="api"), "unpublished prepared rules must not survive the new install");
        }
        s.stop().await;
    }
    }
}

#[tokio::test]
async fn native_indexed_db_commit_recovers_without_another_generated_key_or_time_progress() {
    use frontcap_core::intervention::{StateChange, StateField};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    for point in ["state_before_write_0", "state_native_written_0", "state_after_write_0"] {
        let env=[("FRONTCAP_FAULT".to_string(),format!("record-write@{point}"))];
        let mut s=Session::launch("idb-records.html",RuleSet::default(),&env).await;
        s.mutate("load",Op::Advance {by_us:50_000}).await.unwrap();
        assert!(s.text_is("ready", "#status", "ready").await);
        let target=Locator::Css {selector:"#status".into()};
        let Reply::Observed {result:Observation::StorageScope(scope)}=s.client.call(Call::Observe {
            op:ObserveOp::StorageScope {target:target.clone()},context:None}).await.unwrap() else {panic!()};
        let field=StateField::IndexedDbRecord {scope:scope.scope,bucket:None,database:"records".into(),store:"items".into(),key:serde_json::json!(5)};
        let value=serde_json::json!({"state":"present","value":{"label":"prepared"}});
        let op=Op::StateApply {boundary:scope.boundary,changes:vec![StateChange {target:target.clone(),field:field.clone(),value:value.clone(),expected:Some(frontcap_core::intervention::ExpectedValue {value:serde_json::json!({"state":"absent"})})}],context:None,progress_us:None};
        assert!(s.mutate("record-write",op.clone()).await.is_err(),"{point}");
        s.recover_and_reattach().await;
        let Reply::Done {outcome:Outcome::StateApplied(applied),..}=s.mutate("record-write",op).await.unwrap() else {panic!()};
        assert!(applied.completed,"{point}: {applied:?}");
        assert_eq!(applied.boundary.t_us.0,50_000);
        let Reply::Observed {result:Observation::State(read)}=s.client.call(Call::Observe {op:ObserveOp::StateRead {target,field},context:None}).await.unwrap() else {panic!()};
        assert_eq!(read.value,value);
        s.mutate("next",Op::Evaluate {expression:"var added=db.transaction('items','readwrite').objectStore('items').add('next'); added.onsuccess=()=>globalThis.nextKey=added.result; true".into()}).await.unwrap();
        s.mutate("deliver",Op::Advance {by_us:50_000}).await.unwrap();
        let Reply::Done {outcome:Outcome::Evaluated {value},..}=s.mutate("key",Op::Evaluate {expression:"nextKey".into()}).await.unwrap() else {panic!()};
        assert_eq!(value,serde_json::json!(6),"retry must not advance the key generator: {point}");
        s.stop().await;
    }
}

#[tokio::test]
async fn native_cache_write_recovers_at_each_commit_boundary_without_removing_another_entry() {
    use frontcap_core::intervention::{StateChange, StateField};
    use frontcap_recorder_protocol::{ObserveOp, Observation};
    for point in ["state_before_write_0", "state_native_written_0", "state_after_write_0"] {
        let env=[("FRONTCAP_FAULT".to_string(),format!("cache-write@{point}"))];
        let mut s=Session::launch("state-counter.html",RuleSet::default(),&env).await;
        s.mutate("load",Op::Advance {by_us:50_000}).await.unwrap();
        let target=Locator::Css {selector:"#calls".into()};
        let Reply::Observed {result:Observation::StorageScope(scope)}=s.client.call(Call::Observe {
            op:ObserveOp::StorageScope {target:target.clone()},context:None}).await.unwrap() else {panic!()};
        let field=|lang:&str|StateField::CacheEntry {scope:scope.scope.clone(),bucket:None,cache:"variants".into(),request_url:format!("{}/item-{lang}",scope.scope.origin),request_headers:std::collections::BTreeMap::from([("accept-language".into(),lang.into())])};
        let value=serde_json::json!({"state":"present","response":{"status":200,"status_text":"","headers":{"vary":"accept-language"},"response_type":"default","body_base64":"AP+A"}});
        let sibling=StateChange {target:target.clone(),field:field("en"),value:value.clone(),expected:None};
        let Reply::Done {outcome:Outcome::StateApplied(seeded),..}=s.mutate("seed",Op::StateApply {boundary:scope.boundary,changes:vec![sibling],context:None,progress_us:None}).await.unwrap() else {panic!()};
        assert!(seeded.completed);
        let op=Op::StateApply {boundary:seeded.boundary,changes:vec![StateChange {target:target.clone(),field:field("ko"),value:value.clone(),expected:Some(frontcap_core::intervention::ExpectedValue {value:serde_json::json!({"state":"absent"})})}],context:None,progress_us:None};
        assert!(s.mutate("cache-write",op.clone()).await.is_err(),"{point}");
        s.recover_and_reattach().await;
        let Reply::Done {outcome:Outcome::StateApplied(applied),..}=s.mutate("cache-write",op).await.unwrap() else {panic!()};
        assert!(applied.completed,"{point}: {applied:?}");
        assert_eq!(applied.boundary.t_us.0,50_000);
        for lang in ["en","ko"] {
            let Reply::Observed {result:Observation::State(read)}=s.client.call(Call::Observe {op:ObserveOp::StateRead {target:target.clone(),field:field(lang)},context:None}).await.unwrap() else {panic!()};
            assert!(read.value["response"]["metadata"].is_object());
            let mut semantic=read.value;
            semantic["response"].as_object_mut().unwrap().remove("metadata");
            assert_eq!(semantic,value,"{point}: {lang}");
        }
        s.stop().await;
    }
}

#[tokio::test]
async fn bucket_and_cookie_expiration_survive_recorder_replacement() {
    let page = r#"<!doctype html><output id="ready"></output><output id="before"></output><output id="after"></output><script>
    (async()=>{
      const bucket=await navigator.storageBuckets.open('expiring',{expires:4102444802000});
      const cache=await bucket.caches.open('items');
      await cache.put('/item',new Response('kept'));
      document.cookie='same=kept; Expires='+new Date(4102444802000).toUTCString()+'; Path=/';
      document.querySelector('#ready').textContent=await(await cache.match('/item')).text();
      for(const [at,id] of [[1999,'before'],[2000,'after']])setTimeout(async()=>{
        document.querySelector('#'+id).textContent=(await navigator.storageBuckets.keys()).join(',')+'|'+document.cookie;
      },Math.ceil(at-performance.now()));
    })();</script>"#;
    let mut outcomes = Vec::new();
    for point in ["after_effect", "after_complete"] {
        let addr = support::serve();
        let url = format!("http://{addr}/quota-clock.html");
        let (tmp, ws) = support::workspace();
        let mut request = support::launch_request("launch", &url);
        request.profile.initial_date_ms = 4102444800000;
        request.rules=RuleSet::from_json(&serde_json::json!([{"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]).to_string()).unwrap();
        let env = [("FRONTCAP_FAULT".to_string(), format!("clock-step@{point}"))];
        let a = launcher::launch(&ws, request, &support::recorder_bin(), &env)
            .await
            .unwrap();
        let guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        let mut s = Session {
            _guard: guard,
            _tmp: tmp,
            ws: ws.clone(),
            id: a.launch_id,
            client: a.client,
            lease: a.lease,
        };
        s.mutate("load", Op::Advance { by_us: 100_000 })
            .await
            .unwrap();
        assert!(s.text_is("ready", "#ready", "kept").await, "{point}");
        let step = Op::Advance { by_us: 900_000 };
        assert!(
            s.mutate("clock-step", step.clone()).await.is_err(),
            "fault must fire: {point}"
        );
        s.recover_and_reattach().await;
        s.mutate("clock-step", step).await.unwrap();
        assert_eq!(
            s.status().await.t_us.0,
            1_000_000,
            "{point}: recovery must not repeat elapsed time"
        );
        s.mutate("past-expiry", Op::Advance { by_us: 1_001_000 })
            .await
            .unwrap();
        let kept_before = s
            .text_is("kept-before", "#before", "expiring|same=kept")
            .await;
        let Reply::Observed {
            result: frontcap_recorder_protocol::Observation::State(after),
        } = s
            .client
            .call(Call::Observe {
                op: frontcap_recorder_protocol::ObserveOp::StateRead {
                    target: Locator::Css {
                        selector: "#after".into(),
                    },
                    field: frontcap_core::intervention::StateField::Text,
                },
                context: None,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        outcomes
            .push(serde_json::json!({"point":point,"kept_before":kept_before,"after":after.value}));
        s.stop().await;
    }
    assert_eq!(
        serde_json::json!(outcomes),
        serde_json::json!([
          {"point":"after_effect","kept_before":true,"after":"|"},
          {"point":"after_complete","kept_before":true,"after":"|"}
        ])
    );
}

/// A popup's stream opening interrupted at each admission point is taken back in its window by the
/// relaunched recorder: it opens once, and its selected message still arrives.
#[tokio::test]
async fn a_popup_stream_admission_recovers_in_its_window() {
    for point in ["before_stream_binding","after_stream_binding","before_open_delivery","after_open_delivery"] {
    use frontcap_core::{event::{EventKind,StreamEventKind},log::BehaviorLog};
    let rules=RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"http://*/stream-popup.html"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><output id='result'>0/0</output><script>setTimeout(()=>window.open('/stream-window.html','feed'),10)</script>"}},
        {"id":"window","match":{"url":"http://*/stream-window.html"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>let opens=0,messages=0;const show=()=>{opener.document.querySelector('#result').textContent=opens+'/'+messages};setTimeout(()=>{const s=new WebSocket('wss://api.example.test/feed');s.onopen=()=>{opens++;show()};s.onmessage=()=>{messages++;show()}},250)</script>"}},
        {"id":"feed","match":{"url":"wss://api.example.test/feed"},"action":{"type":"stream_mock","messages":[{"at_ms":150,"data":"later"}]}}
    ]).to_string()).unwrap();
    let mut s=Session::launch_popups_with("stream-popup.html",rules,&[("FRONTCAP_STREAM_FAULT".into(),point.into())]).await;
    let run=s.status().await.run_id;
    s.mutate("setup",Op::Advance {by_us:51_000}).await.unwrap();
    assert!(s.mutate("open",Op::Advance {by_us:250_000}).await.is_err(), "{point} did not fire");
    s.recover_and_reattach().await;
    s.mutate("later",Op::Advance {by_us:500_000}).await.unwrap();
    let shown=s.text_is("result","#result","1/1").await;
    let events=BehaviorLog::read(&s.ws.runs().join(run.as_str()).join("trace.jsonl")).unwrap();
    let streams:Vec<_>=events.iter().filter(|event|matches!(&event.kind,EventKind::Stream(_)|EventKind::Taint(_)|EventKind::Request(_))).map(|e| format!("{} {:?}", e.t_us.0, e.kind)).collect();
    assert!(shown,"{point}: open must not repeat and the selected future message must survive: {streams:#?}");
    let opens:Vec<_>=events.iter().filter(|event|matches!(&event.kind,EventKind::Stream(stream) if stream.event==StreamEventKind::Open)).collect();
    assert_eq!(opens.len(),1,"{point}: the opening is reconstructed exactly once");
    assert!(matches!(&opens[0].kind, EventKind::Stream(stream) if stream.context.as_deref()==Some("popup-1")), "{point}: {:?}", opens[0]);
    s.stop().await;
    }
}

fn held_stream_rules() -> RuleSet {
    RuleSet::from_json(r#"[{"id":"chat","match":{"url":"https://api.example.test/chat"},"action":{"type":"hold"}}]"#).unwrap()
}

/// The ordinal of the stream the page holds open for the agent.
async fn held_stream(s: &mut Session) -> String {
    s.status().await.held_requests.into_iter().find(|h| h.stream.is_some()).expect("a held stream").ordinal
}

fn stream_open() -> frontcap_core::rules::MockSpec {
    serde_json::from_str(r#"{"status":200}"#).unwrap()
}

fn stream_message(ordinal: &str, data: &str) -> Op {
    Op::StreamSend { ordinal: ordinal.into(), command: frontcap_recorder_protocol::StreamCommand::Message { data: data.into(), event: None, id: None } }
}

/// The agent's answer to a held stream's handshake, interrupted by recorder loss before or after it took
/// effect: the repeat opens the stream once, and messages follow.
#[tokio::test]
async fn a_held_streams_answer_interrupted_by_recorder_loss_opens_it_once() {
    for (point, applied) in [("after_dispatch", false), ("after_effect", true)] {
        let env = [("FRONTCAP_FAULT".to_string(), format!("o-1@{point}"))];
        let mut s = Session::launch("held-stream.html", held_stream_rules(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        let ordinal = held_stream(&mut s).await;
        let open = Op::Respond { ordinal: ordinal.clone(), response: stream_open() };
        assert!(s.mutate("o-1", open.clone()).await.is_err(), "the recorder dies at {point}");
        s.recover_and_reattach().await;
        let again = s.mutate("o-1", open.clone()).await.expect("a definite answer");
        if applied {
            assert!(matches!(again, Reply::Done { outcome: Outcome::Ack, .. }), "{point}: {again:?}");
        } else {
            assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{point}: {again:?}");
            s.mutate("o-2", open).await.unwrap();
        }
        s.mutate("m-1", stream_message(&ordinal, "hi")).await.unwrap();
        assert!(s.text_is("live", "#state", "live 1").await, "{point}: opened once");
        assert!(s.text_is("hi", "#items", "hi").await, "{point}");
        s.stop().await;
    }
}

/// A message sent on a held stream, interrupted by recorder loss before its delivery was decided, after
/// it was decided (the relaunch delivers it), and after its answer was saved: the page has it once,
/// and the stream goes on.
#[tokio::test]
async fn a_message_on_a_held_stream_interrupted_by_recorder_loss_arrives_once() {
    for (variable, point, applied) in [("FRONTCAP_FAULT", "s-1@after_dispatch", false), ("FRONTCAP_STREAM_FAULT", "after_stream_sent", true), ("FRONTCAP_FAULT", "s-1@after_effect", true)] {
        let env = [(variable.to_string(), point.to_string())];
        let mut s = Session::launch("held-stream.html", held_stream_rules(), &env).await;
        s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        let ordinal = held_stream(&mut s).await;
        s.mutate("o-1", Op::Respond { ordinal: ordinal.clone(), response: stream_open() }).await.unwrap();
        assert!(s.mutate("s-1", stream_message(&ordinal, "one")).await.is_err(), "the recorder dies at {point}");
        s.recover_and_reattach().await;
        let again = s.mutate("s-1", stream_message(&ordinal, "one")).await.expect("a definite answer");
        if applied {
            assert!(matches!(again, Reply::Done { outcome: Outcome::Ack, .. }), "{point}: {again:?}");
        } else {
            assert!(matches!(again, Reply::Done { outcome: Outcome::NotApplied, .. }), "{point}: {again:?}");
            s.mutate("s-1b", stream_message(&ordinal, "one")).await.unwrap();
        }
        s.mutate("s-2", stream_message(&ordinal, "two")).await.unwrap();
        assert!(s.text_is("items", "#items", "onetwo").await, "{point}: each message once");
        s.stop().await;
    }
}

/// Recorder loss at a worker's retirement boundary must not resurrect its wait
/// or replay its owner timer when the original advance is retried.
#[tokio::test]
async fn atomic_worker_retirement_survives_recorder_loss_at_the_clock_boundary() {
    for timeout in ["1000", "Infinity"] {
        for point in ["after_clock", "after_effect"] {
            let worker = format!("const words=new Int32Array(new SharedArrayBuffer(4));postMessage('started');fetch('https://worker-retire.example.test/pending').then(r=>r.text()).then(()=>postMessage('response'));Atomics.wait(words,0,0,{timeout});postMessage('returned');");
            let page = format!("<!doctype html><script>globalThis.samples=[];globalThis.errors=[];globalThis.retirements=[];globalThis.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=e=>samples.push(e.data);worker.onerror=e=>errors.push(e.message);setTimeout(()=>{{worker.terminate();retirements.push(performance.now())}},200);</script>", serde_json::json!(worker));
            let rules = RuleSet::from_json(&serde_json::json!([
                {"id":"page","match":{"url":"https://worker-retire.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":page}},
                {"id":"pending","match":{"url":"https://worker-retire.example.test/pending"},"action":{"type":"mock","status":200,"body":"late","latency_ms":500}}
            ]).to_string()).unwrap();
            let env = [("FRONTCAP_FAULT".to_string(), format!("retire@{point}"))];
            let mut s = Session::launch_url("https://worker-retire.example.test/", rules, &env).await;
            s.mutate("start-worker", Op::Advance { by_us: 50_000 }).await.unwrap();
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("started", Op::Evaluate { expression: "({samples,retirements,errors})".into() }).await.unwrap() else { panic!() };
            assert_eq!(value, serde_json::json!({"samples":["started"],"retirements":[],"errors":[]}));
            let before = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser;
            let advance = Op::Advance { by_us: 150_000 };
            assert!(s.mutate("retire", advance.clone()).await.is_err(), "fault must fire: {timeout}/{point}");
            s.recover_and_reattach().await;
            s.mutate("retire", advance).await.unwrap();
            assert_eq!(s.status().await.t_us.0, 200_000, "retry must not repeat time: {timeout}/{point}");
            assert_eq!(frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser, before);
            s.mutate("past-wait-and-response", Op::Advance { by_us: 1_800_000 }).await.unwrap();
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("finished", Op::Evaluate { expression: "({samples,retirements,errors})".into() }).await.unwrap() else { panic!() };
            assert_eq!(value, serde_json::json!({"samples":["started"],"retirements":[200],"errors":[]}), "{timeout}/{point}");
            let status = s.status().await;
            assert_eq!(status.t_us.0, 2_000_000);
            let trace = frontcap_core::log::BehaviorLog::read(&status.bundle.join("trace.jsonl")).unwrap();
            let pending: Vec<_> = trace.iter().filter_map(|event| match &event.kind {
                frontcap_core::event::EventKind::Request(request) if request.url.ends_with("/pending") => Some(request.id.clone()),
                _ => None,
            }).collect();
            assert_eq!(pending.len(), 1, "the retired worker issued its load exactly once");
            for event in &trace {
                if let frontcap_core::event::EventKind::Response(response) = &event.kind
                    && pending.contains(&response.id) {
                    assert_eq!(response.source, frontcap_core::event::ResponseSource::Fail, "{timeout}/{point}");
                }
            }
            s.stop().await;
        }
    }
}

/// A recovered advance must preserve the native atomic deadline and must not
/// replay the notification timer when the same request is retried.
#[tokio::test]
async fn atomic_notification_deadline_survives_recorder_replacement_and_retry() {
    for boundary in [99_000, 100_000] {
        for point in ["after_clock", "after_effect"] {
            let worker = "onmessage=e=>{const words=new Int32Array(e.data);Atomics.store(words,1,1);const start=Date.now();const result=Atomics.wait(words,0,0,100);postMessage({start,at:Date.now(),result});};postMessage('ready');";
            let page = format!("<!doctype html><script>globalThis.atomicEpoch=Date.now();globalThis.samples=[];globalThis.notifications=[];globalThis.errors=[];globalThis.started=null;globalThis.buffer=new SharedArrayBuffer(8);globalThis.words=new Int32Array(buffer);globalThis.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onerror=e=>errors.push(e.message);worker.onmessage=e=>{{if(e.data==='ready'){{started=Date.now()-atomicEpoch;worker.postMessage(buffer);setTimeout(()=>notifications.push({{at:Date.now()-atomicEpoch,count:Atomics.notify(words,0)}}),100);}}else samples.push({{start:e.data.start-atomicEpoch,at:e.data.at-atomicEpoch,result:e.data.result}});}};</script>", serde_json::json!(worker));
            let rules = RuleSet::from_json(&serde_json::json!([
                {"id":"page","match":{"url":"https://atomic-recovery.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":page}}
            ]).to_string()).unwrap();
            let env = [("FRONTCAP_FAULT".to_string(), format!("atomic-boundary@{point}"))];
            let mut s = Session::launch_url("https://atomic-recovery.example.test/", rules, &env).await;
            s.mutate("start-wait", Op::Advance { by_us: 50_000 }).await.unwrap();
            let expression = "({started,entered:Atomics.load(words,1),samples,notifications,errors})";
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("waiting", Op::Evaluate { expression: expression.into() }).await.unwrap() else { panic!() };
            assert_eq!(value, serde_json::json!({"started":0,"entered":1,"samples":[],"notifications":[],"errors":[]}));
            let before = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser;
            let advance = Op::Advance { by_us: boundary - 50_000 };
            assert!(s.mutate("atomic-boundary", advance.clone()).await.is_err(), "fault must fire: {boundary}/{point}");
            s.recover_and_reattach().await;
            s.mutate("atomic-boundary", advance.clone()).await.unwrap();
            assert_eq!(s.status().await.t_us.0, boundary, "retry must not add time: {boundary}/{point}");
            assert_eq!(frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser, before);
            s.mutate("past-deadline", Op::Advance { by_us: 200_000 - boundary }).await.unwrap();
            s.mutate("atomic-boundary", advance).await.unwrap();
            assert_eq!(s.status().await.t_us.0, 200_000, "a later retry must not replay or rewind progress");
            let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("completed", Op::Evaluate { expression: expression.into() }).await.unwrap() else { panic!() };
            assert_eq!(value, serde_json::json!({"started":0,"entered":1,"samples":[{"start":0,"at":100,"result":"timed-out"}],"notifications":[{"at":100,"count":0}],"errors":[]}), "{boundary}/{point}");
            s.stop().await;
        }
    }
}

/// Admission into a renderer blocked by its sibling must survive controller loss
/// without replaying the completed response or losing the child's future timer.
#[tokio::test]
async fn a_popup_in_a_synchronously_blocked_renderer_survives_recorder_replacement() {
    use frontcap_core::{action::Action, event::EventKind, profile::PopupPolicy};
    use serde_json::json;
    for point in ["after_clock", "after_effect"] {
        let root = "<!doctype html><button id=open onclick=\"window.open('https://auth.example.test/')\">open</button><script>globalThis.epoch=Date.now();globalThis.samples=[];</script>";
        let opener = "<!doctype html><script>setTimeout(()=>window.open('https://app.example.test/child'),800)</script>";
        let child = "<!doctype html><script>const samples=[{kind:'first',at:Date.now(),cookie:document.cookie}];const save=()=>localStorage.setItem('child-samples',JSON.stringify(samples));save();const x=new XMLHttpRequest();x.open('GET','/child-response',false);x.send();samples.push({kind:'response',at:Date.now(),cookie:document.cookie,body:x.responseText});save();setTimeout(()=>{samples.push({kind:'timer',at:Date.now()});save()},200);</script>";
        let headers = json!({"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"});
        let rules = RuleSet::from_json(&json!([
            {"id":"root","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":headers,"body":root}},
            {"id":"opener","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,"headers":headers,"body":opener}},
            {"id":"child","match":{"url":"https://app.example.test/child"},"action":{"type":"mock","status":200,"headers":headers,"body":child}},
            {"id":"root-response","match":{"url":"https://app.example.test/sync"},"action":{"type":"mock","status":200,"body":"root","latency_ms":1000}},
            {"id":"child-response","match":{"url":"https://app.example.test/child-response"},"action":{"type":"mock","status":200,"body":"child","latency_ms":100}}
        ]).to_string()).unwrap();
        let (_tmp, ws) = support::workspace();
        let mut request = support::launch_request("launch", "https://app.example.test/");
        request.rules = rules;
        request.profile.popups = PopupPolicy::Controlled;
        let env = [("FRONTCAP_FAULT".to_string(), format!("arrival@{point}"))];
        let launched = launcher::launch(&ws, request, &support::recorder_bin(), &env).await.unwrap();
        let mut s = Session {
            _guard: support::Runtime(ws.runtime_dir(&launched.launch_id)),
            _tmp, ws: ws.clone(), id: launched.launch_id, client: launched.client, lease: launched.lease,
        };
        s.mutate("setup", Op::Advance { by_us: 50_000 }).await.unwrap();
        s.mutate("schedule", Op::Evaluate { expression: "document.cookie='shared=live; Domain=example.test; Path=/; Max-Age=1';setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','/sync',false);x.send();samples.push({at:Date.now()-epoch,body:x.responseText,cookie:document.cookie})},500);null".into() }).await.unwrap();
        s.mutate("open", act(Action::Click { target: Locator::Css { selector: "#open".into() } })).await.unwrap();
        let browser = frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser;
        let advance = Op::Advance { by_us: 1_650_000 };
        assert!(s.mutate("arrival", advance.clone()).await.is_err(), "fault must fire: {point}");
        s.recover_and_reattach().await;
        s.mutate("arrival", advance.clone()).await.unwrap();
        assert_eq!(s.status().await.t_us.0, 1_700_000, "retry must retain the original endpoint: {point}");
        assert_eq!(frontcap_recorder::runtime::load_runtime(&s.ws.runtime_dir(&s.id)).unwrap().unwrap().browser, browser);
        let expression = "({root:samples,child:JSON.parse(localStorage.getItem('child-samples')).map(s=>({...s,at:s.at-epoch}))})";
        let expected = |timer: bool| {
            let mut child = vec![json!({"kind":"first","at":1550,"cookie":""}), json!({"kind":"response","at":1650,"cookie":"","body":"child"})];
            if timer { child.push(json!({"kind":"timer","at":1850})); }
            json!({"root":[{"at":1550,"body":"root","cookie":""}],"child":child})
        };
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("before-timer", Op::Evaluate { expression: expression.into() }).await.unwrap() else { panic!() };
        assert_eq!(value, expected(false), "{point}");
        s.mutate("finish", Op::Advance { by_us: 300_000 }).await.unwrap();
        s.mutate("arrival", advance).await.unwrap();
        assert_eq!(s.status().await.t_us.0, 2_000_000, "a late retry must not rewind or repeat progress");
        let Reply::Done { outcome: Outcome::Evaluated { value }, .. } = s.mutate("after-timer", Op::Evaluate { expression: expression.into() }).await.unwrap() else { panic!() };
        assert_eq!(value, expected(true), "{point}");
        let trace = frontcap_core::log::BehaviorLog::read(&s.status().await.bundle.join("trace.jsonl")).unwrap();
        for url in ["https://app.example.test/child", "https://app.example.test/sync", "https://app.example.test/child-response"] {
            assert_eq!(trace.iter().filter(|event| matches!(&event.kind, EventKind::Request(request) if request.url == url)).count(), 1, "request must not repeat: {point}/{url}");
        }
        s.stop().await;
    }
}
