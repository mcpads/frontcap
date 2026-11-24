//! Public observation and replay retain the controlled Wasm completion instant, including rejection.
mod support;
use serde_json::json;

#[tokio::test]
async fn wasm_success_and_compile_failure_keep_their_response_instant_in_replay() {
    // (module (func (export "answer") (result i32) i32.const 42))
    let module:&[u8]=&[0,97,115,109,1,0,0,0,1,5,1,96,0,1,127,3,2,1,0,7,10,1,6,97,110,115,119,101,114,0,0,10,6,1,4,0,65,42,11];
    let page=r#"<!doctype html><output id="status">waiting</output><output id="timer">pending</output><output id="later">pending</output><script>
setTimeout(()=>document.querySelector('#timer').textContent=String(performance.now()),50);
setTimeout(()=>document.querySelector('#later').textContent=String(performance.now()),400);
WebAssembly.instantiateStreaming(fetch('https://wasm.test/module.wasm')).then(
 ({instance})=>document.querySelector('#status').textContent=JSON.stringify({state:'ready',at:performance.now(),value:instance.exports.answer()}),
 error=>document.querySelector('#status').textContent=JSON.stringify({state:'error',at:performance.now(),name:error.name})
);
</script>"#;
    for invalid in [false,true] {
        let ws=support::workspace();let mut m=support::Mcp::start_in(&ws.root()).await;
        let bytes=if invalid {b"invalid wasm".as_slice()} else {module};
        let launched=m.call_tool("launch",json!({"url":"https://wasm-page.test/","then":{"type":"none"},"rules":[
            {"id":"page","match":{"url":"https://wasm-page.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
            {"id":"wasm","match":{"url":"https://wasm.test/module.wasm"},"action":{"type":"mock","status":200,"headers":{"content-type":"application/wasm","access-control-allow-origin":"*"},"body_base64":frontcap_core::encode::base64(bytes),"latency_ms":300}}
        ]})).await;
        let id=&launched["launch_id"];
        let query=|selector:&str|json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":selector},"props":["text"]}});
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
        assert_eq!(m.call_tool("observe",query("#timer")).await["values"]["text"],"50");
        assert_eq!(m.call_tool("observe",query("#status")).await["values"]["text"],"waiting");
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":250}})).await;
        let observed=m.call_tool("observe",query("#status")).await;
        let result:serde_json::Value=serde_json::from_str(observed["values"]["text"].as_str().unwrap()).unwrap();
        let expected=if invalid {json!({"state":"error","at":300,"name":"CompileError"})} else {json!({"state":"ready","at":300,"value":42})};
        assert_eq!(result,expected,"invalid={invalid}");
        assert_eq!(m.call_tool("observe",query("#later")).await["values"]["text"],"pending");
        m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":100}})).await;
        assert_eq!(m.call_tool("observe",query("#later")).await["values"]["text"],"400");
        m.call_tool("stop",json!({"launch_id":id})).await;
        let replay=m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
        assert_eq!(replay["equal"],true,"invalid={invalid}: {replay}");
    }
}
