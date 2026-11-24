//! Bucket expiration must follow each generation's controlled date.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

#[tokio::test]
async fn bucket_expiration_acceptance_and_boundary_follow_the_virtual_epoch() {
    for epoch in [0u64, 946684800000, 4102444800000] {
        let page = r#"<!doctype html><script>
        window.result={epoch:Date.now(),samples:[]};
        (async()=>{
          try {
            const b=await navigator.storageBuckets.open('expiring',{expires:Date.now()+100});
            result.expiration=await b.expires();
            await navigator.storageBuckets.open('persistent');
            for (const delay of [50,100,150]) setTimeout(async()=>{
              const keys=await navigator.storageBuckets.keys();
              result.samples.push({time:performance.now(),keys:keys.sort()});
            },delay);
            result.ready=true;
          } catch(e) {result.error={name:e.name,message:e.message};}
        })();</script>"#;
        let rules=RuleSet::from_json(&json!([{"id":"page","match":{"url":"https://bucket-clock.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]).to_string()).unwrap();
        let runs = tempfile::tempdir().unwrap();
        let mut options = support::options(
            runs.path(),
            "bucket-expiration",
            "https://bucket-clock.test/".into(),
            rules,
        );
        options.profile.initial_date_ms = epoch;
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(200)).await.unwrap();
        let result = g.eval_main("result").await.unwrap();
        let now = g.now();
        g.stop().await.unwrap();
        assert_eq!(
            result,
            json!({"epoch":epoch,"expiration":epoch+100,"ready":true,"samples":[
                {"time":50,"keys":["expiring","persistent"]},
                {"time":100,"keys":["persistent"]},
                {"time":150,"keys":["persistent"]}
            ]}),
            "epoch {epoch}"
        );
        assert_eq!(now.0, 200_000);
    }
}

#[tokio::test]
async fn bucket_and_cookie_expiration_survive_renderer_replacement_together() {
    use cdp::protocol::system_info;
    use frontcap_core::{action::Action, ids::RequestId};
    use std::collections::BTreeSet;
    let runs = tempfile::tempdir().unwrap();
    let start = "https://bucket-handoff.test/start";
    let back = "https://bucket-handoff.test/back";
    let first = r#"<script>window.ready=false;(async()=>{
      const b=await navigator.storageBuckets.open('expiring',{expires:Date.now()+2000});
      await(await b.caches.open('items')).put('/item',new Response('kept'));
      document.cookie='same=kept; Max-Age=2; Secure; Path=/';ready=true;
    })();setTimeout(()=>location.href='http://127.0.0.1:1/x',300)</script>"#;
    let returned = r#"<script>window.result={samples:[]};(async()=>{
      const b=await navigator.storageBuckets.open('expiring');
      result.body=await(await(await b.caches.open('items')).match('/item')).text();
      result.expires=await b.expires();
      for(const instant of [1999,2000]) setTimeout(async()=>{
        result.samples.push({keys:await navigator.storageBuckets.keys(),cookie:document.cookie,time:Date.now()-4102444800000});
      },instant-(Date.now()-4102444800000));
    })();</script>"#;
    let rules=RuleSet::from_json(&json!([
        {"id":"start","match":{"url":start},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":first}},
        {"id":"back","match":{"url":back},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":returned}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "bucket-handoff", start.into(), rules);
    options.passthrough_origins = Some(vec!["http://127.0.0.1:1".into()]);
    options.profile.initial_date_ms = 4102444800000;
    let mut g = Generation::launch(options).await.unwrap();
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url)
        .await
        .unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    assert_eq!(g.eval_main("ready").await.unwrap(), true);
    let before = client
        .call(None, &system_info::GetProcessInfoParams::default())
        .await
        .unwrap()
        .process_info
        .into_iter()
        .filter(|p| p.r#type == "renderer")
        .map(|p| p.id)
        .collect::<BTreeSet<_>>();
    g.advance(VirtualDuration::from_ms(990)).await.unwrap();
    let after = client
        .call(None, &system_info::GetProcessInfoParams::default())
        .await
        .unwrap()
        .process_info
        .into_iter()
        .filter(|p| p.r#type == "renderer")
        .map(|p| p.id)
        .collect::<BTreeSet<_>>();
    assert!(
        !after.is_subset(&before),
        "error-page navigation must replace the renderer"
    );
    g.navigate_action(
        &RequestId::new("return-to-bucket").unwrap(),
        &Action::Navigate { url: back.into() },
    )
    .await
    .unwrap();
    g.advance(VirtualDuration::from_ms(1001)).await.unwrap();
    let result = g.eval_main("result").await.unwrap();
    g.stop().await.unwrap();
    assert_eq!(
        result,
        json!({"body":"kept","expires":4102444802000u64,"samples":[
          {"keys":["expiring"],"cookie":"same=kept","time":1999},
          {"keys":[],"cookie":"","time":2000}
        ]})
    );
}

#[tokio::test]
async fn browser_contexts_keep_independent_bucket_clocks() {
    use cdp::protocol::{emulation, network, page, target};
    use frontcap_chromium::session::Session;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0; 4096];
                let _ = socket.read(&mut buf).await;
                let body = "<!doctype html><p>independent context</p>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let runs = tempfile::tempdir().unwrap();
    let url = "https://bucket-owner.test/";
    let first = r#"<script>window.result={};(async()=>{
      const b=await navigator.storageBuckets.open('first',{expires:Date.now()+1000});
      result.expires=await b.expires();
      for(const at of [100,1000])setTimeout(async()=>{const date=Date.now();result[at]={date,keys:await navigator.storageBuckets.keys()}},Math.ceil(at-performance.now()));
    })()</script>"#;
    let rules=RuleSet::from_json(&json!([{"id":"first","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":first}}]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "bucket-owners", url.into(), rules);
    options.profile.initial_date_ms = 4102444800000;
    options.passthrough_origins = Some(vec![origin.clone()]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    assert_eq!(
        g.eval_main("result.expires").await.unwrap(),
        json!(4102444801000u64)
    );
    let client = cdp::Client::connect(&g.endpoint().browser_ws_url)
        .await
        .unwrap();
    let context = client
        .call(
            None,
            &target::CreateBrowserContextParams {
                dispose_on_detach: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .browser_context_id;
    let created = client
        .call(
            None,
            &target::CreateTargetParams {
                browser_context_id: Some(context.clone()),
                ..target::CreateTargetParams::new("about:blank".into())
            },
        )
        .await
        .unwrap();
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(created.target_id)
            },
        )
        .await
        .unwrap();
    let independent = Session::new(client.clone(), attached.session_id.0);
    independent
        .call(&page::EnableParams::default())
        .await
        .unwrap();
    let mut loaded = independent.subscribe::<page::LoadEventFiredEvent>();
    independent
        .call(&page::NavigateParams::new(format!("{origin}/")))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), loaded.next())
        .await
        .unwrap()
        .unwrap();
    independent
        .call(&emulation::SetVirtualTimePolicyParams {
            initial_virtual_time: Some(network::TimeSinceEpoch(7258118400.0)),
            ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
        })
        .await
        .unwrap();
    independent.eval(r#"window.result={epoch:Date.now()};(async()=>{
      const b=await navigator.storageBuckets.open('second',{expires:Date.now()+1000});
      result.expires=await b.expires();
      setTimeout(async()=>{result.date=Date.now();result.keys=await navigator.storageBuckets.keys()},7258118401000-Date.now());
    })();true"#,None).await.unwrap();
    for budget in [10.0, 1090.0] {
        let mut ended = independent.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>();
        independent
            .call(&emulation::SetVirtualTimePolicyParams {
                budget: Some(budget),
                ..emulation::SetVirtualTimePolicyParams::new(
                    emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending,
                )
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), ended.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            independent.eval("result.expires", None).await.unwrap(),
            json!(7258118401000u64)
        );
    }
    let second = independent.eval("result", None).await.unwrap();
    assert_eq!(
        g.eval_main("Date.now()").await.unwrap(),
        json!(4102444800010u64)
    );
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let first = g.eval_main("result").await.unwrap();
    client
        .call(None, &target::DisposeBrowserContextParams::new(context))
        .await
        .unwrap();
    g.stop().await.unwrap();
    server.abort();
    assert_eq!(
        json!({"first":first,"second":second}),
        json!({
          "first":{"expires":4102444801000u64,"100":{"date":4102444800100u64,"keys":["first"]},"1000":{"date":4102444801000u64,"keys":[]}},
          "second":{"epoch":7258118400000u64,"expires":7258118401000u64,"keys":[],"date":7258118401000u64}
        })
    );
}
