//! A composed probe (`Op::Probe`) answers each step as the same steps sent as separate calls do, and its
//! behavior log is theirs; a recorder fault inside it completes it from its steps' answers.
mod support;

use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::intervention::{StateChange, StateField};
use frontcap_core::predicate::Locator;
use frontcap_recorder::launcher::{self, Attached};
use frontcap_recorder_protocol::{Answer, Call, ErrorCode, Observation, ObserveOp, Op, Outcome, ProbeStep, Reply};
use serde_json::{Value, json};

fn css(selector: &str) -> Locator {
    Locator::Css { selector: selector.into() }
}

fn steps() -> Vec<ProbeStep> {
    let query = |selector: &str, prop: &str| ProbeStep::Observe {
        op: ObserveOp::Query { target: css(selector), props: vec![prop.into()] },
        context: None,
    };
    vec![
        ProbeStep::Mutate { op: Op::Advance { by_us: 50_000 } },
        ProbeStep::Mutate { op: Op::Act { action: Action::Type { target: css("#q"), text: "ab".into() }, then: None, context: None } },
        query("#value", "text"),
        ProbeStep::Apply {
            progress_us: None,
            changes: vec![StateChange { target: css("#q"), field: StateField::Value, value: json!("set"), expected: None }],
            context: None,
        },
        query("#q", "value"),
        query("#value", "text"),
        ProbeStep::Mutate { op: Op::Advance { by_us: 100_000 } },
        query("#keys", "text"),
    ]
}

/// What a step answered, without the identities of the generation and request that answered it.
fn comparable(answer: &Answer) -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for key in ["request_id", "launch_id", "ref", "context_id", "wall_ms"] {
                    m.remove(key);
                }
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(answer).unwrap();
    strip(&mut v);
    v
}

async fn call(a: &mut Attached, call: Call) -> Answer {
    match a.client.call(call).await {
        Ok(reply) => Answer::Ok(reply),
        Err(frontcap_recorder_protocol::client::ClientError::Recorder(e)) => Answer::Err(e),
        Err(e) => panic!("{e}"),
    }
}

/// An input step as a separate `input` call sends it: on the held pointer, at the current boundary.
async fn input_op(a: &mut Attached, op: frontcap_recorder_protocol::ProbeInput) -> Op {
    let Answer::Ok(Reply::Observed { result: Observation::Input(state) }) = call(a, Call::Observe { op: ObserveOp::InputState, context: None }).await else {
        panic!("no boundary")
    };
    let action = op.action(state.held.map(|p| p.handle), state.boundary).expect("a held pointer");
    Op::Act { action, then: None, context: None }
}

/// The same steps as separate calls, an apply at the boundary observed just before it.
async fn separately(a: &mut Attached) -> Vec<Answer> {
    let mut answers = Vec::new();
    for (n, step) in steps().into_iter().enumerate() {
        let id = RequestId::new(format!("step-{n}")).unwrap();
        let answer = match step {
            ProbeStep::WaitFrom { .. } => unreachable!("separate-call fixture has no relative waits"),
            ProbeStep::Mutate { op } => {
                let lease = Some(a.lease.clone());
                call(a, Call::Mutate { lease, request_id: id, op }).await
            }
            ProbeStep::Apply { changes, context, progress_us } => {
                let Answer::Ok(Reply::Observed { result: Observation::Input(state) }) = call(a, Call::Observe { op: ObserveOp::InputState, context: None }).await else {
                    panic!("no boundary")
                };
                let lease = Some(a.lease.clone());
                call(a, Call::Mutate { lease, request_id: id, op: Op::StateApply { boundary: state.boundary, changes, context, progress_us } }).await
            }
            ProbeStep::Observe { op, context } => call(a, Call::Observe { op, context }).await,
            ProbeStep::Input { op } => {
                let op = input_op(a, op).await;
                let lease = Some(a.lease.clone());
                call(a, Call::Mutate { lease, request_id: id, op }).await
            }
        };
        answers.push(answer);
    }
    answers
}

#[tokio::test]
async fn a_probe_answers_as_its_steps_sent_separately() {
    let addr = support::serve();
    let url = format!("http://{addr}/type.html");
    let (_tmp, ws) = support::workspace();
    let mut probed = launcher::launch(&ws, support::launch_request("probed", &url), &support::recorder_bin(), &[]).await.unwrap();
    let mut separate = launcher::launch(&ws, support::launch_request("separate", &url), &support::recorder_bin(), &[]).await.unwrap();

    let probe = Op::Probe { steps: steps() };
    let lease = Some(probed.lease.clone());
    let answer = call(&mut probed, Call::Mutate { lease: lease.clone(), request_id: RequestId::new("probe").unwrap(), op: probe.clone() }).await;
    let Answer::Ok(Reply::Done { outcome: Outcome::Probed { results, completed, end, .. }, .. }) = &answer else { panic!("{answer:?}") };
    assert_eq!(*completed, steps().len(), "{results:?}");
    let expected = separately(&mut separate).await;
    for (n, (probe_step, separate_step)) in results.iter().zip(&expected).enumerate() {
        assert_eq!(comparable(probe_step), comparable(separate_step), "step {n}");
    }
    // It ends where the separate steps left their generation.
    let Answer::Ok(Reply::Observed { result: Observation::Input(state) }) = call(&mut separate, Call::Observe { op: ObserveOp::InputState, context: None }).await else {
        panic!("no boundary")
    };
    let end = end.as_ref().expect("an end boundary");
    assert_eq!((end.t_us, end.state_revision), (state.boundary.t_us, state.boundary.state_revision));

    // A retried probe answers from its journal; other arguments under its id conflict.
    let again = call(&mut probed, Call::Mutate { lease: lease.clone(), request_id: RequestId::new("probe").unwrap(), op: probe }).await;
    assert_eq!(again, answer);
    let other = call(&mut probed, Call::Mutate { lease: lease.clone(), request_id: RequestId::new("probe").unwrap(), op: Op::Probe { steps: steps()[..1].to_vec() } }).await;
    assert!(matches!(&other, Answer::Err(e) if e.code == ErrorCode::RequestIdConflict), "{other:?}");
    // Bounds are refused before anything runs.
    let none = call(&mut probed, Call::Mutate { lease: lease.clone(), request_id: RequestId::new("empty").unwrap(), op: Op::Probe { steps: vec![] } }).await;
    assert!(matches!(&none, Answer::Err(e) if e.code == ErrorCode::InvalidArguments), "{none:?}");

    // The two runs behave alike, step for step.
    let mut traces = Vec::new();
    for a in [&mut probed, &mut separate] {
        let lease = Some(a.lease.clone());
        let Answer::Ok(Reply::Done { .. }) = call(a, Call::Mutate { lease, request_id: RequestId::new("stop").unwrap(), op: Op::Stop }).await else { panic!("not stopped") };
        traces.push(std::fs::read_to_string(ws.runs().join(launcher::run_id_of(&a.launch_id).as_str()).join("trace.jsonl")).unwrap());
    }
    assert_eq!(frontcap_core::projection::compare_behavior(&traces[0], &traces[1]), Ok(()));
}

/// A probe whose recorder dies at `fault` (`<request id>@<point>`), sent again after the recorder is
/// relaunched: its answer as recovery completed it, twice.
#[cfg(feature = "fault-injection")]
async fn interrupted_at(fault: &str) -> (Vec<Answer>, usize) {
    let addr = support::serve();
    let url = format!("http://{addr}/type.html");
    let (_tmp, ws) = support::workspace();
    let env = [("FRONTCAP_FAULT".to_string(), fault.to_string())];
    let mut a = launcher::launch(&ws, support::launch_request("probed", &url), &support::recorder_bin(), &env).await.unwrap();
    let _guard = support::Runtime(ws.runtime_dir(&a.launch_id));
    let lease = Some(a.lease.clone());
    let died = a.client.call(Call::Mutate { lease, request_id: RequestId::new("probe").unwrap(), op: Op::Probe { steps: steps() } }).await;
    assert!(died.is_err(), "the recorder must die at {fault}: {died:?}");
    launcher::recover(&ws, &a.launch_id, &support::recorder_bin(), &[]).await.unwrap();
    let mut a = launcher::reattach(&ws, &a.launch_id, RequestId::new("reattach").unwrap()).await.unwrap();
    let probe = Call::Mutate { lease: Some(a.lease.clone()), request_id: RequestId::new("probe").unwrap(), op: Op::Probe { steps: steps() } };
    let answer = call(&mut a, probe.clone()).await;
    assert_eq!(call(&mut a, probe).await, answer, "{fault}: answered from the journal");
    let Answer::Ok(Reply::Done { outcome: Outcome::Probed { results, completed, end, cancelled: false, interrupted: true }, .. }) = answer else {
        panic!("{fault}: {answer:?}")
    };
    // It stops where its last answered step left the generation, which goes on taking requests.
    let Answer::Ok(Reply::Observed { result: Observation::Input(state) }) = call(&mut a, Call::Observe { op: ObserveOp::InputState, context: None }).await else {
        panic!("no boundary")
    };
    assert_eq!(end.map(|b| b.t_us), Some(state.boundary.t_us), "{fault}");
    let lease = Some(a.lease.clone());
    assert!(matches!(call(&mut a, Call::Mutate { lease, request_id: RequestId::new("after").unwrap(), op: Op::Advance { by_us: 10_000 } }).await, Answer::Ok(Reply::Done { .. })));
    (results, completed)
}

/// The probe's steps as single calls under the probe's step ids, in a generation whose recorder dies at
/// `fault` if one names a step: that step is sent again after the recorder is relaunched, as a single
/// call is recovered, and nothing after it is sent.
#[cfg(feature = "fault-injection")]
async fn single_calls(fault: Option<&str>) -> Vec<Answer> {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let env: Vec<_> = fault.map(|f| ("FRONTCAP_FAULT".to_string(), f.to_string())).into_iter().collect();
    let mut a = launcher::launch(&ws, support::launch_request("single", &format!("http://{addr}/type.html")), &support::recorder_bin(), &env).await.unwrap();
    let _guard = support::Runtime(ws.runtime_dir(&a.launch_id));
    let probe = RequestId::new("probe").unwrap();
    let mut answers = Vec::new();
    for (n, step) in steps().into_iter().enumerate() {
        let id = probe.derived(&format!("probe-{n}"));
        let sent = match step {
            ProbeStep::WaitFrom { .. } => unreachable!("separate-call fixture has no relative waits"),
            ProbeStep::Mutate { op } => Call::Mutate { lease: Some(a.lease.clone()), request_id: id, op },
            ProbeStep::Apply { changes, context, progress_us } => {
                let Answer::Ok(Reply::Observed { result: Observation::Input(state) }) = call(&mut a, Call::Observe { op: ObserveOp::InputState, context: None }).await else {
                    panic!("no boundary")
                };
                Call::Mutate { lease: Some(a.lease.clone()), request_id: id, op: Op::StateApply { boundary: state.boundary, changes, context, progress_us } }
            }
            ProbeStep::Observe { op, context } => Call::Observe { op, context },
            ProbeStep::Input { op } => Call::Mutate { lease: Some(a.lease.clone()), request_id: id, op: input_op(&mut a, op).await },
        };
        match a.client.call(sent.clone()).await {
            Ok(reply) => answers.push(Answer::Ok(reply)),
            Err(frontcap_recorder_protocol::client::ClientError::Recorder(e)) => answers.push(Answer::Err(e)),
            Err(_) => {
                launcher::recover(&ws, &a.launch_id, &support::recorder_bin(), &[]).await.unwrap();
                a = launcher::reattach(&ws, &a.launch_id, RequestId::new("reattach").unwrap()).await.unwrap();
                let Call::Mutate { request_id, op, .. } = sent else { panic!("a read died") };
                let lease = Some(a.lease.clone());
                answers.push(call(&mut a, Call::Mutate { lease, request_id, op }).await);
                break;
            }
        }
    }
    answers
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_probe_recovers_a_provider_writes_progress_grant() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let env = [("FRONTCAP_FAULT".to_string(), "mcp-probe-1-provider@state_after_write_0".to_string())];
    let mut a = launcher::launch(&ws, support::launch_request("provider", &format!("http://{addr}/state-async.html")), &support::recorder_bin(), &env).await.unwrap();
    let _guard = support::Runtime(ws.runtime_dir(&a.launch_id));
    let op = Op::Probe { steps: vec![
        ProbeStep::Mutate { op: Op::Advance { by_us: 50_000 } },
        ProbeStep::Apply {
            changes: vec![StateChange { target: Locator::Document { selector: None }, field: StateField::App { provider: "setting".into() }, value: json!({"level":3}), expected: None }],
            context: None, progress_us: Some(500_000),
        },
        ProbeStep::Mutate { op: Op::Advance { by_us: 1_000_000 } },
    ] };
    let id = RequestId::new("provider").unwrap();
    assert!(a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: id.clone(), op: op.clone() }).await.is_err());
    launcher::recover(&ws, &a.launch_id, &support::recorder_bin(), &[]).await.unwrap();
    let mut a = launcher::reattach(&ws, &a.launch_id, RequestId::new("reattach").unwrap()).await.unwrap();
    let request = Call::Mutate { lease: Some(a.lease.clone()), request_id: id, op };
    let answer = call(&mut a, request.clone()).await;
    assert_eq!(call(&mut a, request).await, answer);
    let Answer::Ok(Reply::Done { outcome: Outcome::Probed { results, completed, end, interrupted, .. }, .. }) = answer else { panic!("{answer:?}") };
    assert!(interrupted);
    assert_eq!(completed,2);
    assert!((150_000..=167_000).contains(&end.unwrap().t_us.0));
    let Answer::Ok(Reply::Done { outcome: Outcome::StateApplied(applied), .. }) = &results[1] else { panic!("{results:?}") };
    assert_eq!(applied.receipts[0].after,Some(json!({"level":3})));
    let Answer::Ok(Reply::Observed { result: Observation::Query(q) }) = call(&mut a,Call::Observe { op: ObserveOp::Query { target:css("#writes"),props:vec!["text".into()] },context:None }).await else { panic!("no writes readback") };
    assert_eq!(q.values["text"],"1","a resumed provider write runs once");
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_relative_wait_recovers_its_original_goal() {
    for point in ["after_dispatch", "after_clock"] {
        let addr = support::serve();
        let (_tmp, ws) = support::workspace();
        let env = [("FRONTCAP_FAULT".to_string(), format!("mcp-probe-2-relative@{point}"))];
        let mut a = launcher::launch(&ws, support::launch_request("relative", &format!("http://{addr}/type.html")), &support::recorder_bin(), &env).await.unwrap();
        let _guard = support::Runtime(ws.runtime_dir(&a.launch_id));
        let op = Op::Probe { steps: vec![
            ProbeStep::Mutate { op: Op::Advance { by_us: 50_000 } },
            ProbeStep::Mutate { op: Op::Advance { by_us: 25_000 } },
            ProbeStep::WaitFrom { step_index: 0, after_us: 200_000 },
            ProbeStep::Mutate { op: Op::Advance { by_us: 1_000_000 } },
        ] };
        let request_id = RequestId::new("relative").unwrap();
        let died = a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: request_id.clone(), op: op.clone() }).await;
        assert!(died.is_err(), "{point}: {died:?}");
        launcher::recover(&ws, &a.launch_id, &support::recorder_bin(), &[]).await.unwrap();
        let mut a = launcher::reattach(&ws, &a.launch_id, RequestId::new("reattach").unwrap()).await.unwrap();
        let request = Call::Mutate { lease: Some(a.lease.clone()), request_id, op };
        let answer = call(&mut a, request.clone()).await;
        assert_eq!(call(&mut a, request).await, answer);
        let Answer::Ok(Reply::Done { outcome: Outcome::Probed { completed, end, interrupted, .. }, .. }) = answer else { panic!("{answer:?}") };
        assert!(interrupted);
        assert_eq!(completed, 3);
        assert_eq!(end.unwrap().t_us.0, 250_000, "the wait retains the earlier step's goal, and the unstarted suffix stays unstarted");
    }
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_probe_a_recorder_fault_interrupts_completes_from_its_steps_answers() {
    for (fault, answered, completed) in [
        // Before the type step's dispatch: it never reached the page.
        ("mcp-probe-1-probe@before_dispatch", 2, 1),
        // After a read was kept: the reads so far answer as they did.
        ("probe@after_step_2", 3, 3),
        // The second advance after its clock ran: it finishes as a single advance does.
        ("mcp-probe-6-probe@after_clock", 7, 7),
        // After the last step, before the probe completed.
        ("probe@after_step_7", 8, 8),
    ] {
        let (results, n) = interrupted_at(fault).await;
        assert_eq!((results.len(), n), (answered, completed), "{fault}: {results:?}");
        // Each step answers as the same single call does, an interrupted one as it is recovered.
        let expected = single_calls(fault.starts_with("mcp-probe-").then_some(fault)).await;
        for (step, (got, want)) in results.iter().zip(&expected).enumerate() {
            assert_eq!(comparable(got), comparable(want), "{fault}: step {step}");
        }
    }
}
