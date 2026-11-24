//! Worker-only frame demand preserves callback order and reaches presented canvas pixels.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};

fn pixels(path: &std::path::Path) -> [[u8; 3]; 2] {
    let mut reader = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buffer).unwrap();
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert!(matches!(
        info.color_type,
        png::ColorType::Rgb | png::ColorType::Rgba
    ));
    assert!(info.width > 96 && info.height > 32);
    [32, 96].map(|x| {
        let offset = (32 * info.width as usize + x) * info.color_type.samples();
        [buffer[offset], buffer[offset + 1], buffer[offset + 2]]
    })
}

// Worker frame batches use their own native microtask boundary. Measure the pinned
// ordinary browser with the identical worker source instead of borrowing page order.
async fn native_callback_order() -> Value {
    use cdp::protocol::{runtime, target};
    let root = support::repo_root();
    let pin = browser_pin::load_pin(&root).unwrap();
    let binary =
        browser_pin::install(&root, &pin, browser_pin::current_platform().unwrap()).unwrap();
    let browser = browser_pin::launch(&binary, &[], std::time::Duration::from_secs(20)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let page = targets
        .target_infos
        .into_iter()
        .find(|p| p.r#type == "page")
        .unwrap();
    let session = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(page.target_id)
            },
        )
        .await
        .unwrap()
        .session_id;
    let script = serde_json::to_string(include_str!("fixtures/worker_frames.js")).unwrap();
    let expression = format!(
        "new Promise(resolve=>{{const canvas=document.createElement('canvas');canvas.width=64;canvas.height=64;document.body.append(canvas);const worker=new Worker(URL.createObjectURL(new Blob([{script}],{{type:'text/javascript'}})));worker.onmessage=e=>resolve(e.data.order);const transfer=canvas.transferControlToOffscreen();worker.postMessage({{canvas:transfer,color:'#ff0000'}},[transfer]);}})"
    );
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.call(
            Some(&session.0),
            &runtime::EvaluateParams {
                await_promise: Some(true),
                return_by_value: Some(true),
                ..runtime::EvaluateParams::new(expression)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.exception_details.is_none(), "{reply:?}");
    let order = reply.result.value.unwrap();
    let entries = order.as_array().unwrap();
    assert_eq!(entries.first(), Some(&json!("first")));
    assert_eq!(entries.last(), Some(&json!("next")));
    assert!(entries.contains(&json!("microtask")) && entries.contains(&json!("promise")));
    assert!(!entries.contains(&json!("direct cancellation failed")));
    eprintln!("FRONTCAP_NATIVE_WORKER_CALLBACK_ORDER {order}");
    drop(browser);
    order
}

async fn callbacks_and_pixels(slices: &[u64], wall_hold: bool, native_order: &Value) -> Value {
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let options = support::options(
        &root,
        "worker-frames",
        format!("http://{addr}/worker_frames.html"),
        RuleSet::default(),
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(
        generation.eval_main("startWorkers()").await.unwrap(),
        json!(true)
    );
    for ms in slices {
        if wall_hold {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        }
        generation
            .advance(VirtualDuration::from_ms(*ms))
            .await
            .unwrap();
    }
    assert_eq!(generation.now().0, 60_000);
    let value = generation
        .eval_main("JSON.stringify(results)")
        .await
        .unwrap();
    let observed: Value = serde_json::from_str(value.as_str().unwrap()).unwrap();
    assert_eq!(observed.as_object().unwrap().len(), 2, "{observed}");
    for id in ["left", "right"] {
        let row = &observed[id];
        assert_eq!(
            &row["order"], native_order,
            "{id}: preserve the pinned browser worker order"
        );
        assert_eq!(row["first"]["date"], json!(epoch + 17), "{id}: {row}");
        assert_eq!(row["next"]["date"], json!(epoch + 34), "{id}: {row}");
        let delta = row["next"]["time"].as_f64().unwrap() - row["first"]["time"].as_f64().unwrap();
        assert!((16.0..=17.1).contains(&delta), "{id}: {row}");
    }
    let shot = generation.screenshot().await.unwrap();
    assert!(
        shot.current,
        "the returned screenshot must belong to the settled presentation"
    );
    assert_eq!(shot.t_us.0, 60_000);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    std::fs::write(root.join("worker-frame-observation.json"), serde_json::to_vec_pretty(&json!({
        "native_callback_order":native_order,"callbacks":observed,"slices_ms":slices,"wall_hold":wall_hold,"epoch_ms":epoch,
        "screenshot":shot.path,"screenshot_time_us":shot.t_us.0,"current":shot.current,
        "pixels":pixels(&shot.path),"boundary":"Two sibling workers only; no OOPIF, popup isolation, backpressure or retirement claim."
    })).unwrap()).unwrap();
    generation.stop().await.unwrap();
    observed
}

#[tokio::test]
async fn worker_frames_keep_microtasks_and_publish_their_canvas_pixels() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let native_order = native_callback_order().await;
    let whole = callbacks_and_pixels(&[50], false, &native_order).await;
    let sliced = callbacks_and_pixels(&[7, 17, 26], true, &native_order).await;
    assert_eq!(
        whole, sliced,
        "virtual results survive slicing and wall-only holds"
    );
}

async fn frames_during_synchronous_service(stops: &[u64]) -> Value {
    use frontcap_core::{event::EventKind, log::BehaviorLog};
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules = RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-frame-service",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation
        .eval_main(&format!(
            r#"(()=>{{
      const source={source};window.serviceResults={{}};window.serviceWorkers=[];
      for(const [id,color,synchronous] of [['left','#ff0000',true],['right','#00ff00',false]]){{
        const worker=new Worker(URL.createObjectURL(new Blob([source],{{type:'text/javascript'}})));
        serviceWorkers.push(worker);worker.onmessage=e=>serviceResults[id]=e.data;
        const canvas=document.getElementById(id).transferControlToOffscreen();
        worker.postMessage({{canvas,color,synchronous}},[canvas]);
      }}return true;
    }})()"#
        ))
        .await
        .unwrap();
    for stop in stops {
        let now = generation.now().0;
        assert!(
            now <= stop * 1000,
            "service must not exceed the next requested boundary"
        );
        generation
            .advance(VirtualDuration::from_micros(stop * 1000 - now))
            .await
            .unwrap();
    }
    assert_eq!(generation.now().0, 120_000);
    let value = generation
        .eval_main("JSON.stringify(serviceResults)")
        .await
        .unwrap();
    let observed: Value = serde_json::from_str(value.as_str().unwrap()).unwrap();
    std::fs::write(
        root.join("worker-frame-service-observation.json"),
        serde_json::to_vec_pretty(&observed).unwrap(),
    )
    .unwrap();
    eprintln!("FRONTCAP_WORKER_FRAME_SERVICE {observed}");
    let left = observed["left"]["callbacks"].as_array().unwrap();
    let right = observed["right"]["callbacks"].as_array().unwrap();
    assert_eq!((left.len(), right.len()), (2, 2));
    assert_eq!(left[0]["before"], json!(epoch + 17));
    assert_eq!(left[0]["after"], json!(epoch + 57));
    assert_eq!(left[0]["status"], json!(200));
    assert_eq!(left[0]["body"], json!("served"));
    for rows in [left, right] {
        assert_eq!(rows[0]["time"], json!(7));
        assert_eq!(rows[1]["before"], json!(epoch + 67));
        assert_eq!(rows[1]["time"], json!(57));
    }
    assert!([json!(epoch + 17), json!(epoch + 57)].contains(&right[0]["before"]));
    assert_eq!(right[0]["before"], right[0]["after"]);
    let events = BehaviorLog::read(&generation.trace_path()).unwrap();
    let requests: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Request(r) if r.url.ends_with("/frame-service")))
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].t_us.0, 17_000);
    let request_id = match &requests[0].kind {
        EventKind::Request(r) => r.id.clone(),
        _ => unreachable!(),
    };
    let responses: Vec<_> = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Response(r) if r.id == request_id))
        .collect();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].t_us.0, 57_000);
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, 120_000);
    assert_eq!(
        generation
            .eval_main("JSON.stringify(serviceResults)")
            .await
            .unwrap(),
        value
    );
    generation.stop().await.unwrap();
    observed
}

#[tokio::test]
async fn worker_frame_service_keeps_the_approved_argument_and_presents_both_canvases() {
    assert!(support::controlled_engine_or_check_baseline().await);
    for stops in [&[120][..], &[17, 70, 120][..]] {
        frames_during_synchronous_service(stops).await;
    }
}

#[tokio::test]
async fn a_worker_wait_outside_the_frame_does_not_extend_its_approved_tick() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"outside-frame","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-frame-scope",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    generation
        .eval_main(
            r#"(()=>{
      window.outsideFrame=null;
      window.timerWorker=new Worker(URL.createObjectURL(new Blob([`
        setTimeout(()=>{const before=Date.now();const request=new XMLHttpRequest();
        request.open('GET','https://api.example.test/frame-service',false);request.send();
        postMessage({before,after:Date.now(),body:request.responseText});},6);
      `],{type:'text/javascript'})));
      timerWorker.onmessage=e=>outsideFrame=e.data;
      return startWorkers();
    })()"#,
        )
        .await
        .unwrap();
    let first = generation.advance(VirtualDuration::from_ms(7)).await;
    let at = generation.now().0;
    std::fs::write(
        root.join("worker-frame-scope-boundary.json"),
        serde_json::to_vec_pretty(
            &json!({"outcome":format!("{first:?}"),"now_us":at,"expected_us":17_000}),
        )
        .unwrap(),
    )
    .unwrap();
    first.unwrap();
    assert_eq!(
        at, 17_000,
        "an unrelated timer's wait must not extend the approved frame"
    );
    assert_eq!(
        generation.eval_main("outsideFrame").await.unwrap(),
        Value::Null
    );
    generation
        .advance(VirtualDuration::from_ms(43))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 60_000);
    assert_eq!(
        generation.eval_main("outsideFrame").await.unwrap(),
        json!({"before":epoch+16,"after":epoch+56,"body":"served"})
    );
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    generation.stop().await.unwrap();
}

#[tokio::test]
async fn retiring_a_worker_with_an_unfinished_frame_refuses_the_frame() {
    use frontcap_chromium::ChromiumError;
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules = RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-pending-retirement",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation
        .eval_main(&format!(
            r#"(()=>{{
      window.retirement={{at:null,messages:[]}};
      const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));
      window.retiringWorker=worker;
      worker.onmessage=e=>retirement.messages.push(e.data);
      const canvas=document.getElementById('left').transferControlToOffscreen();
      worker.postMessage({{canvas,color:'#ff0000',synchronous:true}},[canvas]);
      setTimeout(()=>{{retirement.at=Date.now();worker.terminate();}},10);
      return true;
    }})()"#
        ))
        .await
        .unwrap();
    let outcome = generation.advance(VirtualDuration::from_ms(110)).await;
    let at = generation.now().0;
    std::fs::write(
        root.join("worker-pending-retirement-boundary.json"),
        serde_json::to_vec_pretty(
            &json!({"outcome":format!("{outcome:?}"),"now_us":at,"epoch_ms":epoch}),
        )
        .unwrap(),
    )
    .unwrap();
    let error = outcome.expect_err("a retired worker cannot acknowledge the outstanding frame");
    assert!(
        matches!(&error, ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }) if message.contains("Owned worker frame did not settle")),
        "{error:?}"
    );
    let observed = generation
        .eval_main("JSON.stringify(retirement)")
        .await
        .unwrap();
    let row: Value = serde_json::from_str(observed.as_str().unwrap()).unwrap();
    assert_eq!(row["at"], json!(epoch + 20));
    assert_eq!(row["messages"], json!([]));
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, at);
    assert_eq!(
        generation
            .eval_main("JSON.stringify(retirement)")
            .await
            .unwrap(),
        observed
    );
    std::fs::write(
        root.join("worker-pending-retirement-observation.json"),
        serde_json::to_vec_pretty(&row).unwrap(),
    )
    .unwrap();
    generation.stop().await.unwrap();
}

#[tokio::test]
async fn a_response_stop_inside_a_worker_frame_can_resume_the_original_frame() {
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-response-stop",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation.eval_main(&format!(r#"(()=>{{
      window.serviceResults={{}};window.serviceWorkers=[];
      for(const [id,color,synchronous] of [['left','#ff0000',true],['right','#00ff00',false]]){{
        const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));
        serviceWorkers.push(worker);worker.onmessage=e=>serviceResults[id]=e.data;
        const canvas=document.getElementById(id).transferControlToOffscreen();
        worker.postMessage({{canvas,color,synchronous}},[canvas]);
      }}return true;
    }})()"#)).await.unwrap();
    let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
    let result = generation
        .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
        .await;
    std::fs::write(root.join("worker-response-stop-boundary.json"),serde_json::to_vec_pretty(&json!({"outcome":format!("{result:?}"),"now_us":generation.now().0,"expected_us":57_000})).unwrap()).unwrap();
    let outcome = result.unwrap();
    assert!(
        matches!(outcome,ClockOutcome::Event{event:ProgressEvent::Response,ref triggers} if !triggers.is_empty()),
        "{outcome:?}"
    );
    assert_eq!(generation.now().0, 57_000);
    let stopped = generation
        .eval_main("JSON.stringify(serviceResults)")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, 57_000);
    assert_eq!(
        generation
            .eval_main("JSON.stringify(serviceResults)")
            .await
            .unwrap(),
        stopped
    );
    generation
        .advance(VirtualDuration::from_ms(63))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120_000);
    let observed: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(serviceResults)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for id in ["left", "right"] {
        let rows = observed[id]["callbacks"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["time"], json!(7));
        assert_eq!(rows[1]["time"], json!(57));
        assert_eq!(rows[1]["before"], json!(epoch + 67));
    }
    assert_eq!(
        observed["left"]["callbacks"][0]["before"],
        json!(epoch + 17)
    );
    assert_eq!(observed["left"]["callbacks"][0]["after"], json!(epoch + 57));
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    std::fs::write(
        root.join("worker-response-stop-resumed.json"),
        serde_json::to_vec_pretty(&observed).unwrap(),
    )
    .unwrap();
    generation.stop().await.unwrap();
}

async fn stop_before_the_worker_frame_response(cancel: bool) {
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[
      {"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}},
      {"id":"earlier-response","match":{"url":"https://api.example.test/earlier-response"},"action":{"type":"mock","status":200,"body":"earlier","headers":{"access-control-allow-origin":"*"},"latency_ms":10}}
    ]"#).unwrap();
    let options = support::options(
        &root,
        "worker-frame-early-stop",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation.eval_main(&format!(r#"(()=>{{
      window.serviceResults={{}};window.serviceWorkers=[];window.earlier=null;
      for(const [id,color,synchronous] of [['left','#ff0000',true],['right','#00ff00',false]]){{
        const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));
        serviceWorkers.push(worker);worker.onmessage=e=>serviceResults[id]=e.data;
        const canvas=document.getElementById(id).transferControlToOffscreen();
        worker.postMessage({{canvas,color,synchronous}},[canvas]);
      }}
      if(!{cancel})setTimeout(()=>fetch('https://api.example.test/earlier-response').then(r=>r.text()).then(body=>earlier={{body,at:Date.now()}}),10);
      return true;
    }})()"#)).await.unwrap();
    let result = if cancel {
        generation.arm_cancel(Some(VirtualTime(34_000)));
        generation.advance(VirtualDuration::from_ms(110)).await
    } else {
        let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
        generation
            .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
            .await
    };
    let expected = if cancel { 34_000 } else { 30_000 };
    std::fs::write(root.join("worker-frame-early-stop-boundary.json"),serde_json::to_vec_pretty(&json!({"cancel":cancel,"outcome":format!("{result:?}"),"now_us":generation.now().0,"expected_us":expected})).unwrap()).unwrap();
    let outcome = result.unwrap();
    if cancel {
        assert_eq!(outcome, ClockOutcome::Cancelled);
        assert_eq!(generation.take_cancelled(), Some(VirtualTime(34_000)));
    } else {
        assert!(
            matches!(outcome,ClockOutcome::Event{event:ProgressEvent::Response,ref triggers} if !triggers.is_empty()),
            "{outcome:?}"
        );
    }
    assert_eq!(generation.now().0, expected);
    let stopped = generation
        .eval_main("JSON.stringify({serviceResults,earlier})")
        .await
        .unwrap();
    let before: Value = serde_json::from_str(stopped.as_str().unwrap()).unwrap();
    assert!(before["serviceResults"]["left"].is_null());
    if !cancel {
        assert_eq!(before["earlier"], json!({"body":"earlier","at":epoch+30}));
    }
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, expected);
    assert_eq!(
        generation
            .eval_main("JSON.stringify({serviceResults,earlier})")
            .await
            .unwrap(),
        stopped
    );
    generation
        .advance(VirtualDuration::from_micros(120_000 - expected))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120_000);
    let observed: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(serviceResults)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for id in ["left", "right"] {
        let rows = observed[id]["callbacks"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["time"], json!(7));
        assert_eq!(rows[1]["time"], json!(57));
        assert_eq!(rows[1]["before"], json!(epoch + 67));
    }
    assert_eq!(
        observed["left"]["callbacks"][0]["before"],
        json!(epoch + 17)
    );
    assert_eq!(observed["left"]["callbacks"][0]["after"], json!(epoch + 57));
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    std::fs::write(root.join("worker-frame-early-stop-resumed.json"),serde_json::to_vec_pretty(&json!({"cancel":cancel,"stopped":before,"resumed":observed,"pixels":pixels(&shot.path)})).unwrap()).unwrap();
    generation.stop().await.unwrap();
}

#[tokio::test]
async fn an_earlier_response_stop_retains_the_unfinished_worker_frame() {
    stop_before_the_worker_frame_response(false).await;
}

#[tokio::test]
async fn cancelling_before_a_worker_frame_response_retains_the_original_frame() {
    stop_before_the_worker_frame_response(true).await;
}

#[tokio::test]
async fn a_failed_frame_does_not_leave_a_stop_on_the_next_progress_request() {
    use frontcap_chromium::ChromiumError;
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[
      {"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}},
      {"id":"earlier-response","match":{"url":"https://api.example.test/earlier-response"},"action":{"type":"mock","status":200,"body":"earlier","headers":{"access-control-allow-origin":"*"},"latency_ms":10}}
    ]"#).unwrap();
    let options = support::options(
        &root,
        "worker-frame-error-stop",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation.eval_main(&format!(r#"(()=>{{
      window.retirement={{at:null,messages:[]}};
      const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));
      window.retiringWorker=worker;worker.onmessage=e=>retirement.messages.push(e.data);
      const canvas=document.getElementById('left').transferControlToOffscreen();
      worker.postMessage({{canvas,color:'#ff0000',synchronous:true}},[canvas]);
      setTimeout(()=>{{retirement.at=Date.now();worker.terminate();fetch('https://api.example.test/earlier-response');}},10);
      return true;
    }})()"#)).await.unwrap();
    let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
    let refused = generation
        .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
        .await;
    let before = generation.now().0;
    std::fs::write(
        root.join("worker-frame-error-first.json"),
        serde_json::to_vec_pretty(&json!({"outcome":format!("{refused:?}"),"now_us":before}))
            .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(&refused,Err(ChromiumError::Cdp(cdp::CdpError::Protocol{message,..})) if message.contains("Owned worker frame did not settle")),
        "{refused:?}"
    );
    let retired: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(retirement)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retired["at"], json!(epoch + 20));
    assert_eq!(retired["messages"], json!([]));
    let next = generation.advance(VirtualDuration::from_ms(90)).await;
    std::fs::write(root.join("worker-frame-error-next.json"),serde_json::to_vec_pretty(&json!({"outcome":format!("{next:?}"),"before_us":before,"now_us":generation.now().0,"expected_us":before+90_000})).unwrap()).unwrap();
    assert_eq!(next.unwrap(), ClockOutcome::Reached);
    assert_eq!(
        generation.now().0,
        before + 90_000,
        "the previous failed request cannot stop a new advance"
    );
    generation.stop().await.unwrap();
}

const OWNED_FRAME_WORKER: &str = r#"
onmessage=({data:{canvas,color,synchronous}})=>{
  const context=canvas.getContext('2d');globalThis.frameCalls=[];globalThis.frameArmed=false;
  const publish=()=>{context.fillStyle=color;context.fillRect(0,0,64,64);postMessage({callbacks:frameCalls});};
  requestAnimationFrame(time=>{
    const first={time,before:Date.now()};
    if(synchronous){const request=new XMLHttpRequest();request.open('GET','https://api.example.test/frame-service',false);request.send();first.body=request.responseText;}
    first.after=Date.now();frameCalls.push(first);publish();
    requestAnimationFrame(time=>{frameCalls.push({time,before:Date.now(),after:Date.now()});publish();});
  });
  frameArmed=true;
};
"#;

async fn start_owned_frame_workers(generation: &Generation, synchronous: bool) {
    let source = serde_json::to_string(OWNED_FRAME_WORKER).unwrap();
    generation.eval_main(&format!(r#"(()=>{{
      window.serviceResults={{}};window.workerUrls={{}};window.frameWorkers=[];
      for(const [id,color] of [['left','#ff0000'],['right','#00ff00']]){{
        const url=URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}}));workerUrls[id]=url;
        const worker=new Worker(url);frameWorkers.push(worker);worker.onmessage=e=>serviceResults[id]=e.data;
        const canvas=document.getElementById(id).transferControlToOffscreen();
        worker.postMessage({{canvas,color,synchronous:{synchronous}&&id==='left'}},[canvas]);
      }}return true;
    }})()"#)).await.unwrap();
}

async fn native_owned_frame_counts(generation: &Generation, epoch: u64) -> Value {
    use cdp::protocol::{emulation, runtime, target};
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap()
        .target_infos;
    let popup = targets
        .iter()
        .filter(|t| {
            t.r#type == "page"
                && t.opener_id
                    .as_ref()
                    .is_some_and(|id| id.0 == endpoint.target_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(popup.len(), 1);
    let mut result = serde_json::Map::new();
    let mut identities = std::collections::BTreeSet::new();
    for (name, id, expected) in [
        ("root", target::TargetID(endpoint.target_id.clone()), 1),
        ("popup", popup[0].target_id.clone(), 0),
    ] {
        let session = client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(id)
                },
            )
            .await
            .unwrap()
            .session_id;
        let before = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            before.unix_time_us.as_deref(),
            Some((epoch * 1000 + 74_000).to_string().as_str())
        );
        let native = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetWorkersParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(native.workers.len(), 2);
        let mut rows = Vec::new();
        for worker in native.workers {
            assert!(worker.thread_started && !worker.creator_is_worker);
            assert!(identities.insert(worker.target_id.clone()));
            let ws = client
                .call(
                    None,
                    &target::AttachToTargetParams {
                        flatten: Some(true),
                        ..target::AttachToTargetParams::new(target::TargetID(
                            worker.target_id.clone(),
                        ))
                    },
                )
                .await
                .unwrap()
                .session_id;
            let global = client
                .call(
                    Some(&ws.0),
                    &runtime::EvaluateParams {
                        throw_on_side_effect: Some(true),
                        return_by_value: Some(false),
                        await_promise: Some(false),
                        silent: Some(true),
                        ..runtime::EvaluateParams::new("globalThis".into())
                    },
                )
                .await
                .unwrap();
            assert!(global.exception_details.is_none());
            let properties = client
                .call(
                    Some(&ws.0),
                    &runtime::GetPropertiesParams {
                        own_properties: Some(true),
                        ..runtime::GetPropertiesParams::new(global.result.object_id.unwrap())
                    },
                )
                .await
                .unwrap()
                .result;
            let armed = properties
                .iter()
                .find(|p| p.name == "frameArmed")
                .and_then(|p| p.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .unwrap();
            assert_eq!(armed, &json!(true));
            let calls = properties
                .iter()
                .find(|p| p.name == "frameCalls")
                .and_then(|p| p.value.as_ref())
                .and_then(|v| v.object_id.clone())
                .unwrap();
            let entries = client
                .call(
                    Some(&ws.0),
                    &runtime::GetPropertiesParams {
                        own_properties: Some(true),
                        ..runtime::GetPropertiesParams::new(calls)
                    },
                )
                .await
                .unwrap()
                .result;
            let count = entries
                .iter()
                .find(|p| p.name == "length")
                .and_then(|p| p.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .unwrap();

            rows.push(json!({"target":worker.target_id,"creator":worker.creator_id,"script":worker.script_url,"calls":count,"expectedCalls":expected,"armed":armed}));
            client
                .call(
                    None,
                    &target::DetachFromTargetParams {
                        session_id: Some(ws),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        let after = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(before.unix_time_us, after.unix_time_us);
        assert_eq!(before.renderer_id, after.renderer_id);
        result.insert(
            name.into(),
            json!({"workers":rows,"renderer":after.renderer_id,"clock":after.unix_time_us}),
        );
        client
            .call(
                None,
                &target::DetachFromTargetParams {
                    session_id: Some(session),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(identities.len(), 4);
    Value::Object(result)
}

async fn inspect_popup_frame_presentation(generation: &mut Generation) -> Result<Value, String> {
    use cdp::protocol::{emulation, page, target};
    struct ReadConnection(cdp::Client);
    impl Drop for ReadConnection {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let endpoint = generation.endpoint();
    let connection = ReadConnection(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .map_err(|e| format!("{e:?}"))?,
    );
    let client = &connection.0;
    let session = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
            },
        )
        .await
        .map_err(|e| format!("{e:?}"))?
        .session_id;
    let before_clock = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetClockParams::default(),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let before_frame = client
        .call(Some(&session.0), &page::GetPresentedFrameParams::default())
        .await
        .map_err(|e| format!("{e:?}"))?;
    let native_workers = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetWorkersParams::default(),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let before_count = generation.frame_requests();
    let before_time = generation.now().0;
    let dom=generation.eval_main(r#"JSON.stringify({url:location.href,visibility:document.visibilityState,focused:document.hasFocus(),html:document.documentElement.outerHTML.slice(0,16384),canvases:Array.from(document.querySelectorAll('canvas')).map(c=>{const r=c.getBoundingClientRect();return {id:c.id,width:c.width,height:c.height,rect:{left:r.left,top:r.top,width:r.width,height:r.height},display:getComputedStyle(c).display,visibility:getComputedStyle(c).visibility}}),results:serviceResults})"#).await.map_err(|e|format!("{e:?}"))?;
    let shot = generation
        .screenshot()
        .await
        .map_err(|e| format!("{e:?}"))?;
    let wanted = client
        .call(
            Some(&session.0),
            &emulation::IsMainFrameWantedParams::default(),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let after_frame = client
        .call(Some(&session.0), &page::GetPresentedFrameParams::default())
        .await
        .map_err(|e| format!("{e:?}"))?;
    let after_clock = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetClockParams::default(),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    let detached = client
        .call(
            None,
            &target::DetachFromTargetParams {
                session_id: Some(session),
                ..Default::default()
            },
        )
        .await;
    Ok(
        json!({"target":endpoint.target_id,"beforeClock":before_clock,"afterClock":after_clock,"beforeFrame":before_frame,"afterFrame":after_frame,"nativeWorkers":native_workers,"frameWanted":wanted,"dom":dom,"screenshot":{"path":shot.path,"current":shot.current,"time_us":shot.t_us.0,"painted_at_us":shot.painted_at_us,"pixels":pixels(&shot.path)},"beforeFrameRequests":before_count,"afterFrameRequests":generation.frame_requests(),"beforeTimeUs":before_time,"afterTimeUs":generation.now().0,"detach":format!("{detached:?}")}),
    )
}

async fn popup_worker_frames_with_optional_stop(stop: bool) -> Value {
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        profile::PopupPolicy,
        time::VirtualTime,
    };
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let url = format!("http://{addr}/worker_frames.html");
    let mut options = support::options(&root, "worker-popup-frame", url.clone(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    generation
        .eval_main(&format!(
            "(()=>{{window.framePopup=window.open({},'worker-popup');return !!framePopup;}})()",
            serde_json::to_string(&url).unwrap()
        ))
        .await
        .unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 20_000);
    generation.enter_context("popup-1").unwrap();
    start_owned_frame_workers(&generation, false).await;
    generation.leave_context();
    start_owned_frame_workers(&generation, true).await;
    let before = generation.frame_requests();
    if stop {
        let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
        let outcome = generation
            .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ClockOutcome::Event {
                event: ProgressEvent::Response,
                ..
            }
        ));
        assert_eq!(generation.now().0, 74_000);
        assert_eq!(
            generation.frame_requests(),
            before + 1,
            "only the root native frame was dispatched"
        );
        let owned = native_owned_frame_counts(&generation, epoch).await;
        std::fs::write(
            root.join("worker-popup-owned-first.json"),
            serde_json::to_vec_pretty(&owned).unwrap(),
        )
        .unwrap();
        for (owner, expected) in [("root", 1), ("popup", 0)] {
            for worker in owned[owner]["workers"].as_array().unwrap() {
                assert_eq!(worker["calls"], json!(expected), "{owner}: {worker}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert_eq!(native_owned_frame_counts(&generation, epoch).await, owned);
        assert_eq!(generation.now().0, 74_000);
        std::fs::write(
            root.join("worker-popup-owned-stop.json"),
            serde_json::to_vec_pretty(&owned).unwrap(),
        )
        .unwrap();
    }
    generation
        .advance(VirtualDuration::from_micros(120_000 - generation.now().0))
        .await
        .unwrap();
    let mut observed = serde_json::Map::new();
    for (name, context, first, arg) in [("root", None, 34, 14), ("popup", Some("popup-1"), 74, 54)]
    {
        if let Some(context) = context {
            generation.enter_context(context).unwrap();
        }
        let value: Value = serde_json::from_str(
            generation
                .eval_main("JSON.stringify(serviceResults)")
                .await
                .unwrap()
                .as_str()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join(format!("worker-popup-{name}-raw.json")),
            serde_json::to_vec_pretty(&json!({"stop":stop,"observed":value})).unwrap(),
        )
        .unwrap();
        for id in ["left", "right"] {
            let rows = value[id]["callbacks"].as_array().unwrap();
            assert_eq!(rows.len(), 2, "{name}/{id}: {value}");
            assert_eq!(rows[0]["time"], json!(arg));
            assert_eq!(rows[1]["time"], json!(64));
            assert_eq!(rows[1]["before"], json!(epoch + 84));
            if name == "root" && id == "right" {
                assert!([json!(epoch + 34), json!(epoch + 74)].contains(&rows[0]["before"]));
                assert_eq!(rows[0]["before"], rows[0]["after"]);
            } else {
                assert_eq!(rows[0]["before"], json!(epoch + first));
                assert_eq!(rows[0]["after"], json!(epoch + 74));
            }
        }
        if name == "root" {
            assert_eq!(value["left"]["callbacks"][0]["body"], json!("served"));
        }
        let shot = generation.screenshot().await.unwrap();
        assert!(shot.current);
        if name == "root" {
            let root_state = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                inspect_popup_frame_presentation(&mut generation),
            )
            .await;
            generation.enter_context("popup-1").unwrap();
            let popup_state = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                inspect_popup_frame_presentation(&mut generation),
            )
            .await;
            generation.leave_context();
            std::fs::write(
                root.join("worker-popup-presentation-state.json"),
                serde_json::to_vec_pretty(
                    &json!({"root":root_state.map_err(|e|e.to_string()).and_then(|r|r),"popup":popup_state.map_err(|e|e.to_string()).and_then(|r|r)}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
        observed.insert(name.into(), value);
        if context.is_some() {
            generation.leave_context();
        }
    }
    let observed = Value::Object(observed);
    std::fs::write(
        root.join("worker-popup-resumed.json"),
        serde_json::to_vec_pretty(&json!({"stop":stop,"observed":observed})).unwrap(),
    )
    .unwrap();
    generation.stop().await.unwrap();
    observed
}

#[tokio::test]
async fn stopping_a_root_frame_preserves_unrelated_popup_worker_opportunities() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let whole = popup_worker_frames_with_optional_stop(false).await;
    let stopped = popup_worker_frames_with_optional_stop(true).await;
    // A sibling's first callback may run on either side of the serviced response.
    // Keep both raw dates in evidence; compare every other field, including its approved rAF argument.
    let comparable = |mut value: Value| {
        let first = value["root"]["right"]["callbacks"][0]
            .as_object_mut()
            .unwrap();
        first.remove("before");
        first.remove("after");
        value
    };
    assert_eq!(
        comparable(whole),
        comparable(stopped),
        "a stop must resume the original window sequence without another root batch"
    );
}

async fn single_window_owned_worker_pixels(start_ms: u64) {
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-single",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(start_ms))
        .await
        .unwrap();
    assert_eq!(generation.now().0, start_ms * 1000);
    start_owned_frame_workers(&generation, true).await;
    generation
        .advance(VirtualDuration::from_ms(120 - start_ms))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120000);
    let observed = generation
        .eval_main("JSON.stringify(serviceResults)")
        .await
        .unwrap();
    let row: Value = serde_json::from_str(observed.as_str().unwrap()).unwrap();
    let shot = generation.screenshot().await.unwrap();
    let first = if start_ms == 10 { 17 } else { 34 };
    let second = if start_ms == 10 { 67 } else { 84 };
    let diagnostic = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        inspect_popup_frame_presentation(&mut generation),
    )
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    std::fs::write(root.join("worker-single-boundary.json"),serde_json::to_vec_pretty(&json!({"start_ms":start_ms,"callbacks":row,"originalScreenshot":{"path":shot.path,"current":shot.current,"painted_at_us":shot.painted_at_us,"pixels":pixels(&shot.path)},"presentation":diagnostic})).unwrap()).unwrap();
    for id in ["left", "right"] {
        let calls = row[id]["callbacks"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["time"], json!(first - start_ms));
        assert_eq!(calls[1]["time"], json!(second - start_ms));
        assert_eq!(calls[1]["before"], json!(epoch + second));
    }
    assert_eq!(row["left"]["callbacks"][0]["before"], json!(epoch + first));
    assert_eq!(
        row["left"]["callbacks"][0]["after"],
        json!(epoch + first + 40)
    );
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    generation.stop().await.unwrap();
}
#[tokio::test]
async fn worker_canvas_transfer_before_first_presentation_has_pixels() {
    single_window_owned_worker_pixels(10).await;
}
#[tokio::test]
async fn worker_canvas_transfer_after_first_presentation_has_pixels() {
    single_window_owned_worker_pixels(20).await;
}

#[tokio::test]
async fn a_late_canvas_transfer_stays_pending_until_explicit_progress() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let options = support::options(
        &root,
        "worker-pending-canvas",
        format!("http://{addr}/worker_frames.html"),
        RuleSet::default(),
    );
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(20))
        .await
        .unwrap();
    let before = generation.screenshot().await.unwrap();
    assert!(before.current);
    assert_eq!(pixels(&before.path), [[255, 255, 255], [255, 255, 255]]);
    let frames = generation.frame_requests();
    start_owned_frame_workers(&generation, false).await;
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        inspect_popup_frame_presentation(&mut generation),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        inspect_popup_frame_presentation(&mut generation),
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::write(
        root.join("worker-canvas-stopped.json"),
        serde_json::to_vec_pretty(&json!({"first":first,"second":second,"frames":frames})).unwrap(),
    )
    .unwrap();
    for state in [&first, &second] {
        assert_eq!(state["beforeTimeUs"], json!(20000));
        assert_eq!(state["afterTimeUs"], json!(20000));
        assert_eq!(state["beforeFrameRequests"], json!(frames));
        assert_eq!(state["afterFrameRequests"], json!(frames));
        assert_eq!(state["beforeClock"], state["afterClock"]);
        assert_eq!(state["beforeFrame"], state["afterFrame"]);
        assert_eq!(state["frameWanted"]["wanted"], json!(true));
        assert_eq!(state["screenshot"]["current"], json!(false));
        assert_eq!(
            state["screenshot"]["pixels"],
            json!([[255, 255, 255], [255, 255, 255]])
        );
        let dom: Value = serde_json::from_str(state["dom"].as_str().unwrap()).unwrap();
        assert_eq!(dom["results"], json!({}));
    }
    assert_eq!(first["beforeClock"], second["afterClock"]);
    assert_eq!(first["beforeFrame"], second["afterFrame"]);
    assert_eq!(generation.now().0, 20000);
    assert_eq!(generation.frame_requests(), frames);
    generation
        .advance(VirtualDuration::from_ms(100))
        .await
        .unwrap();
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    generation.stop().await.unwrap();
}

async fn native_child_worker_ownership(generation: &Generation) -> Value {
    use cdp::protocol::{emulation, page, target};
    struct Reader(cdp::Client);
    impl Drop for Reader {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let endpoint = generation.endpoint();
    let reader = Reader(
        cdp::Client::connect(&endpoint.browser_ws_url)
            .await
            .unwrap(),
    );
    let client = &reader.0;
    let session = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
            },
        )
        .await
        .unwrap()
        .session_id;
    let before = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetClockParams::default(),
        )
        .await
        .unwrap();
    let tree = client
        .call(Some(&session.0), &page::GetFrameTreeParams::default())
        .await
        .unwrap()
        .frame_tree;
    let workers = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetWorkersParams::default(),
        )
        .await
        .unwrap()
        .workers;
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap()
        .target_infos;
    let after = client
        .call(
            Some(&session.0),
            &emulation::FrontcapGetClockParams::default(),
        )
        .await
        .unwrap();
    assert_eq!(before.unix_time_us, after.unix_time_us);
    assert_eq!(before.renderer_id, after.renderer_id);
    client
        .call(
            None,
            &target::DetachFromTargetParams {
                session_id: Some(session),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    json!({"clock":before,"tree":tree,"workers":workers,"workerTargets":targets.into_iter().filter(|t|t.r#type=="worker").collect::<Vec<_>>()})
}

async fn child_worker_frame_with_optional_retirement(retire: bool) {
    use frontcap_chromium::ChromiumError;
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let url = format!("http://{addr}/worker_frames.html");
    let options = support::options(&root, "worker-child-frame", url.clone(), rules);
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    generation.eval_main(&format!(r#"(()=>{{window.ownedMessages=[];window.childRemovedAt=null;window.frameHost=document.createElement('iframe');frameHost.style='position:absolute;left:0;top:0;width:128px;height:64px;border:0;display:block;z-index:10';frameHost.src={url};document.body.append(frameHost);window.childWindow=frameHost.contentWindow;addEventListener('message',e=>{{if(e.source===childWindow&&e.data.owner==='frame-canvas')ownedMessages.push({{receivedAt:Date.now(),...e.data}});}});return true;}})()"#,url=serde_json::to_string(&url).unwrap())).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 20000);
    assert_eq!(
        generation
            .eval_main("frameHost.contentDocument.readyState")
            .await
            .unwrap(),
        json!("complete")
    );
    let source = serde_json::to_string(OWNED_FRAME_WORKER).unwrap();
    let setup = format!(
        r#"(()=>{{window.serviceResults={{}};window.frameWorkers=[];for(const [id,color] of [['left','#ff0000'],['right','#00ff00']]){{const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));frameWorkers.push(worker);worker.onmessage=e=>{{serviceResults[id]=e.data;parent.postMessage({{owner:'frame-canvas',id,callbacks:e.data.callbacks}},'*');}};const canvas=document.getElementById(id).transferControlToOffscreen();worker.postMessage({{canvas,color,synchronous:id==='left'}},[canvas]);}}return true;}})()"#
    );
    generation
        .eval_main(&format!(
            "frameHost.contentWindow.eval({})",
            serde_json::to_string(&setup).unwrap()
        ))
        .await
        .unwrap();
    generation
        .advance(VirtualDuration::from_ms(0))
        .await
        .unwrap();
    let initial = native_child_worker_ownership(&generation).await;
    std::fs::write(
        root.join("worker-child-initial.json"),
        serde_json::to_vec_pretty(&initial).unwrap(),
    )
    .unwrap();
    let children = initial["tree"]["childFrames"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    let child = &children[0]["frame"];
    let document = child["frontcapDocumentId"].as_str().unwrap();
    assert_ne!(
        child["frontcapDocumentId"],
        initial["tree"]["frame"]["frontcapDocumentId"]
    );
    let workers = initial["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 2);
    for worker in workers {
        assert_eq!(worker["creatorId"], json!(document));
        assert_eq!(worker["creatorIsWorker"], json!(false));
        assert_eq!(worker["threadStarted"], json!(true));
        let target = initial["workerTargets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["targetId"] == worker["targetId"])
            .unwrap();
        assert_eq!(target["parentFrameId"], child["id"]);
        assert_eq!(target["frontcapCreatorDocumentId"], json!(document));
    }
    if retire {
        generation
            .eval_main("setTimeout(()=>{childRemovedAt=Date.now();frameHost.remove();},20);true")
            .await
            .unwrap();
    }
    let outcome = generation.advance(VirtualDuration::from_ms(100)).await;
    let at = generation.now().0;
    std::fs::write(
        root.join("worker-child-progress.json"),
        serde_json::to_vec_pretty(
            &json!({"retire":retire,"outcome":format!("{outcome:?}"),"time_us":at}),
        )
        .unwrap(),
    )
    .unwrap();
    if retire {
        let error = outcome.expect_err(
            "retiring the frame's owner cannot acknowledge its unfinished worker batch",
        );
        assert!(
            matches!(&error,ChromiumError::Cdp(cdp::CdpError::Protocol{message,..}) if message.contains("Owned worker frame did not settle")),
            "{error:?}"
        );
        let observed = generation
            .eval_main("JSON.stringify({removedAt:childRemovedAt,messages:ownedMessages})")
            .await
            .unwrap();
        let values: Value = serde_json::from_str(observed.as_str().unwrap()).unwrap();
        assert_eq!(values["removedAt"], json!(epoch + 40));
        for message in values["messages"].as_array().unwrap() {
            assert!(message["receivedAt"].as_u64().unwrap() <= epoch + 40);
            assert_ne!(message["id"], json!("left"));
        }
        let retired = native_child_worker_ownership(&generation).await;
        assert_eq!(retired["workers"], json!([]));
        assert!(
            retired["tree"]["childFrames"].is_null() || retired["tree"]["childFrames"] == json!([])
        );
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert_eq!(generation.now().0, at);
        assert_eq!(
            generation
                .eval_main("JSON.stringify({removedAt:childRemovedAt,messages:ownedMessages})")
                .await
                .unwrap(),
            observed
        );
        assert_eq!(
            native_child_worker_ownership(&generation).await["workers"],
            json!([])
        );
        std::fs::write(
            root.join("worker-child-retired.json"),
            serde_json::to_vec_pretty(&json!({"observed":values,"native":retired})).unwrap(),
        )
        .unwrap();
        generation
            .advance(VirtualDuration::from_ms(10))
            .await
            .unwrap();
        assert_eq!(generation.now().0, at + 10000);
    } else {
        outcome.unwrap();
        assert_eq!(at, 120000);
        let result = generation
            .eval_main("JSON.stringify(frameHost.contentWindow.serviceResults)")
            .await
            .unwrap();
        let row: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        std::fs::write(
            root.join("worker-child-live.json"),
            serde_json::to_vec_pretty(&row).unwrap(),
        )
        .unwrap();
        for id in ["left", "right"] {
            let calls = row[id]["callbacks"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["time"], json!(14));
            assert_eq!(calls[1]["time"], json!(64));
            assert_eq!(calls[1]["before"], json!(epoch + 84));
        }
        assert_eq!(row["left"]["callbacks"][0]["before"], json!(epoch + 34));
        assert_eq!(row["left"]["callbacks"][0]["after"], json!(epoch + 74));
        let shot = generation.screenshot().await.unwrap();
        assert!(shot.current);
        assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    }
    generation.stop().await.unwrap();
}
#[tokio::test]
async fn a_child_document_worker_frame_publishes_into_the_root_surface() {
    child_worker_frame_with_optional_retirement(false).await;
}
#[tokio::test]
async fn retiring_a_child_document_refuses_its_unfinished_worker_frame() {
    child_worker_frame_with_optional_retirement(true).await;
}
const RELAY_FRAME_WORKER: &str = r#"
onmessage=({data})=>{
  const child=new Worker(URL.createObjectURL(new Blob([data.childSource],{type:'text/javascript'})));
  globalThis.ownedChild=child;
  child.onmessage=e=>postMessage(e.data);
  child.postMessage({canvas:data.canvas,color:data.color,synchronous:data.synchronous},[data.canvas]);
};
"#;

#[tokio::test]
async fn the_pinned_browser_refuses_animation_frames_in_a_nested_worker() {
    use cdp::protocol::{runtime, target};
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let repo = support::repo_root();
    let pin = browser_pin::load_pin(&repo).unwrap();
    let binary =
        browser_pin::install(&repo, &pin, browser_pin::current_platform().unwrap()).unwrap();
    let browser = browser_pin::launch(&binary, &[], std::time::Duration::from_secs(20)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let page = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap()
        .target_infos
        .into_iter()
        .find(|t| t.r#type == "page")
        .unwrap();
    let session = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(page.target_id)
            },
        )
        .await
        .unwrap()
        .session_id;
    let probe=serde_json::to_string(r#"onmessage=({data:{canvas}})=>{const context=canvas.getContext('2d');try{const handle=requestAnimationFrame(()=>{context.fillStyle='#ff0000';context.fillRect(0,0,64,64);postMessage({kind:'callback'});});postMessage({kind:'accepted',handle});}catch(error){postMessage({kind:'refused',name:error.name,message:error.message});}};"#).unwrap();
    let relay = serde_json::to_string(RELAY_FRAME_WORKER).unwrap();
    let expression = format!(
        r#"new Promise(resolve=>{{const results={{direct:[],nested:[]}};const check=()=>{{if(results.direct.some(x=>x.kind==='callback'||x.kind==='refused')&&results.nested.some(x=>x.kind==='callback'||x.kind==='refused'))resolve(results);}};window.nativeWorkers=[];for(const name of ['direct','nested']){{const canvas=document.createElement('canvas');canvas.width=64;canvas.height=64;document.body.append(canvas);const worker=new Worker(URL.createObjectURL(new Blob([name==='direct'?{probe}:{relay}],{{type:'text/javascript'}})));nativeWorkers.push(worker);worker.onmessage=e=>{{results[name].push(e.data);check();}};worker.onerror=e=>{{results[name].push({{kind:'unexpectedError',message:e.message}});resolve(results);}};const transfer=canvas.transferControlToOffscreen();worker.postMessage({{canvas:transfer,color:'#ff0000',synchronous:false,childSource:{probe}}},[transfer]);}}}})"#
    );
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.call(
            Some(&session.0),
            &runtime::EvaluateParams {
                await_promise: Some(true),
                return_by_value: Some(true),
                ..runtime::EvaluateParams::new(expression)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.exception_details.is_none(), "{reply:?}");
    let value = reply.result.value.unwrap();
    std::fs::write(root.join("native-nested-worker-frame-support.json"),serde_json::to_vec_pretty(&json!({"binary":binary,"result":value,"scope":"Ordinary pinned browser; identical direct/nested capability probe with transferred canvas. No custom frame control or time qualification."})).unwrap()).unwrap();
    assert_eq!(value["direct"][0]["kind"], json!("accepted"));
    assert_eq!(value["direct"][1]["kind"], json!("callback"));
    assert_eq!(value["nested"].as_array().unwrap().len(), 1);
    assert_eq!(value["nested"][0]["kind"], json!("refused"));
    assert_eq!(value["nested"][0]["name"], json!("NotSupportedError"));
    client.close();
    drop(browser);
}
#[tokio::test]
async fn a_nested_worker_wait_does_not_extend_its_parent_workers_frame() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[{"id":"outside-frame","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}}]"#).unwrap();
    let options = support::options(
        &root,
        "worker-nested-outside-frame",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let timer = r#"setTimeout(()=>{const before=Date.now();const request=new XMLHttpRequest();request.open('GET','https://api.example.test/frame-service',false);request.send();postMessage({before,after:Date.now(),body:request.responseText});},6);"#;
    let prefix = format!(
        "globalThis.nestedTimer=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));nestedTimer.onmessage=e=>postMessage({{outsideFrame:e.data}});\n",
        serde_json::to_string(timer).unwrap()
    );
    let ordinary = serde_json::to_string(include_str!("fixtures/worker_frames.js")).unwrap();
    let prefix = serde_json::to_string(&prefix).unwrap();
    generation.eval_main(&format!(r#"(()=>{{window.outsideFrame=null;window.frameResults={{}};window.workerUrls={{}};window.frameWorkers=[];for(const [id,color] of [['left','#ff0000'],['right','#00ff00']]){{const script=(id==='left'?{prefix}:'')+{ordinary};const url=URL.createObjectURL(new Blob([script],{{type:'text/javascript'}}));workerUrls[id]=url;const worker=new Worker(url);frameWorkers.push(worker);worker.onmessage=e=>{{if(e.data.outsideFrame)outsideFrame=e.data.outsideFrame;else frameResults[id]=e.data;}};const canvas=document.getElementById(id).transferControlToOffscreen();worker.postMessage({{canvas,color}},[canvas]);}}return true;}})()"#)).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(0))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 10000);
    let first = generation.advance(VirtualDuration::from_ms(7)).await;
    std::fs::write(
        root.join("worker-nested-outside-boundary.json"),
        serde_json::to_vec_pretty(
            &json!({"outcome":format!("{first:?}"),"time_us":generation.now().0}),
        )
        .unwrap(),
    )
    .unwrap();
    first.unwrap();
    assert_eq!(generation.now().0, 17000);
    let native = native_child_worker_ownership(&generation).await;
    std::fs::write(
        root.join("worker-nested-outside-native.json"),
        serde_json::to_vec_pretty(&native).unwrap(),
    )
    .unwrap();
    let urls: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(workerUrls)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let workers = native["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 3);
    let parent = workers
        .iter()
        .find(|w| w["scriptUrl"] == urls["left"])
        .unwrap();
    assert_eq!(parent["creatorIsWorker"], json!(false));
    assert_eq!(
        parent["creatorId"],
        native["tree"]["frame"]["frontcapDocumentId"]
    );
    let descendants = workers
        .iter()
        .filter(|w| w["creatorIsWorker"] == json!(true))
        .collect::<Vec<_>>();
    assert_eq!(descendants.len(), 1);
    let child = descendants[0];
    assert_eq!(child["creatorId"], parent["targetId"]);
    assert!(child["synchronousWaitId"].as_str().is_some());
    assert!(child["frameOpportunity"].is_null());
    let target = native["workerTargets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["targetId"] == child["targetId"])
        .unwrap();
    assert_eq!(target["parentId"], parent["targetId"]);
    assert_eq!(
        generation.eval_main("outsideFrame").await.unwrap(),
        Value::Null
    );
    let before = generation.frame_requests();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, 17000);
    assert_eq!(generation.frame_requests(), before);
    assert_eq!(
        generation.eval_main("outsideFrame").await.unwrap(),
        Value::Null
    );
    assert_eq!(native_child_worker_ownership(&generation).await, native);
    generation
        .advance(VirtualDuration::from_ms(43))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 60000);
    assert_eq!(
        generation.eval_main("outsideFrame").await.unwrap(),
        json!({"before":epoch+16,"after":epoch+56,"body":"served"})
    );
    let result = generation
        .eval_main("JSON.stringify(frameResults)")
        .await
        .unwrap();
    let row: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
    std::fs::write(
        root.join("worker-nested-outside-result.json"),
        serde_json::to_vec_pretty(&row).unwrap(),
    )
    .unwrap();
    for id in ["left", "right"] {
        assert_eq!(row[id]["first"]["date"], json!(epoch + 17));
        assert_eq!(row[id]["next"]["date"], json!(epoch + 34));
    }
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    generation.stop().await.unwrap();
}
mod delayed_native_trace {
    use cdp::protocol::{emulation, io, system_info, target, tracing};
    use frontcap_chromium::generation::Generation;
    use serde_json::{Value, json};
    use std::{collections::BTreeSet, path::Path, time::Duration};
    type ProbeResult<T> = Result<T, String>;
    macro_rules! ensure { ($condition:expr,$($message:tt)*) => {if !$condition {return Err(format!($($message)*));}}; }
    mod trace_config {
        // Pinned Perfetto TraceConfig fields; independently decoded with the checkout's
        // existing protoc. This fixture runs on Linux, whose native primary clock is 3.
        pub fn encode(browser: u32, renderers: &[u32]) -> Vec<u8> {
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
            for pid in std::iter::once(browser).chain(renderers.iter().copied()) {
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
                self.received_bytes = self
                    .received_bytes
                    .checked_add(data.len())
                    .ok_or("trace byte count overflow")?;
                self.eof_received = eof;
                if base64_encoded {
                    return Err("unknown trace stream encoding".into());
                }
                let retained = data.len().min(self.limit - self.confirmed_written_bytes);
                output
                    .write_all(&data.as_bytes()[..retained])
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

    fn process_producers(roster: &Value) -> ProbeResult<(u32, Vec<u32>)> {
        let rows = roster["processInfo"]
            .as_array()
            .ok_or("missing native process roster")?;
        let one = |kind: &str| -> ProbeResult<u32> {
            let selected: Vec<_> = rows.iter().filter(|row| row["type"] == kind).collect();
            ensure!(
                selected.len() == 1,
                "expected one native {kind} process: {selected:?}"
            );
            let pid = selected[0]["id"]
                .as_u64()
                .ok_or("process id is not a positive integer")?;
            ensure!(pid > 0, "process id is zero");
            u32::try_from(pid).map_err(|_| "process id exceeds the native producer range".into())
        };
        let mut renderers = rows
            .iter()
            .filter(|row| row["type"] == "renderer")
            .map(|row| {
                row["id"]
                    .as_u64()
                    .filter(|pid| *pid > 0)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .ok_or_else(|| "invalid renderer PID".to_string())
            })
            .collect::<ProbeResult<Vec<_>>>()?;
        renderers.sort_unstable();
        renderers.dedup();
        let pair = (one("browser")?, renderers);
        ensure!(
            !pair.1.is_empty() && !pair.1.contains(&pair.0),
            "missing or invalid renderer processes"
        );
        Ok(pair)
    }

    fn trace_producers(
        trace: &Value,
        worker: &str,
        url: &str,
        producers: (u32, Vec<u32>),
        marker_ids: &[&str],
    ) -> ProbeResult<()> {
        let events = trace_events(trace)?;
        let worker = token(worker)?;
        let mut mappings = 0;
        let mut markers = 0;
        for event in events {
            if event["name"] == "TracingSessionIdForWorker" {
                let data = &event["args"]["data"];
                let id = data["workerId"]
                    .as_str()
                    .ok_or("worker mapping lacks target identity")?;
                if token(id)? == worker {
                    ensure!(
                        data["url"] == url
                            && event["pid"].as_u64().is_some_and(|pid| producers
                                .1
                                .iter()
                                .any(|p| u64::from(*p) == pid)),
                        "native worker trace mapping does not belong to the selected renderer"
                    );
                    mappings += 1;
                }
            }
            if event["name"] == "clock_sync"
                && event["args"]["sync_id"]
                    .as_str()
                    .is_some_and(|id| marker_ids.contains(&id))
            {
                ensure!(
                    event["pid"].as_u64() == Some(u64::from(producers.0)),
                    "clock-sync marker does not belong to the selected browser"
                );
                markers += 1;
            }
        }
        ensure!(
            mappings > 0 && markers > 0,
            "selected producers lack worker/clock-sync evidence"
        );
        Ok(())
    }

    pub struct Observation {
        client: cdp::Client,
        root: String,
        tracing: bool,
        stream: Option<io::StreamHandle>,
        complete: cdp::Subscription<tracing::TracingCompleteEvent>,
    }

    impl Observation {
        pub async fn connect(g: &Generation) -> ProbeResult<Self> {
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

        pub async fn mark(&self, id: String, epoch: u64, ms: u64) -> ProbeResult<Value> {
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
            let result = self
                .client
                .call(None, &system_info::GetProcessInfoParams::default())
                .await;
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

        pub async fn start_trace(
            &mut self,
            directory: &Path,
            label: &str,
            expected: Option<(u32, Vec<u32>)>,
        ) -> ProbeResult<(u32, Vec<u32>)> {
            ensure!(
                cfg!(target_os = "linux"),
                "producer-filter trace config is pinned to Linux"
            );
            let roster = self
                .process_roster(&directory.join(format!("{label}-producers-before.json")))
                .await?;
            let producers = process_producers(&roster)?;
            ensure!(
                expected.is_none_or(|expected| expected == producers),
                "native process identities changed between measurement and inspection"
            );
            let config = trace_config::encode(producers.0, &producers.1);
            std::fs::write(directory.join(format!("{label}-trace-config.pb")), &config)
                .map_err(|e| e.to_string())?;
            let encoded = frontcap_core::encode::base64(&config);
            save(
                &directory.join(format!("{label}-producer-selection.json")),
                &json!({
                    "browser_pid":producers.0,"renderer_pids":producers.1,
                    "track_event_producers":std::iter::once(producers.0).chain(producers.1.iter().copied()).map(|pid|format!("org.chromium-{pid}")).collect::<Vec<_>>(),
                    "metadata_producers_filtered":false,"perfetto_config_base64":encoded,
                    "main_buffer_kib":8192,"metadata_buffer_kib":256,"stream_byte_limit":trace_collection::TRACE_BYTE_LIMIT,
                    "primary_trace_clock":"BUILTIN_CLOCK_MONOTONIC",
                    "binding":"SystemInfo process.Pid -> tracing client PID -> org.chromium-<PID>; pinned source review"
                }),
            )?;
            let params: tracing::StartParams = serde_json::from_value(json!({
                "transferMode":"ReturnAsStream", "streamFormat":"json", "streamCompression":"none",
                "perfettoConfig":encoded
            }))
            .map_err(|e| e.to_string())?;
            self.client
                .call(None, &params)
                .await
                .map_err(|e| e.to_string())?;
            self.tracing = true;
            Ok(producers)
        }

        pub async fn check_producers(
            &self,
            directory: &Path,
            label: &str,
            expected: (u32, Vec<u32>),
        ) -> ProbeResult<(u32, Vec<u32>)> {
            let roster = self
                .process_roster(&directory.join(format!("{label}-producers-after.json")))
                .await?;
            let actual = process_producers(&roster)?;
            ensure!(
                actual.0 == expected.0,
                "native browser producer changed during {label}: {expected:?} -> {actual:?}"
            );
            Ok(actual)
        }

        pub async fn finish_trace(&mut self, path: &Path) -> ProbeResult<Value> {
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
            let mut collection =
                trace_collection::Collection::new(trace_collection::TRACE_BYTE_LIMIT);
            let report = |collection: &trace_collection::Collection,
                          outcome: &str,
                          read_error: Option<&str>,
                          completion_error: Option<&str>,
                          close_error: Option<&str>|
             -> ProbeResult<()> {
                save(
                    &report_path,
                    &json!({
                        "state":outcome,"trace_byte_limit":trace_collection::TRACE_BYTE_LIMIT,
                        "received_bytes":collection.received_bytes,
                        "confirmed_written_bytes":collection.confirmed_written_bytes,
                        "retained_file_bytes":std::fs::metadata(&partial).ok().map(|m| m.len()),
                        "chunks":collection.chunks,"eof_received":collection.eof_received,
                        "truncated":collection.truncated,"data_loss_occurred":complete.data_loss_occurred,
                        "raw_partial_path":partial,"complete_trace_path":path,"read_error":read_error,
                        "completion_error":completion_error,"close_error":close_error,
                        "callback_or_pixel_qualification":false
                    }),
                )
            };
            report(&collection, "collecting", None, None, None)?;
            let read_result: ProbeResult<()> = async {
                loop {
                    let chunk = self
                        .client
                        .call(
                            None,
                            &io::ReadParams {
                                size: Some(65_536),
                                ..io::ReadParams::new(handle.clone())
                            },
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    let appended = collection.append(
                        &mut output,
                        &chunk.data,
                        chunk.base64_encoded.unwrap_or(false),
                        chunk.eof,
                    );
                    report(
                        &collection,
                        "collecting",
                        appended.as_ref().err().map(String::as_str),
                        None,
                        None,
                    )?;
                    appended?;
                    if chunk.eof {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            drop(output);
            let completion_result = collection.validate_completion(
                complete.data_loss_occurred,
                serde_json::to_value(complete.trace_format).map_err(|e| e.to_string())?
                    == json!("json"),
                serde_json::to_value(complete.stream_compression).map_err(|e| e.to_string())?
                    == json!("none"),
            );
            // Attempt closing the owned stream even after malformed/oversized input;
            // retain both the original collection error and any close error.
            let close_result = self
                .client
                .call(None, &io::CloseParams::new(handle))
                .await
                .map_err(|e| e.to_string());
            if close_result.is_ok() {
                self.stream = None;
            }
            report(
                &collection,
                "collected",
                read_result.as_ref().err().map(String::as_str),
                completion_result.as_ref().err().map(String::as_str),
                close_result.as_ref().err().map(String::as_str),
            )?;
            read_result?;
            completion_result?;
            close_result?;
            let trace: Value = match std::fs::File::open(&partial)
                .map_err(|e| e.to_string())
                .and_then(|file| {
                    serde_json::from_reader(file).map_err(|e| format!("incomplete trace JSON: {e}"))
                }) {
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
    }
    impl Drop for Observation {
        fn drop(&mut self) {
            self.client.close();
        }
    }
    pub fn qualify(
        trace: &Value,
        inventory: &Value,
        producers: (u32, Vec<u32>),
        after: (u32, Vec<u32>),
        output: &Path,
    ) -> ProbeResult<()> {
        let events = trace_events(trace)?;
        let names = [
            "advance.begin",
            "advance.end",
            "held.begin",
            "held.end",
            "resume.begin",
            "resume.end",
        ];
        let times = names
            .iter()
            .map(|id| marker_time(events, id))
            .collect::<ProbeResult<Vec<_>>>()?;
        ensure!(
            times.windows(2).all(|pair| pair[0] < pair[1]),
            "native phase markers are not strictly ordered"
        );
        let workers = inventory["workers"]
            .as_array()
            .ok_or("missing native workers")?;
        ensure!(workers.len() == 2, "expected two enrolled workers");
        let mut rows = Vec::new();
        for worker in workers {
            let target = worker["targetId"].as_str().ok_or("missing worker target")?;
            let url = worker["scriptUrl"].as_str().ok_or("missing worker URL")?;
            trace_producers(trace, target, url, producers.clone(), &names)?;
            trace_producers(trace, target, url, after.clone(), &names)?;
            let callbacks = worker_invocations(trace, target, url)?;
            let entries: Vec<_> = callbacks
                .iter()
                .map(|call| {
                    json!({"id":call.id,"begin":call.at,"end":call.end,
   "starts_during_held_notify":call.at>=times[2] && call.at<=times[3],
   "starts_during_advance":call.at>=times[0] && call.at<=times[1],
   "starts_during_resume":call.at>=times[4] && call.at<=times[5]})
                })
                .collect();
            rows.push(json!({"worker":worker,"callbacks":entries}));
        }
        let result = json!({"phase_markers":names,"phase_native_times":times,"workers":rows,
  "no_worker_runtime_commands_during_trace":true});
        save(output, &result)?;
        for row in &rows {
            let callbacks = row["callbacks"].as_array().ok_or("missing callback rows")?;
            ensure!(
                callbacks.len() == 2,
                "native trace must positively observe both worker callbacks: {row}"
            );
            ensure!(
                callbacks
                    .iter()
                    .all(|c| c["starts_during_held_notify"] == false),
                "native worker callback executed while held after explicit notify: {row}"
            );
            ensure!(
                callbacks.iter().all(
                    |c| c["starts_during_advance"] == true || c["starts_during_resume"] == true
                ),
                "callback outside permitted phase: {row}"
            );
        }
        Ok(())
    }
}
#[tokio::test]
async fn native_trace_excludes_delayed_worker_callbacks_during_stopped_notify() {
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://frame.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":include_str!("fixtures/worker_frames.html")}},
        {"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}},
        {"id":"earlier","match":{"url":"https://frame.example.test/earlier"},"action":{"type":"mock","status":200,"body":"early","latency_ms":20}}
    ]).to_string()).unwrap();
    let options = support::options(
        &root,
        "worker-late-frame-wake",
        "https://frame.example.test/".into(),
        rules,
    );
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(
        generation
            .eval_main("crossOriginIsolated && typeof SharedArrayBuffer==='function'")
            .await
            .unwrap(),
        json!(true)
    );
    let left = serde_json::to_string(OWNED_FRAME_WORKER).unwrap();
    let right=serde_json::to_string(r#"onmessage=({data:{canvas,shared}})=>{const context=canvas.getContext('2d');globalThis.frameCalls=[];const report=()=>postMessage({callbacks:frameCalls});requestAnimationFrame(time=>{frameCalls.push({time,date:Date.now()});context.fillStyle='#00ff00';context.fillRect(0,0,64,64);report();requestAnimationFrame(time=>{frameCalls.push({time,date:Date.now()});report();});});postMessage({armedAt:Date.now()});const wake=Atomics.wait(new Int32Array(shared),0,0,250);postMessage({wake,wokeAt:Date.now()});};"#).unwrap();
    generation.eval_main(&format!(r#"(()=>{{window.delayedFrame={{left:null,right:null,armedAt:null,wake:null,wokeAt:null}};window.frameWords=new Int32Array(new SharedArrayBuffer(4));window.frameWorkers=[];for(const [id,source] of [['left',{left}],['right',{right}]]){{const worker=new Worker(URL.createObjectURL(new Blob([source],{{type:'text/javascript'}})));frameWorkers.push(worker);worker.onmessage=e=>{{if(id==='left')delayedFrame.left=e.data;else if(e.data.callbacks)delayedFrame.right=e.data;else Object.assign(delayedFrame,e.data);}};const canvas=document.getElementById(id).transferControlToOffscreen();worker.postMessage({{canvas,color:'#ff0000',synchronous:true,shared:frameWords.buffer}},[canvas]);}}fetch('/earlier');return true;}})()"#)).await.unwrap();
    let mut trace = delayed_native_trace::Observation::connect(&generation)
        .await
        .unwrap();
    let producers = trace
        .start_trace(&root, "delayed-wake", None)
        .await
        .unwrap();
    trace.mark("advance.begin".into(), epoch, 10).await.unwrap();
    let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
    let outcome = generation
        .next_event(ProgressEvent::Response, &arm, VirtualTime(120000))
        .await;
    std::fs::write(
        root.join("worker-delayed-frame-stop.json"),
        serde_json::to_vec_pretty(
            &json!({"outcome":format!("{outcome:?}"),"time_us":generation.now().0}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        outcome.unwrap(),
        ClockOutcome::Event {
            event: ProgressEvent::Response,
            ..
        }
    ));
    assert_eq!(generation.now().0, 30000);
    trace.mark("advance.end".into(), epoch, 30).await.unwrap();
    let before = generation
        .eval_main("JSON.stringify(delayedFrame)")
        .await
        .unwrap();
    let state: Value = serde_json::from_str(before.as_str().unwrap()).unwrap();
    assert_eq!(state["armedAt"], json!(epoch + 10));
    assert!(state["right"].is_null());
    assert!(state["wake"].is_null());
    let frames = generation.frame_requests();
    let native_before = native_child_worker_ownership(&generation).await;
    trace.mark("held.begin".into(), epoch, 30).await.unwrap();
    assert_eq!(
        generation
            .eval_main("Atomics.store(frameWords,0,1);Atomics.notify(frameWords,0,1)")
            .await
            .unwrap(),
        json!(1)
    );
    let immediate = generation
        .eval_main("JSON.stringify(delayedFrame)")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    trace.mark("held.end".into(), epoch, 30).await.unwrap();
    let held = generation
        .eval_main("JSON.stringify(delayedFrame)")
        .await
        .unwrap();
    let observed: Value = serde_json::from_str(held.as_str().unwrap()).unwrap();
    let native_after = native_child_worker_ownership(&generation).await;
    std::fs::write(root.join("worker-delayed-frame-held.json"),serde_json::to_vec_pretty(&json!({"before":state,"immediate":immediate,"held":observed,"nativeBefore":native_before,"nativeAfter":native_after,"framesBefore":frames,"framesAfter":generation.frame_requests(),"time_us":generation.now().0})).unwrap()).unwrap();
    assert_eq!(generation.now().0, 30000);
    assert_eq!(generation.frame_requests(), frames);
    assert_eq!(native_before["clock"], native_after["clock"]);
    assert!(
        observed["right"].is_null(),
        "an old frame ticket executed a delayed callback during a stopped wall hold: {observed}"
    );
    trace.mark("resume.begin".into(), epoch, 30).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(90))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120000);
    trace.mark("resume.end".into(), epoch, 120).await.unwrap();
    let producer_check = trace
        .check_producers(&root, "delayed-wake", producers.clone())
        .await;
    let native_trace = trace
        .finish_trace(&root.join("delayed-wake-trace.json"))
        .await
        .unwrap();
    let after_producers = producer_check.unwrap();
    let native_verdict = delayed_native_trace::qualify(
        &native_trace,
        &native_before,
        producers,
        after_producers,
        &root.join("delayed-wake-native-verdict.json"),
    );
    let final_state = generation
        .eval_main("JSON.stringify(delayedFrame)")
        .await
        .unwrap();
    let row: Value = serde_json::from_str(final_state.as_str().unwrap()).unwrap();
    std::fs::write(
        root.join("worker-delayed-frame-resumed.json"),
        serde_json::to_vec_pretty(&row).unwrap(),
    )
    .unwrap();
    assert_eq!(row["wake"], json!("ok"));
    let right = row["right"]["callbacks"].as_array().unwrap();
    assert_eq!(right.len(), 2);
    assert_eq!(right[0]["time"], json!(7));
    assert!([json!(epoch + 30), json!(epoch + 57)].contains(&right[0]["date"]));
    assert_eq!(right[1]["time"], json!(57));
    assert_eq!(right[1]["date"], json!(epoch + 67));
    let left = row["left"]["callbacks"].as_array().unwrap();
    assert_eq!(left.len(), 2);
    assert_eq!(left[0]["before"], json!(epoch + 17));
    assert_eq!(left[0]["after"], json!(epoch + 57));
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    generation.stop().await.unwrap();
    native_verdict.unwrap();
}

async fn owned_worker_tickets_at(generation: &Generation, epoch: u64, at_us: u64) -> Value {
    use cdp::protocol::{emulation, target};
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap()
        .target_infos;
    let popups = targets
        .iter()
        .filter(|t| {
            t.r#type == "page"
                && t.opener_id
                    .as_ref()
                    .is_some_and(|id| id.0 == endpoint.target_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(popups.len(), 1);
    let mut observed = serde_json::Map::new();
    let mut identities = std::collections::BTreeSet::new();
    for (name, id) in [
        ("root", target::TargetID(endpoint.target_id.clone())),
        ("popup", popups[0].target_id.clone()),
    ] {
        let session = client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(id)
                },
            )
            .await
            .unwrap()
            .session_id;
        let before = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            before.unix_time_us.as_deref(),
            Some((epoch * 1000 + at_us).to_string().as_str())
        );
        let workers = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetWorkersParams::default(),
            )
            .await
            .unwrap()
            .workers;
        assert_eq!(workers.len(), 2);
        for worker in &workers {
            assert!(worker.thread_started && !worker.creator_is_worker);
            assert!(identities.insert(worker.target_id.clone()));
            if name == "root" {
                assert!(
                    worker.frame_opportunity.is_none(),
                    "root completed before popup service: {worker:?}"
                );
            }
        }
        if name == "popup" {
            assert!(workers.iter().any(|worker| {
                worker.synchronous_wait_id.is_some()
                    && worker
                        .frame_opportunity
                        .as_deref()
                        .and_then(|ticket| ticket.parse::<u64>().ok())
                        .is_some_and(|ticket| ticket > 0)
            }));
        }
        observed.insert(name.into(), serde_json::to_value(&workers).unwrap());
        let after = client
            .call(
                Some(&session.0),
                &emulation::FrontcapGetClockParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(after.unix_time_us, before.unix_time_us);
        client
            .call(
                None,
                &target::DetachFromTargetParams {
                    session_id: Some(session),
                    target_id: None,
                },
            )
            .await
            .unwrap();
    }
    Value::Object(observed)
}

async fn popup_worker_frame_stop(watch: bool) {
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent, SampleValue},
        ids::WatchName,
        predicate::{Locator, Predicate},
        profile::PopupPolicy,
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules = RuleSet::from_json(r#"[
      {"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}},
      {"id":"earlier-response","match":{"url":"https://api.example.test/earlier-response"},"action":{"type":"mock","status":200,"body":"earlier","headers":{"access-control-allow-origin":"*"},"latency_ms":10}}
    ]"#).unwrap();
    let url = format!("http://{addr}/worker_frames.html");
    let mut options = support::options(&root, "popup-worker-frame-stop", url.clone(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    generation
        .eval_main(&format!(
            "(()=>{{window.framePopup=window.open({},'worker-popup');return !!framePopup;}})()",
            serde_json::to_string(&url).unwrap()
        ))
        .await
        .unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 20_000);
    generation.enter_context("popup-1").unwrap();
    start_owned_frame_workers(&generation, true).await;
    generation.leave_context();
    start_owned_frame_workers(&generation, false).await;
    generation.eval_main(&format!(r#"(()=>{{
      window.earlier=null;
      if({watch})setTimeout(()=>{{const ready=document.createElement('span');ready.id='frame-ready';ready.textContent='ready';document.body.append(ready);}},20);
      else setTimeout(()=>fetch('https://api.example.test/earlier-response').then(r=>r.text()).then(body=>earlier={{body,at:Date.now()}}),10);
      return true;
    }})()"#)).await.unwrap();
    let before = generation.frame_requests();
    let watch_name = WatchName::new("frame-ready").unwrap();
    let result = if watch {
        assert_eq!(
            generation
                .watch_add(
                    watch_name.clone(),
                    Predicate::Attached {
                        target: Locator::Css {
                            selector: "#frame-ready".into()
                        }
                    }
                )
                .await
                .unwrap(),
            SampleValue::False
        );
        generation
            .until(&watch_name, VirtualDuration::from_ms(100))
            .await
    } else {
        let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
        generation
            .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
            .await
    };
    std::fs::write(root.join("popup-worker-stop-result.json"), serde_json::to_vec_pretty(&json!({"watch":watch,"result":format!("{result:?}"),"at_us":generation.now().0,"requests":generation.frame_requests(),"before_requests":before})).unwrap()).unwrap();
    let outcome = result.unwrap();
    if watch {
        assert_eq!(
            outcome,
            ClockOutcome::ConditionMet {
                watch: watch_name.clone()
            }
        );
    } else {
        assert!(
            matches!(outcome, ClockOutcome::Event { event: ProgressEvent::Response, ref triggers } if !triggers.is_empty()),
            "{outcome:?}"
        );
    }
    assert_eq!(generation.now().0, 40_000);
    assert_eq!(
        generation.frame_requests(),
        before + 2,
        "one root and one original popup command"
    );
    let owned = owned_worker_tickets_at(&generation, epoch, 40_000).await;
    let stopped = generation.eval_main("JSON.stringify({serviceResults,earlier,ready:!!document.getElementById('frame-ready')})").await.unwrap();
    std::fs::write(
        root.join("popup-worker-stopped.json"),
        serde_json::to_vec_pretty(&json!({"watch":watch,"owned":owned,"parent":stopped})).unwrap(),
    )
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(generation.now().0, 40_000);
    assert_eq!(generation.frame_requests(), before + 2);
    assert_eq!(
        owned_worker_tickets_at(&generation, epoch, 40_000).await,
        owned
    );
    assert_eq!(generation.eval_main("JSON.stringify({serviceResults,earlier,ready:!!document.getElementById('frame-ready')})").await.unwrap(), stopped);
    if watch {
        assert_eq!(
            generation.watch_remove(&watch_name).await.unwrap(),
            SampleValue::True
        );
    }
    generation
        .advance(VirtualDuration::from_ms(80))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120_000);
    let mut observed = serde_json::Map::new();
    for (owner, context) in [("root", None), ("popup", Some("popup-1"))] {
        if let Some(context) = context {
            generation.enter_context(context).unwrap();
        }
        let value: Value = serde_json::from_str(
            generation
                .eval_main("JSON.stringify(serviceResults)")
                .await
                .unwrap()
                .as_str()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join(format!("popup-worker-resumed-{owner}.json")),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        for id in ["left", "right"] {
            let rows = value[id]["callbacks"].as_array().unwrap();
            assert_eq!(rows.len(), 2, "{owner}/{id}: {value}");
            assert_eq!(rows[0]["time"], json!(14));
            assert_eq!(rows[1]["time"], json!(64));
            assert_eq!(rows[1]["before"], json!(epoch + 84));
            if owner == "popup" && id == "left" {
                assert_eq!(rows[0]["before"], json!(epoch + 34));
                assert_eq!(rows[0]["after"], json!(epoch + 74));
                assert_eq!(rows[0]["body"], json!("served"));
            } else if owner == "root" {
                assert_eq!(rows[0]["before"], json!(epoch + 34));
                assert_eq!(rows[0]["after"], rows[0]["before"]);
            } else {
                assert!(
                    [json!(epoch + 34), json!(epoch + 40), json!(epoch + 74)]
                        .contains(&rows[0]["before"])
                );
                assert_eq!(rows[0]["after"], rows[0]["before"]);
            }
        }
        let shot = generation.screenshot().await.unwrap();
        assert!(shot.current);
        assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
        observed.insert(
            owner.into(),
            json!({"callbacks":value,"pixels":pixels(&shot.path)}),
        );
        if context.is_some() {
            generation.leave_context();
        }
    }
    std::fs::write(
        root.join("popup-worker-resumed.json"),
        serde_json::to_vec_pretty(&json!({"watch":watch,"observed":observed})).unwrap(),
    )
    .unwrap();
    generation.stop().await.unwrap();
}

#[tokio::test]
async fn a_popup_worker_frame_resumes_after_an_earlier_root_response_stop() {
    popup_worker_frame_stop(false).await;
}

#[tokio::test]
async fn a_root_watch_stop_preserves_an_unfinished_popup_worker_frame() {
    popup_worker_frame_stop(true).await;
}

#[tokio::test]
async fn detached_worker_frame_demand_is_readable_but_cannot_dispatch_callbacks() {
    use cdp::protocol::{emulation, page, target};
    use frontcap_chromium::{generation::ReconnectOptions, session::Session};
    use frontcap_core::{ids::RunId, predicate::WatchRegistry};
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let detached = support::DetachedProfile::new();
    let mut options = support::options(
        &root,
        "detached-worker-demand",
        format!("http://{addr}/worker_frames.html"),
        RuleSet::default(),
    );
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let epoch = profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    assert_eq!(
        generation.eval_main("startWorkers()").await.unwrap(),
        json!(true)
    );
    generation
        .advance(VirtualDuration::from_ms(1))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 11_000);
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
    let native = Session::new(client, attached.session_id.0);
    let before = native
        .call(&emulation::FrontcapGetWorkersParams::default())
        .await
        .unwrap();
    assert_eq!(before.workers.len(), 2);
    assert!(before.workers.iter().all(|worker| worker.thread_started
        && worker.frame_opportunity.is_none()));
    assert!(
        native
            .call(&emulation::IsMainFrameWantedParams::default())
            .await
            .unwrap()
            .wanted
    );
    let launch_id = generation.launch_id().clone();
    let observations = generation.observation_state();
    generation.abandon();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let detached_clock = loop {
        let clock = native
            .call(&page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        if !clock.controller_attached {
            break clock;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "native controller did not detach"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(
        detached_clock.activated
            && !detached_clock.failed
            && !detached_clock.suspension_id.is_empty()
    );
    assert_eq!(
        detached_clock.unix_time_us,
        (epoch * 1000 + 11_000).to_string()
    );
    let demand = native
        .call(&emulation::IsMainFrameWantedParams::default())
        .await;
    std::fs::write(
        root.join("detached-worker-demand.json"),
        serde_json::to_vec_pretty(
            &json!({"clock":detached_clock,"workers":before.workers,"query":format!("{demand:?}")}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(demand.unwrap().wanted);
    let refused = native.call(&emulation::RunMainFrameParams::default()).await;
    std::fs::write(
        root.join("detached-worker-dispatch.json"),
        serde_json::to_vec_pretty(&json!({"result":format!("{refused:?}")})).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(refused, Err(frontcap_chromium::ChromiumError::Cdp(cdp::CdpError::Protocol { ref message, .. })) if message == "Owned worker frame did not settle"),
        "{refused:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let after = native
        .call(&emulation::FrontcapGetWorkersParams::default())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after.workers).unwrap(),
        serde_json::to_value(before.workers).unwrap()
    );
    let held = native
        .call(&page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    assert!(!held.controller_attached && !held.failed);
    assert_eq!(held.unix_time_us, detached_clock.unix_time_us);
    assert_eq!(held.suspension_id, detached_clock.suspension_id);
    assert!(
        native
            .call(&emulation::IsMainFrameWantedParams::default())
            .await
            .unwrap()
            .wanted
    );
    let (mut generation, recovered) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: root.clone(),
        run: RunId::new("detached-worker-demand").unwrap(),
        launch_id,
        profile,
        endpoint,
        watches: WatchRegistry::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    })
    .await
    .unwrap();
    assert!(recovered.affected.is_empty());
    generation.restore_observations(observations);
    assert_eq!(generation.now().0, 11_000);
    assert_eq!(
        generation
            .eval_main("JSON.stringify(results)")
            .await
            .unwrap(),
        json!("{}")
    );
    generation
        .advance(VirtualDuration::from_ms(49))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 60_000);
    let results: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(results)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for id in ["left", "right"] {
        assert_eq!(results[id]["first"]["date"], json!(epoch + 17));
        assert_eq!(results[id]["next"]["date"], json!(epoch + 34));
    }
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    std::fs::write(
        root.join("detached-worker-resumed.json"),
        serde_json::to_vec_pretty(
            &json!({"results":results,"pixels":pixels(&shot.path),"at_us":generation.now().0}),
        )
        .unwrap(),
    )
    .unwrap();
    native.client.close();
    generation.stop().await.unwrap();
}

#[tokio::test]
async fn a_stopped_worker_frame_recovers_its_original_opportunity_after_controller_loss() {
    use cdp::protocol::{emulation, page, target};
    use frontcap_chromium::{generation::ReconnectOptions, session::Session};
    use frontcap_core::{
        event::{ClockOutcome, ProgressEvent},
        ids::RunId,
        predicate::WatchRegistry,
        time::VirtualTime,
    };
    assert!(support::controlled_engine_or_check_baseline().await);
    let addr = support::serve();
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("worker frame product evidence: {}", root.display());
    let rules=RuleSet::from_json(r#"[
      {"id":"frame-service","match":{"url":"https://api.example.test/frame-service"},"action":{"type":"mock","status":200,"body":"served","headers":{"access-control-allow-origin":"*"},"latency_ms":40}},
      {"id":"earlier-response","match":{"url":"https://api.example.test/earlier-response"},"action":{"type":"mock","status":200,"body":"earlier","headers":{"access-control-allow-origin":"*"},"latency_ms":10}}
    ]"#).unwrap();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(
        &root,
        "worker-frame-controller-loss",
        format!("http://{addr}/worker_frames.html"),
        rules,
    );
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let epoch = profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation
        .advance(VirtualDuration::from_ms(10))
        .await
        .unwrap();
    let source = serde_json::to_string(include_str!("fixtures/worker_frame_service.js")).unwrap();
    generation.eval_main(&format!(r#"(()=>{{
      window.serviceResults={{}};window.serviceWorkers=[];window.earlier=null;
      for(const [id,color,synchronous] of [['left','#ff0000',true],['right','#00ff00',false]]){{
        const worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));
        serviceWorkers.push(worker);worker.onmessage=e=>serviceResults[id]=e.data;
        const canvas=document.getElementById(id).transferControlToOffscreen();
        worker.postMessage({{canvas,color,synchronous}},[canvas]);
      }}
      setTimeout(()=>fetch('https://api.example.test/earlier-response').then(r=>r.text()).then(body=>earlier={{body,at:Date.now()}}),10);
      return true;
    }})()"#)).await.unwrap();
    let arm = generation.arm_event(ProgressEvent::Response).await.unwrap();
    let stopped = generation
        .next_event(ProgressEvent::Response, &arm, VirtualTime(120_000))
        .await
        .unwrap();
    assert!(matches!(
        stopped,
        ClockOutcome::Event {
            event: ProgressEvent::Response,
            ..
        }
    ));
    assert_eq!(generation.now().0, 30_000);
    let before = generation
        .eval_main("JSON.stringify({serviceResults,earlier})")
        .await
        .unwrap();
    let observed: Value = serde_json::from_str(before.as_str().unwrap()).unwrap();
    assert!(observed["serviceResults"]["left"].is_null());
    assert_eq!(observed["earlier"], json!({"body":"earlier","at":epoch+30}));
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
    let native = Session::new(client, attached.session_id.0);
    let workers = native
        .call(&emulation::FrontcapGetWorkersParams::default())
        .await
        .unwrap();
    assert_eq!(workers.workers.len(), 2);
    assert_eq!(
        workers
            .workers
            .iter()
            .filter(|w| w.synchronous_wait_id.is_some() && w.frame_opportunity.is_some())
            .count(),
        1
    );
    let original = serde_json::to_value(&workers.workers).unwrap();
    let launch_id = generation.launch_id().clone();
    let observations = generation.observation_state();
    generation.abandon();
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let detached_clock = loop {
        let clock = native
            .call(&page::FrontcapGetSharedClockParams::default())
            .await
            .unwrap();
        if !clock.controller_attached {
            break clock;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "controller did not detach"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(
        detached_clock.activated
            && !detached_clock.failed
            && !detached_clock.suspension_id.is_empty()
    );
    assert_eq!(
        detached_clock.unix_time_us,
        (epoch * 1000 + 30_000).to_string()
    );
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let held = native
        .call(&page::FrontcapGetSharedClockParams::default())
        .await
        .unwrap();
    let retained = native
        .call(&emulation::FrontcapGetWorkersParams::default())
        .await
        .unwrap();
    std::fs::write(
        root.join("worker-frame-controller-absent.json"),
        serde_json::to_vec_pretty(
            &json!({"clock":held,"workers":retained.workers,"original_workers":original}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!held.controller_attached && !held.failed);
    assert_eq!(held.unix_time_us, detached_clock.unix_time_us);
    assert_eq!(serde_json::to_value(&retained.workers).unwrap(), original);
    let recovery = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Generation::reconnect(ReconnectOptions {
            repo_root: support::repo_root(),
            runs_root: root.clone(),
            run: RunId::new("worker-frame-controller-loss").unwrap(),
            launch_id,
            profile,
            endpoint,
            watches: WatchRegistry::default(),
            watchdog: std::time::Duration::from_secs(10),
            granted_until: None,
            dialog_rules: Default::default(),
            upstream: None,
        }),
    )
    .await;
    let outcome = match &recovery {
        Ok(Ok((g, r))) => json!({"ok":true,"at_us":g.now().0,"affected":r.affected}),
        Ok(Err(e)) => json!({"error":format!("{e:?}")}),
        Err(e) => json!({"timeout":e.to_string()}),
    };
    std::fs::write(
        root.join("worker-frame-controller-recovery.json"),
        serde_json::to_vec_pretty(&outcome).unwrap(),
    )
    .unwrap();
    let (mut generation, recovered) = recovery.unwrap().unwrap();
    assert!(recovered.affected.is_empty());
    generation.restore_observations(observations);
    assert_eq!(
        generation.now().0,
        30_000,
        "reattachment must retain the completed response stop"
    );
    assert_eq!(
        generation
            .eval_main("JSON.stringify({serviceResults,earlier})")
            .await
            .unwrap(),
        before
    );
    generation
        .advance(VirtualDuration::from_ms(90))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 120_000);
    let observed: Value = serde_json::from_str(
        generation
            .eval_main("JSON.stringify(serviceResults)")
            .await
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for id in ["left", "right"] {
        let rows = observed[id]["callbacks"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["time"], json!(7));
        assert_eq!(rows[1]["time"], json!(57));
        assert_eq!(rows[1]["before"], json!(epoch + 67));
    }
    assert_eq!(
        observed["left"]["callbacks"][0]["before"],
        json!(epoch + 17)
    );
    assert_eq!(observed["left"]["callbacks"][0]["after"], json!(epoch + 57));
    let shot = generation.screenshot().await.unwrap();
    assert!(shot.current);
    assert_eq!(pixels(&shot.path), [[255, 0, 0], [0, 255, 0]]);
    std::fs::write(
        root.join("worker-frame-controller-resumed.json"),
        serde_json::to_vec_pretty(&json!({"results":observed,"pixels":pixels(&shot.path)}))
            .unwrap(),
    )
    .unwrap();
    native.client.close();
    generation.stop().await.unwrap();
}
