//! Role predicates share native inspector semantics across dynamic documents and recorder loss.
mod support;
use serde_json::json;

#[tokio::test]
async fn native_roles_drive_cross_origin_watches_condition_stops_and_recovery() {
    for recover in [false,true] {
        let ws=support::workspace();
        let mut m=support::Mcp::start_in(&ws.root()).await;
        let page=include_str!("../../adapter-chromium/tests/fixtures/roles.html")
            .replace("roles-child.html","http://child.roles.test/roles-child.html");
        let child=include_str!("../../adapter-chromium/tests/fixtures/roles-child.html");
        let rules=json!([
            {"id":"main","match":{"url":"http://roles.test/roles.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
            {"id":"child","match":{"url":"http://child.roles.test/roles-child.html"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":child}}
        ]);
        let launched=m.call_tool("launch",json!({"url":"http://roles.test/roles.html","then":{"type":"none"},"rules":rules})).await;
        let id=&launched["launch_id"];
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
        let target=json!({"by":"frame","frame":{"by":"role","role":"iframe","name":"Child controls"},"target":{"by":"role","role":"button","name":"Child ready"}});
        let predicate=json!({"is":"attached","target":target});
        let before=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"scene"}})).await;
        let registered=m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"child-ready","predicate":predicate})).await;
        assert_eq!(registered["value"],"false","{registered}");
        let after=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"scene"}})).await;
        assert_eq!(after["boundary"],before["boundary"]);
        if recover {
            let runtime=ws.root().join("runtimes").join(id.as_str().unwrap());
            frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap();
            m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
        }
        let waited=m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"child-ready","max_ms":1000}})).await;
        assert_eq!(waited["outcome"]["type"],"condition_met","{waited}");
        assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],100000);
        let found=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":target,"props":["text"]}})).await;
        assert_eq!(found["values"]["text"],"Child control");
        let ready=json!({"is":"attached","target":{"by":"role","role":"button","name":"Ready"}});
        let waited=m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":ready,"max_ms":1000}})).await;
        assert_eq!(waited["outcome"]["type"],"condition_met","{waited}");
        assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],200000);
        m.call_tool("stop",json!({"launch_id":id})).await;
        if !recover {
            let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
            assert_eq!(replay["equal"],true,"{replay}");
            let saved=m.call_tool("state",json!({"operation":"save","kind":"recipe","recipe":{
                "url":"http://roles.test/roles.html","rules":rules,
                "barrier":{"type":"until","predicate":ready,"max_ms":1000},
                "verify":[{"name":"ready","predicate":ready}]}})).await;
            let branch=m.call_tool("state",json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"]})).await;
            assert_eq!(m.call_tool("status",json!({"launch_id":branch["launch_id"]})).await["t_us"],200000,"{branch}");
            m.call_tool("stop",json!({"launch_id":branch["launch_id"]})).await;

        }
    }
}
