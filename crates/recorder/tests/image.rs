//! A process image through the recorder (`Call::Image`) and a generation restored from it
//! (`LaunchRequest::restore`): the child continues the source's page closure, pending timer, worker heap
//! and held request under its own launch, while the source goes on.
mod support;

use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_recorder::launcher::{self, Attached};
use frontcap_recorder_protocol::{Call, Observation, ObserveOp, Op, Reply};
use serde_json::Value;

fn rules() -> RuleSet {
    RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*checkpoint.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("../../adapter-chromium/tests/fixtures/checkpoint.html")}},
        {"id":"worker","match":{"url":"*checkpoint-worker.js"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/javascript"},"body":include_str!("../../adapter-chromium/tests/fixtures/checkpoint-worker.js")}},
        {"id":"held","match":{"url":"*checkpoint-held"},"action":{"type":"hold"}}
    ]).to_string()).unwrap()
}

async fn mutate(a: &mut Attached, id: &str, op: Op) -> Reply {
    a.client.call(Call::Mutate { lease: Some(a.lease.clone()), request_id: RequestId::new(id).unwrap(), op }).await.unwrap()
}

async fn click(a: &mut Attached, id: &str, selector: &str) {
    let op = Op::Act { action: Action::Click { target: Locator::Css { selector: selector.into() } }, then: None, context: None };
    assert!(matches!(mutate(a, id, op).await, Reply::Done { .. }));
}

async fn text(a: &mut Attached, selector: &str) -> Value {
    let Reply::Observed { result: Observation::Query(q) } = a.client.call(Call::Observe {
        op: ObserveOp::Query { target: Locator::Css { selector: selector.into() }, props: vec!["text".into()] }, context: None,
    }).await.unwrap() else { panic!("not a query") };
    q.values["text"].clone()
}

async fn current_boundary(a: &mut Attached) -> frontcap_core::observation::ObservationBoundary {
    let Reply::Observed { result: Observation::Input(state) } = a.client.call(Call::Observe { op: ObserveOp::InputState, context: None }).await.unwrap() else {
        panic!("no input state")
    };
    state.boundary
}

/// `extra` more hidden steps, the held request answered, 400 ms: what the page then shows.
async fn continuation(a: &mut Attached, extra: usize) -> Vec<Value> {
    for n in 0..extra {
        click(a, &format!("extra-{n}"), "#step").await;
    }
    let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!("no status") };
    assert_eq!(status.held.len(), 1, "the held request crossed the image");
    let answer = MockSpec { status: 200, headers: Default::default(), body: "answer".into(), body_base64: None, latency_ms: 0 };
    assert!(matches!(mutate(a, "answer", Op::Respond { ordinal: status.held[0].clone(), response: answer }).await, Reply::Done { .. }));
    assert!(matches!(mutate(a, "later", Op::Advance { by_us: 400_000 }).await, Reply::Done { .. }));
    let mut shown = Vec::new();
    for selector in ["#timer", "#worker", "#response"] {
        shown.push(text(a, selector).await);
    }
    shown
}

#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn a_generation_restored_from_an_image_continues_its_execution() {
    let (_tmp, ws) = support::workspace();
    let env = vec![("FRONTCAP_CELLS".to_string(), "65534:65534".to_string())];
    let url = "https://app.example.test/checkpoint.html";
    let request = |id: &str| launcher::LaunchRequest { rules: rules(), passthrough_origins: Some(vec![]), ..support::launch_request(id, url) };
    let mut source = launcher::launch(&ws, request("source"), &support::recorder_bin(), &env).await.unwrap();
    assert!(matches!(mutate(&mut source, "load", Op::Advance { by_us: 51_000 }).await, Reply::Done { .. }));
    assert_eq!(text(&mut source, "#ready").await, "ready");
    click(&mut source, "step-0", "#step").await;
    click(&mut source, "step-1", "#step").await;
    click(&mut source, "arm", "#arm").await;
    assert!(matches!(mutate(&mut source, "armed", Op::Advance { by_us: 50_000 }).await, Reply::Done { .. }));

    // An image's name names nothing outside the workspace's images.
    let escape = source.client.call(Call::Image { name: "../escape".into() }).await;
    assert!(matches!(&escape, Err(frontcap_recorder_protocol::client::ClientError::Recorder(e)) if e.code == frontcap_recorder_protocol::ErrorCode::InvalidArguments), "{escape:?}");
    let Reply::Status { status: before } = source.client.call(Call::Status).await.unwrap() else { panic!("no status") };
    let observed = current_boundary(&mut source).await;
    let Reply::Imaged { boundary } = source.client.call(Call::Image { name: "image".into() }).await.unwrap() else { panic!("not imaged") };
    // The source is unchanged: its boundary stays current and no request completed.
    let Reply::Status { status: after } = source.client.call(Call::Status).await.unwrap() else { panic!("no status") };
    assert_eq!(after.last_completed_request_id, before.last_completed_request_id);
    assert_eq!(boundary, observed);
    assert_eq!(current_boundary(&mut source).await, observed, "the image changed nothing an observation names");
    let child_request = launcher::LaunchRequest { restore: Some("image".into()), ..request("child") };
    let mut child = launcher::launch(&ws, child_request, &support::recorder_bin(), &env).await.unwrap();
    assert_ne!(child.launch_id, source.launch_id);
    let Reply::Status { status } = child.client.call(Call::Status).await.unwrap() else { panic!("no status") };
    assert_eq!(status.t_us, boundary.t_us, "the child starts at the image's boundary");

    // The child takes one more hidden step; the source takes none.
    let restored = continuation(&mut child, 1).await;
    let original = continuation(&mut source, 0).await;
    assert_eq!(original, [Value::from("2"), Value::from("2"), Value::from("answer:2")]);
    assert_eq!(restored, [Value::from("3"), Value::from("3"), Value::from("answer:3")]);
    for a in [&mut child, &mut source] {
        assert!(matches!(mutate(a, "stop", Op::Stop).await, Reply::Done { .. }));
    }
}

/// The answer of a suspended operation once its page goes on and the request no longer runs.
async fn finished(a: &mut Attached, id: &str) -> frontcap_recorder_protocol::Answer {
    let call = Call::Result { request_id: RequestId::new(id).unwrap() };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Reply::Result { result: frontcap_recorder_protocol::RequestResult::Completed { answer }, .. } = a.client.call(call.clone()).await.unwrap()
                && matches!(a.client.call(Call::Status).await.unwrap(), Reply::Status { status } if status.running.is_none())
            {
                break *answer;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("the operation ends once answered")
}

/// After an image, the source's recorder still sees the page's held dialogs and the held requests its
/// main thread waits for: each suspends the operation that met it, and its answer lets the operation end.
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn the_source_suspends_on_dialogs_and_held_requests_after_an_image() {
    use frontcap_recorder_protocol::{Answer, DialogResponse};
    let page = r#"<!doctype html><button id=ask>Ask</button><button id=load>Load</button><output id=out></output><script>
        ask.onclick = () => { out.textContent = confirm('Continue?') ? 'yes' : 'no'; };
        load.onclick = () => { const x = new XMLHttpRequest(); x.open('GET', '/sync', false); x.send(); out.textContent = x.responseText; };
    </script>"#;
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*after-image.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"sync","match":{"url":"*/sync"},"action":{"type":"hold"}}
    ]).to_string()).unwrap();
    let (_tmp, ws) = support::workspace();
    let env = vec![("FRONTCAP_CELLS".to_string(), "65534:65534".to_string())];
    let request = launcher::LaunchRequest { rules, passthrough_origins: Some(vec![]), ..support::launch_request("source", "https://app.example.test/after-image.html") };
    let mut a = launcher::launch(&ws, request, &support::recorder_bin(), &env).await.unwrap();
    assert!(matches!(mutate(&mut a, "load", Op::Advance { by_us: 20_000 }).await, Reply::Done { .. }));
    assert!(matches!(mutate(&mut a, "policy", Op::DialogRules { rules: serde_json::from_str(r#"[{"answer":"hold"}]"#).unwrap() }).await, Reply::Done { .. }));
    let Reply::Imaged { .. } = a.client.call(Call::Image { name: "image".into() }).await.unwrap() else { panic!("not imaged") };

    let ask = Op::Act { action: Action::Click { target: Locator::Css { selector: "#ask".into() } }, then: None, context: None };
    let asked = tokio::time::timeout(std::time::Duration::from_secs(15), mutate(&mut a, "ask", ask)).await.expect("the click returns");
    let Reply::Suspended { operation } = asked else { panic!("a held dialog suspends the click: {asked:?}") };
    let dialog = operation.dialog.expect("the dialog it waits for");
    a.client.call(Call::AnswerDialog { lease: a.lease.clone(), request_id: RequestId::new("yes").unwrap(), response: DialogResponse {
        parent_request_id: RequestId::new("ask").unwrap(), dialog_id: dialog.id, accept: true, prompt_text: None,
    }}).await.unwrap();
    assert!(matches!(finished(&mut a, "ask").await, Answer::Ok(Reply::Done { .. })));
    assert_eq!(text(&mut a, "#out").await, "yes");

    let load = Op::Act { action: Action::Click { target: Locator::Css { selector: "#load".into() } }, then: None, context: None };
    let loaded = tokio::time::timeout(std::time::Duration::from_secs(15), mutate(&mut a, "sync", load)).await.expect("the click returns");
    let Reply::Suspended { operation } = loaded else { panic!("a held synchronous request suspends the click: {loaded:?}") };
    let held = operation.request.expect("the request it waits for");
    let answer = MockSpec { status: 200, headers: Default::default(), body: "loaded".into(), body_base64: None, latency_ms: 0 };
    assert!(matches!(mutate(&mut a, "respond", Op::Respond { ordinal: held.ordinal.clone(), response: answer }).await, Reply::Done { .. }));
    assert!(matches!(finished(&mut a, "sync").await, Answer::Ok(Reply::Done { .. })));
    assert_eq!(text(&mut a, "#out").await, "loaded");
    assert!(matches!(mutate(&mut a, "stop", Op::Stop).await, Reply::Done { .. }));
}

/// The source as the main oracle leaves it before its image: two hidden steps, the continuation armed.
#[cfg(feature = "fault-injection")]
async fn armed(ws: &launcher::Workspace, env: &[(String, String)]) -> Attached {
    armed_as(ws, env, "source").await
}

async fn armed_as(ws: &launcher::Workspace, env: &[(String, String)], id: &str) -> Attached {
    let url = "https://app.example.test/checkpoint.html";
    let request = launcher::LaunchRequest { rules: rules(), passthrough_origins: Some(vec![]), ..support::launch_request(id, url) };
    let mut source = launcher::launch(ws, request, &support::recorder_bin(), env).await.unwrap();
    assert!(matches!(mutate(&mut source, "load", Op::Advance { by_us: 51_000 }).await, Reply::Done { .. }));
    click(&mut source, "step-0", "#step").await;
    click(&mut source, "step-1", "#step").await;
    click(&mut source, "arm", "#arm").await;
    assert!(matches!(mutate(&mut source, "armed", Op::Advance { by_us: 50_000 }).await, Reply::Done { .. }));
    source
}

/// A recorder that dies while it images its generation, before or after the image and before or after it
/// takes the browser again: the relaunched recorder takes the browser at the same boundary, the generation
/// goes on as it would have, and a later image of it restores a child as usual.
#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn a_recorder_fault_during_an_image_leaves_the_generation_going_on() {
    let cells = ("FRONTCAP_CELLS".to_string(), "65534:65534".to_string());
    for point in ["after_let_go", "after_keep", "after_adopt"] {
        let (_tmp, ws) = support::workspace();
        let env = [cells.clone(), ("FRONTCAP_FAULT".to_string(), format!("image@{point}"))];
        let mut source = armed(&ws, &env).await;
        let id = source.launch_id.clone();
        let _guard = support::Runtime(ws.runtime_dir(&id));
        let before = current_boundary(&mut source).await;
        let died = source.client.call(Call::Image { name: "image".into() }).await;
        assert!(died.is_err(), "the recorder must die at {point}: {died:?}");
        launcher::recover(&ws, &id, &support::recorder_bin(), std::slice::from_ref(&cells)).await.unwrap();
        let mut source = launcher::reattach(&ws, &id, RequestId::new("reattach").unwrap()).await.unwrap();
        let Reply::Status { status } = source.client.call(Call::Status).await.unwrap() else { panic!("no status") };
        assert_eq!(status.t_us, before.t_us, "{point}: the generation stays at its boundary");

        let Reply::Imaged { boundary } = source.client.call(Call::Image { name: "again".into() }).await.unwrap() else { panic!("{point}: not imaged") };
        assert_eq!(boundary.t_us, before.t_us);
        let child_request = launcher::LaunchRequest { restore: Some("again".into()), rules: rules(), passthrough_origins: Some(vec![]),
            ..support::launch_request("child", "https://app.example.test/checkpoint.html") };
        let mut child = launcher::launch(&ws, child_request, &support::recorder_bin(), std::slice::from_ref(&cells)).await.unwrap();
        let restored = continuation(&mut child, 1).await;
        let original = continuation(&mut source, 0).await;
        assert_eq!(original, [Value::from("2"), Value::from("2"), Value::from("answer:2")], "{point}");
        assert_eq!(restored, [Value::from("3"), Value::from("3"), Value::from("answer:3")], "{point}");
        for a in [&mut child, &mut source] {
            assert!(matches!(mutate(a, "stop", Op::Stop).await, Reply::Done { .. }));
        }
    }
}

/// A child recorder that dies while it restores an image, before the restore, after it or after it took
/// the restored browser: no child is published, no browser of it is left running, a retry of the same
/// launch answers the same failure, and the image restores a new child as usual. The source goes on.
#[cfg(feature = "fault-injection")]
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn a_recorder_fault_during_a_restore_publishes_no_child() {
    let cells = ("FRONTCAP_CELLS".to_string(), "65534:65534".to_string());
    for point in ["before_restore", "after_restore", "after_adopt"] {
        let (_tmp, ws) = support::workspace();
        let mut source = armed(&ws, std::slice::from_ref(&cells)).await;
        let _source_guard = support::Runtime(ws.runtime_dir(&source.launch_id));
        let Reply::Imaged { .. } = source.client.call(Call::Image { name: "image".into() }).await.unwrap() else { panic!("not imaged") };
        let child = |id: &str| launcher::LaunchRequest { restore: Some("image".into()), rules: rules(), passthrough_origins: Some(vec![]),
            ..support::launch_request(id, "https://app.example.test/checkpoint.html") };
        let faulty = [cells.clone(), ("FRONTCAP_FAULT".to_string(), format!("child@{point}"))];
        let failed = launcher::launch(&ws, child("child"), &support::recorder_bin(), &faulty).await;
        assert!(failed.is_err(), "{point}: the child must not be published");

        let id = launcher::registered_launch(&ws, &RequestId::new("child").unwrap()).expect("the child was registered");
        let record = frontcap_recorder::runtime::load_runtime(&ws.runtime_dir(&id)).unwrap().unwrap();
        assert!(matches!(record.state, frontcap_recorder::runtime::RuntimeState::Failed { .. }), "{point}: {:?}", record.state);
        assert!(!record.browser.as_ref().is_some_and(|b| b.is_alive()), "{point}: no browser of the child is left running");
        let again = launcher::launch(&ws, child("child"), &support::recorder_bin(), std::slice::from_ref(&cells)).await;
        assert!(again.is_err(), "{point}: a retry answers the same failure");
        assert_eq!(launcher::registered_launch(&ws, &RequestId::new("child").unwrap()), Some(id), "{point}: and starts no child");

        let mut fresh = launcher::launch(&ws, child("fresh"), &support::recorder_bin(), std::slice::from_ref(&cells)).await.unwrap();
        let restored = continuation(&mut fresh, 1).await;
        let original = continuation(&mut source, 0).await;
        assert_eq!(original, [Value::from("2"), Value::from("2"), Value::from("answer:2")], "{point}");
        assert_eq!(restored, [Value::from("3"), Value::from("3"), Value::from("answer:3")], "{point}");
        for a in [&mut fresh, &mut source] {
            assert!(matches!(mutate(a, "stop", Op::Stop).await, Reply::Done { .. }));
        }
    }
}

/// A page that logs the pointer and key events it receives, and an editable field.
const INPUT_PAGE: &str = r#"<!doctype html><div id=pad style="width:300px;height:120px;background:#ddd"></div><input id=field><output id=log></output><script>
  const log = [];
  const note = (s) => { log.push(s); document.querySelector('#log').textContent = log.join(' '); };
  // Pressing the pad keeps the field's focus and its composition.
  pad.addEventListener('mousedown', (e) => e.preventDefault());
  pad.addEventListener('pointerdown', (e) => note(`down:${e.buttons}`));
  pad.addEventListener('pointermove', (e) => note(`move:${e.clientX}:${e.buttons}`));
  pad.addEventListener('pointerup', (e) => note(`up:${e.buttons}`));
  field.addEventListener('keydown', (e) => note(`kd:${e.key}:${e.shiftKey}`));
  field.addEventListener('keyup', (e) => note(`ku:${e.key}`));
</script>"#;

async fn act(a: &mut Attached, id: &str, action: Action) -> Reply {
    mutate(a, id, Op::Act { action, then: None, context: None }).await
}

/// What a generation holding the mouse button, Shift and a composition does next: moves with the button
/// held and releases it, commits the composition, types a letter with Shift held and releases Shift.
/// The log of what the page received, and the field's value.
async fn finish_input(a: &mut Attached, x: i32, letter: &str) -> (Value, Value) {
    let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!("no status") };
    let pointer = status.pointer.expect("the held pointer crossed");
    assert_eq!(status.keys, ["Shift"], "the held key crossed");
    let boundary = current_boundary(a).await;
    assert!(matches!(act(a, "move", Action::PointerMove { handle: pointer.handle.clone(), point: [x, 40], boundary }).await, Reply::Done { .. }));
    assert!(matches!(act(a, "end", Action::PointerEnd { handle: pointer.handle }).await, Reply::Done { .. }));
    assert!(matches!(act(a, "commit", Action::Commit { key: None }).await, Reply::Done { .. }));
    assert!(matches!(act(a, "letter", Action::Press { target: None, key: letter.into() }).await, Reply::Done { .. }));
    assert!(matches!(act(a, "shift-up", Action::KeyUp { key: "Shift".into() }).await, Reply::Done { .. }));
    let field = {
        let Reply::Observed { result: Observation::Query(q) } = a.client.call(Call::Observe {
            op: ObserveOp::Query { target: Locator::Css { selector: "#field".into() }, props: vec!["value".into()] }, context: None,
        }).await.unwrap() else { panic!("not a query") };
        q.values["value"].clone()
    };
    (text(a, "#log").await, field)
}

/// Held input crosses an image: the source and a child restored from it each hold the mouse button,
/// Shift and the field's composition, and go on with them on their own.
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn held_input_continues_in_the_source_and_a_child() {
    let (_tmp, ws) = support::workspace();
    let env = vec![("FRONTCAP_CELLS".to_string(), "65534:65534".to_string())];
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*held.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":INPUT_PAGE}}
    ]).to_string()).unwrap();
    let request = |id: &str, restore: Option<String>| launcher::LaunchRequest { rules: rules.clone(), passthrough_origins: Some(vec![]), restore,
        ..support::launch_request(id, "https://app.example.test/held.html") };
    let mut source = launcher::launch(&ws, request("source", None), &support::recorder_bin(), &env).await.unwrap();
    assert!(matches!(mutate(&mut source, "load", Op::Advance { by_us: 20_000 }).await, Reply::Done { .. }));
    click(&mut source, "focus", "#field").await;
    assert!(matches!(act(&mut source, "shift", Action::KeyDown { key: "Shift".into() }).await, Reply::Done { .. }));
    assert!(matches!(act(&mut source, "compose", Action::Compose { text: "ka".into() }).await, Reply::Done { .. }));
    let boundary = current_boundary(&mut source).await;
    let begin = Action::PointerBegin { target: Locator::Css { selector: "#pad".into() }, point: [40, 40], button: Default::default(), pointer: Default::default(), boundary };
    assert!(matches!(act(&mut source, "press", begin).await, Reply::Done { .. }));
    let Reply::Imaged { .. } = source.client.call(Call::Image { name: "held".into() }).await.unwrap() else { panic!("not imaged") };

    let mut child = launcher::launch(&ws, request("child", Some("held".into())), &support::recorder_bin(), &env).await.unwrap();
    let (child_log, child_field) = finish_input(&mut child, 200, "B").await;
    let (source_log, source_field) = finish_input(&mut source, 120, "C").await;
    assert_eq!(source_log, "kd:Shift:true down:1 move:120:1 up:0 kd:C:true ku:C ku:Shift", "the source goes on");
    assert_eq!(child_log, "kd:Shift:true down:1 move:200:1 up:0 kd:B:true ku:B ku:Shift", "and the child on its own");
    assert_eq!((source_field, child_field), (Value::from("kaC"), Value::from("kaB")), "each committed the composition it held");
    for a in [&mut child, &mut source] {
        assert!(matches!(mutate(a, "stop", Op::Stop).await, Reply::Done { .. }));
    }
}

/// A held touch contact crosses an image too: the source and a child each move it and lift it.
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn a_held_touch_continues_in_the_source_and_a_child() {
    let (_tmp, ws) = support::workspace();
    let env = vec![("FRONTCAP_CELLS".to_string(), "65534:65534".to_string())];
    let page = r#"<!doctype html><div id=pad style="width:300px;height:200px;background:#ddd;touch-action:none"></div><output id=log></output><script>
      const log = []; const note = (s) => { log.push(s); document.querySelector('#log').textContent = log.join(' '); };
      for (const t of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel']) pad.addEventListener(t, (e) => note(`${t}:${e.pointerType}:${Math.round(e.clientX)}`));
    </script>"#;
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*touch.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}
    ]).to_string()).unwrap();
    let request = |id: &str, restore: Option<String>| {
        let mut r = launcher::LaunchRequest { rules: rules.clone(), passthrough_origins: Some(vec![]), restore,
            ..support::launch_request(id, "https://app.example.test/touch.html") };
        r.profile.touch = true;
        r
    };
    let mut source = launcher::launch(&ws, request("source", None), &support::recorder_bin(), &env).await.unwrap();
    assert!(matches!(mutate(&mut source, "load", Op::Advance { by_us: 20_000 }).await, Reply::Done { .. }));
    let boundary = current_boundary(&mut source).await;
    let begin = Action::PointerBegin { target: Locator::Css { selector: "#pad".into() }, point: [40, 40], button: Default::default(), pointer: frontcap_core::pointer::PointerKind::Touch, boundary };
    assert!(matches!(act(&mut source, "touch", begin).await, Reply::Done { .. }));
    let Reply::Imaged { .. } = source.client.call(Call::Image { name: "touch".into() }).await.unwrap() else { panic!("not imaged") };
    let mut child = launcher::launch(&ws, request("child", Some("touch".into())), &support::recorder_bin(), &env).await.unwrap();
    for (a, x) in [(&mut child, 150), (&mut source, 90)] {
        let Reply::Status { status } = a.client.call(Call::Status).await.unwrap() else { panic!("no status") };
        let pointer = status.pointer.expect("the held contact crossed");
        let contact = pointer.contacts.first().expect("one contact").id;
        let boundary = current_boundary(a).await;
        assert!(matches!(act(a, "touch-move", Action::TouchMove { handle: pointer.handle.clone(), contact, point: [x, 40], boundary }).await, Reply::Done { .. }));
        assert!(matches!(act(a, "lift", Action::PointerEnd { handle: pointer.handle }).await, Reply::Done { .. }));
        assert_eq!(text(a, "#log").await, format!("pointerdown:touch:40 pointermove:touch:{x} pointerup:touch:{x}"));
    }
    for a in [&mut child, &mut source] {
        assert!(matches!(mutate(a, "stop", Op::Stop).await, Reply::Done { .. }));
    }
}

/// Two generations imaged at the same moment, as two agents saving starting points would be: both images
/// are taken, and each restores a child that continues its source. CRIU mounts a `/proc` of the cell's
/// own in each task's working directory, which each cell's browser keeps to itself.
#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn two_generations_imaged_at_once_each_restore_a_child() {
    let env = [("FRONTCAP_CELLS".to_string(), "65534:65534".to_string())];
    for round in 0..3 {
        let (_tmp, ws) = support::workspace();
        let mut first = armed_as(&ws, &env, "first").await;
        let mut second = armed_as(&ws, &env, "second").await;
        let (a, b) = tokio::join!(first.client.call(Call::Image { name: "first".into() }), second.client.call(Call::Image { name: "second".into() }));
        assert!(matches!(a, Ok(Reply::Imaged { .. })), "round {round}: {a:?}");
        assert!(matches!(b, Ok(Reply::Imaged { .. })), "round {round}: {b:?}");
        for (name, source) in [("first", &mut first), ("second", &mut second)] {
            let request = launcher::LaunchRequest { restore: Some(name.into()), rules: rules(), passthrough_origins: Some(vec![]),
                ..support::launch_request(&format!("{name}-child"), "https://app.example.test/checkpoint.html") };
            let mut child = launcher::launch(&ws, request, &support::recorder_bin(), &env).await.unwrap();
            assert_eq!(continuation(&mut child, 1).await, [Value::from("3"), Value::from("3"), Value::from("answer:3")], "round {round}: {name}");
            assert!(matches!(mutate(&mut child, "stop", Op::Stop).await, Reply::Done { .. }));
            assert!(matches!(mutate(source, "stop", Op::Stop).await, Reply::Done { .. }));
        }
    }
}
