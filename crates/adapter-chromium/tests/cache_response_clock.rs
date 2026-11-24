//! Native persisted metadata must follow the controlled response-delivery instant.
mod support;
use cdp::protocol::{cache_storage, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};

async fn sample(origin: &str, remote: &str, seed: u64, epoch: u64) -> Value {
    let page = format!(r#"<!doctype html><output id=status>loading</output><script>
    (async()=>{{
      const responses=await Promise.all([
        fetch('/basic'),fetch('{remote}/cors',{{credentials:'omit'}}),
        fetch('{remote}/opaque',{{mode:'no-cors',credentials:'omit'}})
      ]);
      responses.push(new Response('synthetic'));
      window.createdAt=Date.now();
      window.visible=await Promise.all(responses.map(async r=>[r.type,r.status,await r.clone().text()]));
      const c=await caches.open('clock');
      for(let i=0;i<responses.length;i++)await c.put('/entry-'+i,responses[i]);
      document.querySelector('#status').textContent='ready';
    }})();</script>"#);
    let mut rules=vec![json!({"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}})];
    for (id,url) in [("basic",format!("{origin}/basic")),("cors",format!("{remote}/cors")),("opaque",format!("{remote}/opaque"))] {
        rules.push(json!({"id":id,"match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/plain","access-control-allow-origin":"*"},"body":id,"latency_ms":300}}));
    }
    let runs=tempfile::tempdir().unwrap();
    let mut options=support::options(runs.path(),"cache-clock",format!("{origin}/start"),RuleSet::from_json(&json!(rules).to_string()).unwrap());
    options.profile.random_seed=seed;options.profile.initial_date_ms=epoch;
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(350)).await.unwrap();
    assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(),"ready");
    assert_eq!(g.eval_main("createdAt").await.unwrap(),json!(epoch+300));
    assert_eq!(g.eval_main("visible").await.unwrap(),json!([["basic",200,"basic"],["cors",200,"cors"],["opaque",0,""],["default",200,"synthetic"]]));
    let endpoint=g.endpoint();let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap();
    let native=Session::new(client,attached.session_id.0);
    let caches=native.call(&cache_storage::RequestCacheNamesParams {security_origin:Some(origin.into()),storage_key:None,storage_bucket:None}).await.unwrap().caches;
    assert_eq!(caches.len(),1);
    let mut entries=Vec::new();
    for i in 0..4 {
        let response=native.call(&cache_storage::FrontcapReadEntryParams {cache_id:caches[0].cache_id.clone(),request_url:format!("{origin}/entry-{i}"),request_headers:vec![]}).await.unwrap().response.unwrap();
        entries.push(json!({"response_time":response.metadata.response_time,"padding":response.metadata.padding}));
    }
    assert_eq!(g.now().0,350_000);
    native.client.close();g.stop().await.unwrap();
    json!({"epoch":epoch,"seed":seed,"entries":entries})
}

#[tokio::test]
async fn response_metadata_repeats_across_runs_and_relocated_origins() {
    let epoch=946_684_800_000;
    let mut samples=Vec::new();
    for (origin,remote,seed,start) in [
        ("https://clock.test","https://resource.test",0,epoch),
        ("https://clock.test","https://resource.test",0,epoch),
        ("https://relocated.test","https://moved-resource.test",0,epoch),
        ("https://clock.test","https://resource.test",1,epoch),
    ] { samples.push(sample(origin,remote,seed,start).await); }
    println!("CACHE_METADATA_CLOCK={}",json!(samples));
    // Windows-epoch microseconds are the native metadata wire representation.
    for sample in &samples {
        let expected=((sample["epoch"].as_u64().unwrap()+300)*1000+11_644_473_600_000_000).to_string();
        for (i,entry) in sample["entries"].as_array().unwrap().iter().enumerate() {
            assert_eq!(entry["response_time"],expected,"entry {i}: {sample}");
            let padding=entry["padding"].as_str().unwrap().parse::<i64>().unwrap();
            assert!((0..14_431*1024).contains(&padding));
            if i!=2 {assert_eq!(padding,0);}
        }
    }
    assert_eq!(samples[0]["entries"],samples[1]["entries"]);
    assert_eq!(samples[0]["entries"],samples[2]["entries"]);
    // A different seed must not accidentally behave as an ignored seed.
    assert_ne!(samples[0]["entries"][2]["padding"],samples[3]["entries"][2]["padding"]);
}

#[tokio::test]
async fn zero_epoch_is_a_controlled_clock_not_a_missing_timestamp() {
    let sampled=sample("https://clock.test","https://resource.test",0,0).await;
    println!("CACHE_ZERO_EPOCH={sampled}");
    for entry in sampled["entries"].as_array().unwrap() {
        assert_eq!(entry["response_time"],"11644473600300000");
    }
}
