//! Live recipes use current native storage across removed frames and partitions.
mod support;
use serde_json::json;

#[tokio::test]
async fn recipe_keeps_unvisited_partition_data_and_does_not_resurrect_deleted_frame_buckets() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://scope-recipe.test";
    let frame="https://frame-recipe.test";
    let partition=json!({"top_level_site":origin,"has_cross_site_ancestor":true});
    let top=format!(r#"<!doctype html><button id=change>Change</button><output id=status>idle</output><script>
      document.querySelector('#change').onclick=()=>{{let f=document.createElement('iframe');f.src='{frame}/change';document.body.append(f)}};
      addEventListener('message',e=>{{if(e.data==='done'){{document.querySelector('iframe').remove();document.querySelector('#status').textContent='done'}}}});
    </script>"#);
    let child=r#"<!doctype html><script>(async()=>{
      await navigator.storageBuckets.delete('stale');
      const b=await navigator.storageBuckets.open('current');
      await(await b.caches.open('present')).put('/entry',new Response('current body'));
      const db=await new Promise((ok,no)=>{const r=b.indexedDB.open('records',1);r.onupgradeneeded=()=>r.result.createObjectStore('items');r.onerror=()=>no(r.error);r.onsuccess=()=>ok(r.result)});
      await new Promise((ok,no)=>{const t=db.transaction('items','readwrite');t.objectStore('items').put({saved:true},'key');t.oncomplete=ok;t.onabort=()=>no(t.error)});db.close();
      parent.postMessage('done','*');
    })();</script>"#;
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"profile":{
      "initial_buckets":[
        {"origin":frame,"partition":partition,"name":"stale","quota":"1048576","persistent":true,"durability":"strict"},
        {"origin":"https://unvisited-recipe.test","name":"empty","quota":"2097152","persistent":true,"durability":"strict"}
      ],
      "initial_caches":[
        {"origin":frame,"partition":partition,"bucket":"stale","name":"deleted","entries":[{"request_url":format!("{frame}/old"),"status":200,"body":"stale"}]},
        {"origin":frame,"name":"first-party","entries":[{"request_url":format!("{frame}/kept"),"status":200,"body":"first party"}]}
      ],
      "initial_indexed_db":[{"origin":frame,"partition":partition,"bucket":"stale","name":"deleted","version":1,"stores":[{"name":"items","records":[{"key":{"type":"string","value":"old"},"value":"stale"}]}]}]
    },"rules":[
      {"id":"top","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":top}},
      {"id":"child","match":{"url":format!("{frame}/change")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child}}
    ]})).await;
    let id=&launched["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#change"}}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"done"},"max_ms":2000}})).await;
    let at=m.call_tool("status",json!({"launch_id":id})).await["t_us"].clone();
    let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":id})).await;
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],at);
    let described=m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let initial=&described["initial_state"];
    let buckets=initial["buckets"].as_array().unwrap();
    assert!(!buckets.iter().any(|b|b["name"]=="stale"),"{described}");
    assert!(buckets.iter().any(|b|b["origin"]=="https://unvisited-recipe.test" && b["name"]=="empty" && b["quota"]=="2097152" && b["persistent"]==true && b["durability"]=="strict"),"{described}");
    let caches=initial["caches"].as_array().unwrap();
    assert_eq!(caches.len(),2,"{described}");
    assert!(caches.iter().any(|c|c["origin"]==frame && c["name"]=="present" && c["bucket"]=="current" && c["partition"]==partition && c["entries"]==1),"{described}");
    assert!(caches.iter().any(|c|c["origin"]==frame && c["name"]=="first-party" && c["bucket"].is_null() && c["partition"]["top_level_site"]==frame && c["partition"]["has_cross_site_ancestor"]==false),"{described}");
    let databases=initial["indexed_db"].as_array().unwrap();
    assert_eq!(databases.len(),1,"{described}");
    assert_eq!(databases[0]["name"],"records");
    assert_eq!(databases[0]["bucket"],"current");
    assert_eq!(databases[0]["partition"],partition);
    let branch=m.call_tool("state",json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"],"request_id":"scoped-storage-child"})).await;
    assert_eq!(branch["phase"],"ready","{branch}");
    let child_id=&branch["launch_id"];
    // The branch's page has never opened the foreign frame: this second capture must
    // observe restored native contents, not reconstruction by page JavaScript.
    let captured=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":child_id})).await;
    let restored=m.call_tool("state",json!({"operation":"describe_saved","saved_id":captured["saved_id"]})).await;
    assert_eq!(restored["initial_state"]["buckets"],initial["buckets"]);
    assert_eq!(restored["initial_state"]["caches"],initial["caches"]);
    assert_eq!(restored["initial_state"]["indexed_db"],initial["indexed_db"]);
    m.call_tool("stop",json!({"launch_id":child_id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":child_id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn a_removed_frame_of_another_origin_keeps_its_storage_in_a_live_recipe() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://removed-top.test";
    let frame="https://removed-child.test";
    let top=format!(r#"<!doctype html><button id=seed>Seed</button><button id=probe>Probe</button><output id=status>idle</output><script>
      const open=path=>{{const f=document.createElement('iframe');f.src='{frame}/'+path;document.body.append(f)}};
      document.querySelector('#seed').onclick=()=>open('seed');
      document.querySelector('#probe').onclick=()=>open('probe');
      addEventListener('message',e=>{{if(e.origin!=='{frame}')return;document.querySelector('iframe').remove();document.querySelector('#status').textContent=e.data}});
    </script>"#);
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"rules":[
      {"id":"top","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":top}},
      {"id":"seed","match":{"url":format!("{frame}/seed")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
        "body":"<!doctype html><script>localStorage.setItem('kept','kept');sessionStorage.setItem('kept','kept');parent.postMessage('seeded','*')</script>"}},
      {"id":"probe","match":{"url":format!("{frame}/probe")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
        "body":"<!doctype html><script>parent.postMessage('probe:'+localStorage.getItem('kept')+':'+sessionStorage.getItem('kept'),'*')</script>"}}
    ]})).await;
    let id=&launched["launch_id"];
    let status=|text:&str|json!({"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":text},"max_ms":2000});
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#seed"}}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":status("seeded")})).await;
    let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":id})).await;
    let described=m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let storage=&described["initial_state"]["dom_storage"];
    assert!(storage["local"].as_array().unwrap().iter().any(|a|a["origin"]==frame && a["entries"]==1),"{described}");
    assert!(storage["sessions"][0]["areas"].as_array().unwrap().iter().any(|a|a["origin"]==frame && a["entries"]==1),"{described}");
    let branch=m.call_tool("state",json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"],"request_id":"removed-frame-child"})).await;
    assert_eq!(branch["phase"],"ready","{branch}");
    let child=&branch["launch_id"];
    // The child never ran the seeding frame: its first probe reads what the source's storage held.
    m.call_tool("act",json!({"launch_id":child,"action":{"type":"click","target":{"by":"css","selector":"#probe"}}})).await;
    m.call_tool("wait",json!({"launch_id":child,"op":status("probe:kept:kept")})).await;
    m.call_tool("stop",json!({"launch_id":child})).await;
    m.call_tool("stop",json!({"launch_id":id})).await;
}

#[tokio::test]
async fn a_closed_sign_in_window_stays_closed_in_a_live_recipe() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let origin="https://closed-sign-in.test";
    let top=r#"<!doctype html><button id=sign>Sign in</button><output id=status>signed out</output><script>
      document.querySelector('#sign').onclick=()=>open('/popup','sign-in');
      const show=()=>{document.querySelector('#status').textContent=localStorage.getItem('token')??'signed out'};
      addEventListener('storage',show);show();
    </script>"#;
    let popup="<!doctype html><script>sessionStorage.setItem('step','done');localStorage.setItem('token','granted');close()</script>";
    let launched=m.call_tool("launch",json!({"url":format!("{origin}/start"),"profile":{"popups":"controlled"},"rules":[
      {"id":"top","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":top}},
      {"id":"popup","match":{"url":format!("{origin}/popup")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":popup}}
    ]})).await;
    let id=&launched["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#sign"}}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":{"by":"css","selector":"#status"},"text":"granted"},"max_ms":2000}})).await;
    let contexts=m.call_tool("session",json!({"launch_id":id,"operation":"contexts"})).await;
    assert_eq!(contexts["contexts"][1]["open"],false,"{contexts}");
    let rules=m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    for rule in rules["rules"].as_array().unwrap() {
        assert_eq!(rule["default"]==true,rule["id"].as_str().unwrap().starts_with("default-"),"{rules}");
    }
    let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","from":id})).await;
    let described=m.call_tool("state",json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    assert_eq!(described["initial_state"]["contexts"],json!([{"context":"popup-1","opener":"page","url":"about:blank","name":"","no_opener":false,"closed":true}]),"{described}");
    let branch=m.call_tool("state",json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"],"request_id":"closed-sign-in-child"})).await;
    assert_eq!(branch["phase"],"ready","{branch}");
    let child=&branch["launch_id"];
    let listed=m.call_tool("session",json!({"operation":"list"})).await;
    let lineage:Vec<_>=listed["generations"].as_array().unwrap().iter().map(|g|(g["launch_id"].clone(),g.get("basis").cloned())).collect();
    assert!(lineage.contains(&(child.clone(),Some(saved["saved_id"].clone()))),"{listed}");
    assert!(lineage.contains(&(id.clone(),None)),"{listed}");
    let restored=m.call_tool("session",json!({"launch_id":child,"operation":"contexts"})).await;
    assert_eq!(restored["contexts"].as_array().unwrap().iter().map(|c|(c["id"].clone(),c["open"].clone())).collect::<Vec<_>>(),
        [(json!("page"),json!(true)),(json!("popup-1"),json!(false))],"{restored}");
    let text=m.call_tool("debug",json!({"launch_id":child,"operation":"evaluate","expression":"document.querySelector('#status').textContent"})).await;
    assert_eq!(text["value"],"granted","{text}");
    m.call_tool("stop",json!({"launch_id":child})).await;
    m.call_tool("stop",json!({"launch_id":id})).await;
}
