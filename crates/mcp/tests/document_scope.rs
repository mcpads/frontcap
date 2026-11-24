mod support;
use serde_json::json;

/// Identical selectors in three documents must remain independently addressable through every
/// consumer of a locator, including a watch that survives recorder loss and replay.
#[tokio::test]
async fn document_selectors_isolate_top_and_nested_frames() {
    for recover in [false, true] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let body = |name: &str, child: &str| format!(
            r#"<!doctype html><button id=save onclick="this.textContent='saved';setTimeout(()=>this.textContent='ready',100)">{name}</button>{child}"#
        );
        let rules = json!([
            {"id":"top","match":{"url":"https://top.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body("top",r#"<iframe id=child src="https://child.test/"></iframe>"#)}},
            {"id":"child","match":{"url":"https://child.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body("child",r#"<iframe id=nested src="https://nested.test/"></iframe>"#)}},
            {"id":"nested","match":{"url":"https://nested.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body("nested","")}}
        ]);
        let launched = m.call_tool("launch", json!({"url":"https://top.test/","rules":rules})).await;
        let id = &launched["launch_id"];
        let local = json!({"by":"document","selector":"#save"});
        let child = json!({"by":"frame","frame":{"by":"document","selector":"#child"},"target":local});
        let nested = json!({"by":"frame","frame":{"by":"document","selector":"#child"},"target":{
            "by":"frame","frame":{"by":"document","selector":"#nested"},"target":local}});
        let query = |target| json!({"launch_id":id,"op":{"type":"query","target":target,"props":["text"]}});
        let (failed, error) = m.try_tool("observe", query(json!({"by":"css","selector":"#save"}))).await;
        assert!(failed && error["code"] == "ambiguous_locator", "{error}");
        for (target, text) in [(&local,"top"),(&child,"child"),(&nested,"nested")] {
            let result = m.call_tool("observe", query(target.clone())).await;
            assert_eq!(result["values"]["text"], text, "{result}");
        }
        let predicate = json!({"is":"text_equals","target":child,"text":"ready"});
        let watched = m.call_tool("watch", json!({"launch_id":id,"operation":"add","name":"child-ready","predicate":predicate})).await;
        assert_eq!(watched["value"], "false", "{watched}");
        m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":child},"then":{"type":"none"}})).await;
        let result = m.call_tool("observe", query(child.clone())).await;
        assert_eq!(result["values"]["text"], "saved", "{result}");
        if recover {
            let runtime = ws.root().join("runtimes").join(id.as_str().unwrap());
            frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap();
            m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
        }
        m.call_tool("wait", json!({"launch_id":id,"op":{"type":"until","predicate":predicate,"max_ms":200}})).await;
        for (target, text) in [(&local,"top"),(&child,"ready"),(&nested,"nested")] {
            let result = m.call_tool("observe", query(target.clone())).await;
            assert_eq!(result["values"]["text"], text, "{result}");
        }
        let history = m.call_tool("watch", json!({"launch_id":id,"operation":"history","name":"child-ready","changes_only":true})).await;
        assert_eq!(history["samples"].as_array().unwrap().last().unwrap()["value"], "true", "{history}");
        m.call_tool("stop", json!({"launch_id":id})).await;
        if !recover {
            let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
            assert_eq!(replay["equal"], true, "{replay}");
        }
    }
}
