//! Cookie recipes retain non-page scopes and authoritative deletions through branch and replay.
mod support;
use serde_json::json;

#[tokio::test]
async fn live_recipe_preserves_cookie_scopes_without_resurrecting_deleted_seeds() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://cookie-recipe.test";
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/start"),"profile":{"initial_cookies":[
        {"origin":origin,"name":"deleted","value":"seed","path":"/","same_site":"lax"},
        {"origin":origin,"name":"private","value":"kept","path":"/private","http_only":true,"same_site":"strict","priority":"high"},
        {"origin":"http://unvisited.test","name":"foreign","value":"kept","path":"/","same_site":"unspecified","source":{"scheme":"unset","port":-1}},
        {"origin":"https://third.test","name":"partitioned","value":"kept","path":"/","secure":true,"same_site":"none","partition":{"top_level_site":origin,"has_cross_site_ancestor":true}}
    ]},"rules":[{"id":"page","match":{"url":format!("{origin}/start")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>document.cookie='deleted=; Path=/; Max-Age=0'</script>"}}]})).await;
    let id = &launched["launch_id"];
    let before = m.call_tool("status", json!({"launch_id":id})).await["t_us"].as_u64().expect("live status must expose an integer clock");
    let saved = m.call_tool("state", json!({"operation":"save","kind":"recipe","from":id})).await;
    assert_eq!(m.call_tool("status", json!({"launch_id":id})).await["t_us"], before);
    let described = m.call_tool("state", json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let cookies = &described["initial_state"]["cookies"];
    assert_eq!(cookies.as_array().unwrap().len(), 3, "{described}");
    assert!(cookies.as_array().unwrap().iter().all(|c| c["name"] != "deleted"));
    assert!(cookies.as_array().unwrap().iter().any(|c| c["name"] == "foreign" && c["origin"] == "http://unvisited.test" && c["same_site"] == "unspecified" && c["source"] == json!({"scheme":"unset","port":-1})));
    assert!(cookies.as_array().unwrap().iter().any(|c| c["name"] == "partitioned" && c["partition"]["top_level_site"] == origin));
    let branch = m.call_tool("state", json!({"operation":"branch","kind":"recipe","saved_id":saved["saved_id"],"request_id":"cookie-child"})).await;
    assert_eq!(branch["phase"], "ready", "{branch}");
    let child = &branch["launch_id"];
    let saved = m.call_tool("state", json!({"operation":"save","kind":"recipe","from":child})).await;
    let restored = m.call_tool("state", json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    let elapsed = m.call_tool("status", json!({"launch_id":child})).await["t_us"].as_u64().unwrap();
    for (actual, expected) in restored["initial_state"]["cookies"].as_array().unwrap().iter().zip(cookies.as_array().unwrap()) {
        assert_eq!(actual["timing"]["creation_age_us"].as_u64().unwrap(), expected["timing"]["creation_age_us"].as_u64().unwrap() + elapsed);
        assert_eq!(actual["timing"]["last_update_age_us"].as_u64().unwrap(), expected["timing"]["last_update_age_us"].as_u64().unwrap() + elapsed);
        let mut a = actual.clone(); let mut e = expected.clone();
        a.as_object_mut().unwrap().remove("timing"); e.as_object_mut().unwrap().remove("timing");
        assert_eq!(a,e);
    }
    m.call_tool("stop", json!({"launch_id":child})).await;
    let replay = m.call_tool("debug", json!({"launch_id":child,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
