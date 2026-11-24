mod support;
use serde_json::{Value,json};

fn child(frame:&str,selector:&str)->Value {
    json!({"by":"frame","frame":{"by":"css","selector":format!("#{frame}")},
        "target":{"by":"css","selector":selector}})
}
fn rules(message:&str)->Value {
    json!([
        {"id":"feed","match":{"url":"https://api.example.test/feed"},
         "action":{"type":"stream_mock","messages":[{"at_ms":1000,"data":message}]}},
        {"id":"parent","match":{"url":"http://top.example.test/parent"},
         "action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":"<!doctype html><button id='replace' onclick=\"document.querySelector('#a').src='http://child.example.test/owner?replacement'\">Replace child</button><iframe id='same' src='/owner'></iframe><iframe id='a' src='http://child.example.test/owner?a'></iframe><iframe id='b' src='http://child.example.test/owner?b'></iframe>"}},
        {"id":"owners","match":{"url":"http://*/owner*"},
         "action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},
         "body":include_str!("fixtures/stream-owner.html")}}
    ])
}
async fn events(m:&mut support::Mcp,id:&Value,frame:&str)->Value {
    let read=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query",
        "target":child(frame,"#events"),"props":["text"]}})).await;
    serde_json::from_str(read["values"]["text"].as_str().unwrap()).unwrap()
}

async fn replace_recorder(m:&mut support::Mcp,ws:&support::Workspace,id:&Value) {
    let before=m.call_tool("status",json!({"launch_id":id})).await;
    let dir=ws.root().join("runtimes").join(id.as_str().unwrap());
    let runtime=frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    runtime.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,runtime.browser);
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],before["t_us"]);
}

async fn selected_rules(recover:bool) {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let launched=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
        "rules":rules("old"),"passthrough_origins":[]})).await;
    let id=&launched["launch_id"];
    for frame in ["same","a","b"] {
        assert_eq!(events(&mut m,id,frame).await,json!([[1,"open",null,0]]),"{frame}");
    }
    let before=m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    assert_eq!(before["boundary"]["t_us"],250_000);
    m.call_tool("network",json!({"launch_id":id,"operation":"set_rules","expected_revision":0,
        "boundary":before["boundary"],"rules":rules("new")})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#open")},
        "then":{"type":"none"}})).await;
    if recover { replace_recorder(&mut m,&ws,id).await; }
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1000}})).await;
    for frame in ["same","b"] {
        assert_eq!(events(&mut m,id,frame).await,json!([[1,"open",null,0],[1,"message","old",1000]]),"{frame}");
    }
    assert_eq!(events(&mut m,id,"a").await,json!([
        [1,"open",null,0],[2,"open",null,250],[1,"message","old",1000],[2,"message","new",1250]
    ]));
    m.call_tool("stop",json!({"launch_id":id})).await;
    if !recover {
        let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
}

#[tokio::test]
async fn sibling_streams_keep_their_owners_and_selected_rules_in_replay() { selected_rules(false).await; }

#[tokio::test]
async fn sibling_streams_restore_their_owners_and_selected_rules_after_recorder_loss() { selected_rules(true).await; }

#[tokio::test]
async fn closing_one_child_stream_preserves_siblings_in_replay_and_recovery() {
    for websocket in [false,true] {
    for recover in [false,true] {
        let ws=support::workspace();
        let mut m=support::Mcp::start_in(&ws.root()).await;
        let mut selected=rules("old");
        if websocket {
            selected[0]["match"]["url"]=json!("wss://api.example.test/feed");
            selected[2]["action"]["body"]=json!(include_str!("fixtures/stream-owner.html")
                .replace("new EventSource('https:","new WebSocket('wss:"));
        }
        let launch=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
            "rules":selected,"passthrough_origins":[]})).await;
        let id=&launch["launch_id"];
        if websocket {
            m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#send")},
                "then":{"type":"none"}})).await;
        }
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#close")},
            "then":{"type":"none"}})).await;
        if recover { replace_recorder(&mut m,&ws,id).await; }
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1000}})).await;
        let expected=if websocket { json!([[1,"open",null,0],[1,"close",1000,250]]) }
            else { json!([[1,"open",null,0]]) };
        assert_eq!(events(&mut m,id,"a").await,expected);
        for frame in ["same","b"] {
            assert_eq!(events(&mut m,id,frame).await,json!([[1,"open",null,0],[1,"message","old",1000]]),"{frame}");
        }
        let stopped=m.call_tool("stop",json!({"launch_id":id})).await;
        if websocket {
            let trace=frontcap_core::log::BehaviorLog::read(&std::path::Path::new(stopped["bundle"].as_str().unwrap()).join("trace.jsonl")).unwrap();
            let sends:Vec<_>=trace.iter().filter_map(|e| match &e.kind {
                frontcap_core::event::EventKind::Stream(s) if s.direction==frontcap_core::event::StreamDirection::Out
                    && s.event==frontcap_core::event::StreamEventKind::Message => Some(s), _=>None,
            }).collect();
            assert_eq!(sends.len(),1);
            assert_eq!(sends[0].data.as_deref(),Some("child-send"));
        }
        if !recover {
            let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
            assert_eq!(replay["equal"],true,"{replay}");
        }
    }
}
}

#[tokio::test]
async fn replaced_child_document_does_not_inherit_its_predecessors_stream() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let launch=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
        "rules":rules("old"),"passthrough_origins":[]})).await;
    let id=&launch["launch_id"];
    let before=m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    m.call_tool("network",json!({"launch_id":id,"operation":"set_rules","expected_revision":0,
        "boundary":before["boundary"],"rules":rules("new")})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click",
        "target":{"by":"css","selector":"#replace"}},"then":{"type":"none"}})).await;
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1000}})).await;
    for frame in ["same","b"] {
        assert_eq!(events(&mut m,id,frame).await,json!([[1,"open",null,0],[1,"message","old",1000]]),"{frame}");
    }
    assert_eq!(events(&mut m,id,"a").await,json!([[1,"open",null,0],[1,"message","new",1000]]));
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn child_stream_closed_before_admission_does_not_replace_its_siblings() {
    for websocket in [false,true] {
        let ws=support::workspace();
        let mut m=support::Mcp::start_in(&ws.root()).await;
        let mut selected=rules("old");
        if websocket {
            selected[0]["match"]["url"]=json!("wss://api.example.test/feed");
            selected[2]["action"]["body"]=json!(include_str!("fixtures/stream-owner.html")
                .replace("new EventSource('https:","new WebSocket('wss:"));
        }
        let launch=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
            "rules":selected,"passthrough_origins":[]})).await;
        let id=&launch["launch_id"];
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#open-close")},
            "then":{"type":"none"}})).await;
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":1000}})).await;
        for frame in ["same","b"] {
            assert_eq!(events(&mut m,id,frame).await,json!([[1,"open",null,0],[1,"message","old",1000]]),"{frame}");
        }
        let expected=if websocket { json!([[1,"open",null,0],[2,"error",null,250],[2,"close",1006,250],[1,"message","old",1000]]) }
            else { json!([[1,"open",null,0],[1,"message","old",1000]]) };
        assert_eq!(events(&mut m,id,"a").await,expected);
        m.call_tool("stop",json!({"launch_id":id})).await;
        let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"],true,"{replay}");
    }
}

#[tokio::test]
async fn child_event_sources_reconnect_with_new_stream_bindings_at_the_retry_deadline() {
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let mut selected=rules("old");
    selected[0]["action"]["messages"][0]["at_ms"]=json!(10);
    selected[0]["action"]["close_at_ms"]=json!(30);
    let launch=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
        "rules":selected,"passthrough_origins":[]})).await;
    let id=&launch["launch_id"];
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":3100}})).await;
    for frame in ["same","a","b"] {
        assert_eq!(events(&mut m,id,frame).await,json!([
            [1,"open",null,0],[1,"message","old",10],[1,"error",null,30],
            [1,"open",null,3030],[1,"message","old",3040],[1,"error",null,3060]
        ]),"{frame}");
    }
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

// A real upstream exercises ownership after asynchronous admission, which stream_mock
// cannot establish. The parent value is journaled so offline replay checks child values too.
#[tokio::test]
async fn recorded_child_streams_replay_offline_across_rule_replacement() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let feed=format!("http://{}/feed",listener.local_addr().unwrap());
    let (release,gate)=tokio::sync::watch::channel(false);
    let server=tokio::spawn(async move {
        let mut connections=tokio::task::JoinSet::new();
        for number in 1..=4 {
            let (mut socket,_)=listener.accept().await.unwrap();
            let mut gate=gate.clone();
            connections.spawn(async move {
                let mut request=Vec::new();
                let mut byte=[0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len()<16384);
                }
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\ndata: first-{number}\n\n").as_bytes()).await.unwrap();
                gate.wait_for(|released|*released).await.unwrap();
                socket.write_all(format!("data: last-{number}\n\n").as_bytes()).await.unwrap();
            });
        }
        drop(listener);
        while let Some(result)=connections.join_next().await { result.unwrap(); }
    });
    // Aborting the parent task also drops its JoinSet and all pending sockets on failure.
    struct StopServer(tokio::task::AbortHandle);
    impl Drop for StopServer { fn drop(&mut self) { self.0.abort(); } }
    let _stop_server=StopServer(server.abort_handle());
    let ws=support::workspace();
    let mut m=support::Mcp::start_in(&ws.root()).await;
    let owner=include_str!("fixtures/stream-owner.html").replace("https://api.example.test/feed",&feed)
        .replace("document.querySelector('#events').textContent=JSON.stringify(events)",
            "document.querySelector('#events').textContent=JSON.stringify(events);parent.postMessage({owner:new URL(location.href).searchParams.get('owner'),events},'*')");
    let mut selected=rules("unused");
    selected[0]=json!({"id":"feed","match":{"url":feed},"action":{"type":"record","cassette":"feed"}});
    selected[1]["action"]["body"]=json!("<!doctype html><script>window.received={};addEventListener('message',e=>{if(e.data&&e.data.owner)received[e.data.owner]=e.data.events})</script><iframe id='same' src='/owner?owner=same'></iframe><iframe id='a' src='http://child.example.test/owner?owner=a'></iframe><iframe id='b' src='http://child.example.test/owner?owner=b'></iframe>");
    selected[2]["action"]["body"]=json!(owner);
    let launched=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
        "rules":selected,"passthrough_origins":[]})).await;
    let id=&launched["launch_id"];
    async fn messages(m:&mut support::Mcp,id:&Value,expected:[usize;3])->Vec<Value> {
        for _ in 0..20 {
            let mut seen=Vec::new();
            for frame in ["same","a","b"] { seen.push(events(m,id,frame).await); }
            let counts:Vec<_>=seen.iter().map(|v|v.as_array().unwrap().iter().filter(|e|e[1]=="message").count()).collect();
            if counts==expected { return seen; }
            assert!(counts.iter().zip(expected).all(|(n,max)|*n<=max),"{seen:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
        }
        panic!("child upstream messages did not arrive");
    }
    messages(&mut m,id,[1,1,1]).await;
    let before=m.call_tool("network",json!({"launch_id":id,"operation":"list_rules"})).await;
    let receipt=m.call_tool("network",json!({"launch_id":id,"operation":"set_rules","expected_revision":0,
        "boundary":before["boundary"],"rules":selected})).await;
    assert_eq!(receipt["revision"],1);
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#open")},
        "then":{"type":"none"}})).await;
    messages(&mut m,id,[1,2,1]).await;
    release.send(true).unwrap();
    messages(&mut m,id,[2,4,2]).await;
    server.await.unwrap(); // No listening server or live upstream socket during replay.
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":20}})).await;
    let mut final_events=Vec::new();
    for frame in ["same","a","b"] { final_events.push(events(&mut m,id,frame).await); }
    let value=m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate",
        "expression":"['same','a','b'].map(owner=>window.received[owner])"})).await;
    assert_eq!(value["value"],json!(final_events));
    let stopped=m.call_tool("stop",json!({"launch_id":id})).await;
    let bundle=std::path::Path::new(stopped["bundle"].as_str().unwrap());
    for (path,count) in [("cassettes/feed.jsonl",3),("cassettes/revisions/0001/feed.jsonl",1)] {
        let entries=std::fs::read_to_string(bundle.join(path)).unwrap();
        assert_eq!(entries.lines().map(|line|serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry|entry["stream"]["event"]=="open").count(),count);
    }
    let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn recorded_child_websockets_keep_echoes_and_recorder_loss_with_their_owners() {
    use futures_util::{SinkExt,StreamExt};
    for recover in [false,true] {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let feed=format!("ws://{}/feed",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {
            let mut connections=tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=> {
                        let (stream,_)=accepted.unwrap();
                        connections.spawn(async move {
                            let Ok(mut socket)=tokio_tungstenite::accept_async(stream).await else {return};
                            if socket.send(tokio_tungstenite::tungstenite::Message::text("welcome")).await.is_err() {return;}
                            while let Some(Ok(message))=socket.next().await {
                                if message.is_text() && socket.send(message).await.is_err() {return;}
                            }
                        });
                    }
                    Some(result)=connections.join_next(),if !connections.is_empty()=>{result.unwrap();}
                }
            }
        });
        struct StopServer(tokio::task::AbortHandle);
        impl Drop for StopServer { fn drop(&mut self) { self.0.abort(); } }
        let _stop_server=StopServer(server.abort_handle());
        let ws=support::workspace();
        let mut m=support::Mcp::start_in(&ws.root()).await;
        let mut selected=rules("unused");
        selected[0]=json!({"id":"feed","match":{"url":feed},"action":{"type":"record","cassette":"feed"}});
        selected[2]["action"]["body"]=json!(include_str!("fixtures/stream-owner.html")
            .replace("new EventSource('https://api.example.test/feed')",&format!("new WebSocket('{feed}')")));
        let launch=m.call_tool("launch",json!({"url":"http://top.example.test/parent",
            "rules":selected,"passthrough_origins":[]})).await;
        let id=&launch["launch_id"];
        async fn wait_for(m:&mut support::Mcp,id:&Value,frame:&str,kind:&str,data:Value)->Value {
            for _ in 0..20 {
                let seen=events(m,id,frame).await;
                if seen.as_array().unwrap().iter().any(|e|e[1]==kind && e[2]==data) {return seen;}
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
            }
            panic!("{frame} did not receive {kind} {data}");
        }
        for frame in ["same","a","b"] {wait_for(&mut m,id,frame,"message",json!("welcome")).await;}
        m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#send")},
            "then":{"type":"none"}})).await;
        let seen=wait_for(&mut m,id,"a","message",json!("child-send")).await;
        assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[1]=="message").count(),2);
        for frame in ["same","b"] {
            let seen=events(&mut m,id,frame).await;
            assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[1]=="message").count(),1,"{frame}: {seen}");
        }
        if recover {
            replace_recorder(&mut m,&ws,id).await;
            for frame in ["same","a","b"] {
                let seen=wait_for(&mut m,id,frame,"close",json!(1006)).await;
                assert_eq!(seen.as_array().unwrap().iter().filter(|e|e[1]=="close").count(),1,"{seen}");
            }
            m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child("a","#open")},
                "then":{"type":"none"}})).await;
            for _ in 0..20 {
                let seen=events(&mut m,id,"a").await;
                if seen.as_array().unwrap().iter().any(|e|e[0]==2 && e[1]=="message" && e[2]=="welcome") {break;}
                m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let seen=events(&mut m,id,"a").await;
            assert!(seen.as_array().unwrap().iter().any(|e|e[0]==2 && e[1]=="message" && e[2]=="welcome"),"{seen}");
        }
        let stopped=m.call_tool("stop",json!({"launch_id":id})).await;
        server.abort();let _=server.await;
        let trace=frontcap_core::log::BehaviorLog::read(&std::path::Path::new(stopped["bundle"].as_str().unwrap()).join("trace.jsonl")).unwrap();
        let sent:Vec<_>=trace.iter().filter_map(|e|match &e.kind {
            frontcap_core::event::EventKind::Stream(s) if s.direction==frontcap_core::event::StreamDirection::Out
                && s.event==frontcap_core::event::StreamEventKind::Message=>Some(s),_=>None,
        }).collect();
        assert_eq!(sent.len(),1);
        assert_eq!(sent[0].data.as_deref(),Some("child-send"));
        if !recover {
            let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
            assert_eq!(replay["equal"],true,"{replay}");
        }
    }
}
