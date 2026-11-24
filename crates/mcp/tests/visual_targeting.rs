//! Public image-to-input workflow using the adapter's existing visual geometry fixture.
mod support;

use serde_json::{Value, json};

fn serve_visual_fixture() -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            // Keep one fixture owner: these bytes are the unchanged adapter acceptance fixture.
            let body: Option<&[u8]> = match request.url().split('?').next().unwrap() {
                "/nested-visual.html" => Some(include_bytes!("../../adapter-chromium/tests/fixtures/nested-visual.html")),
                "/nested-visual-middle.html" => Some(include_bytes!("../../adapter-chromium/tests/fixtures/nested-visual-middle.html")),
                "/nested-visual-leaf.html" => Some(include_bytes!("../../adapter-chromium/tests/fixtures/nested-visual-leaf.html")),
                _ => None,
            };
            let response = match body {
                Some(body) => tiny_http::Response::from_data(body)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap()).boxed(),
                None => tiny_http::Response::from_string("not found").with_status_code(404).boxed(),
            };
            let _ = request.respond(response);
        }
    });
    address
}

async fn query(mcp: &mut support::Mcp, launch: &Value, reference: &Value, props: Value) -> Value {
    mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"query","target":{"by":"ref","id":reference},"props":props}})).await
}

#[tokio::test]
async fn image_points_reach_nested_controls_and_canvas_through_the_public_surface() {
    let address = serve_visual_fixture();
    let workspace = support::workspace();
    let mut mcp = support::Mcp::start_in(&workspace.root()).await;
    let launched = mcp.call_tool("launch", json!({
        "url":format!("http://{address}/nested-visual.html"),"then":{"type":"none"},
        "profile":{"viewport":{"width":1280,"height":720,"device_scale_milli":2000}},
        "passthrough_origins":[format!("http://{address}"),format!("http://localhost:{}",address.port())]
    })).await;
    let launch = &launched["launch_id"];
    mcp.call_tool("wait", json!({"launch_id":launch,"op":{"type":"advance","by_ms":201}})).await;
    let response = mcp.request("tools/call", json!({"name":"observe","arguments":{"launch_id":launch,"op":{"type":"scene","image":true}}})).await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert!(response["result"]["content"].as_array().unwrap().iter().any(|part| part["type"] == "image" && part["mimeType"] == "image/png"));
    let scene = &response["result"]["structuredContent"];
    let image = &scene["presentation"]["image"];
    let boundary = &scene["boundary"];
    assert_eq!(boundary["t_us"], 201_000);
    assert_eq!(image["capture"]["current"], true);
    assert_eq!(image["capture"]["exact"], true);
    // Device scale is a browser profile setting; the retained PNG has its own pixel/CSS mapping.
    let scale_x = image["pixel_size"][0].as_f64().unwrap() / image["viewport_size"][0].as_f64().unwrap();
    let scale_y = image["pixel_size"][1].as_f64().unwrap() / image["viewport_size"][1].as_f64().unwrap();
    assert!(scale_x.is_finite() && scale_x > 0.0 && scale_y.is_finite() && scale_y > 0.0);

    // Fixture geometry gives top (712 - .5u, 330 - .5v). The center is covered; this
    // visible point reaches button-local (16,20), independent of the product's click candidate.
    let hit = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"locate_image","image_id":image["id"],"point":[694.0*scale_x,300.0*scale_y]}})).await;
    let button = &hit["target"]["ref"];
    assert_eq!(hit["tag"], "button");
    assert_eq!(&hit["target"]["boundary"], boundary);
    let inspected = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"inspect","target":{"by":"ref","id":button},"props":["bbox","interaction"]}})).await;
    assert_eq!(&inspected["boundary"], boundary);
    assert_eq!(inspected["ref"], *button);
    assert_eq!(inspected["click"]["ready"], false);
    assert!(inspected["click"]["reason"].as_str().unwrap().starts_with("covered_by"));
    for (property, expected) in [("x",652.0),("y",290.0),("width",50.0),("height",20.0)] {
        assert!((inspected["values"]["bbox"][property].as_f64().unwrap() - expected).abs() < 0.001);
    }
    assert_eq!(inspected["border_quad"].as_array().unwrap().len(), 8);
    let (failed, error) = mcp.try_tool("act", json!({"launch_id":launch,"action":{"type":"click","target":{"by":"ref","id":button}},"then":{"type":"none"}})).await;
    assert!(failed && error["code"] == "not_actionable", "{error}");

    let path = hit["hit_path"]["nodes"].as_array().unwrap();
    assert!(hit["hit_path"]["omission"].is_null());
    assert_eq!(path[0]["ref"], *button);
    assert_eq!(path[0]["via"], "hit");
    assert_eq!(path.last().unwrap()["kind"], "document");
    assert_eq!(path.iter().filter(|node| node["kind"] == "document").count(), 3);
    assert_eq!(path.iter().filter(|node| node["kind"] == "shadow_root").count(), 1);
    let mut owners = Vec::new();
    for node in path.iter().filter(|node| node["via"] == "frame_owner" || node["via"] == "shadow_host") {
        let identity = query(&mut mcp, launch, &node["ref"], json!(["attr:id"])).await;
        if node["via"] == "frame_owner" { owners.push(identity["values"]["attr:id"].clone()); }
        else { assert_eq!(identity["values"]["attr:id"], "host"); }
    }
    assert_eq!(owners, vec![json!("inner"), json!("outer")]);
    let covered = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"locate","boundary":boundary,"point":[678,300]}})).await;
    assert_eq!(query(&mut mcp, launch, &covered["target"]["ref"], json!(["attr:id"])).await["values"]["attr:id"], "veil");
    let crop = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"crop","image_id":image["id"],"rect":[(649.0*scale_x) as u32,(285.0*scale_y) as u32,(60.0*scale_x) as u32,(30.0*scale_y) as u32]}})).await;
    assert_eq!(&crop["boundary"], boundary);
    let mapped = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"locate_image","image_id":crop["id"],"point":[45.0*scale_x,15.0*scale_y]}})).await;
    assert_eq!(mapped["target"]["ref"], *button);
    assert_eq!(mapped["hit_path"], hit["hit_path"]);
    let unchanged = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"scene"}})).await;
    assert_eq!(&unchanged["boundary"], boundary, "inspection, failed center click and crop must leave this boundary intact");

    let click = json!({"type":"click_at","target":{"by":"ref","id":button},"point":hit["point"],"boundary":boundary});
    mcp.call_tool("act", json!({"launch_id":launch,"action":click,"then":{"type":"none"}})).await;
    let props = json!(["attr:data-clicks","attr:data-x","attr:data-y","attr:data-trusted"]);
    let received = query(&mut mcp, launch, button, props.clone()).await;
    assert_eq!(received["values"], json!({"attr:data-clicks":"1","attr:data-x":"16","attr:data-y":"20","attr:data-trusted":"true"}));
    let (failed, error) = mcp.try_tool("observe", json!({"launch_id":launch,"op":{"type":"locate_image","image_id":crop["id"],"point":[45.0*scale_x,15.0*scale_y]}})).await;
    assert!(failed && error["code"] == "observation_changed", "{error}");
    let (failed, error) = mcp.try_tool("act", json!({"launch_id":launch,"action":click,"then":{"type":"none"}})).await;
    assert!(failed && error["code"] == "observation_changed", "{error}");
    assert_eq!(query(&mut mcp, launch, button, props.clone()).await["values"], received["values"]);

    let after = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"scene"}})).await;
    assert_eq!(after["boundary"]["t_us"], boundary["t_us"]);
    let canvas = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"locate","boundary":after["boundary"],"point":[692,275]}})).await;
    assert_eq!(canvas["canvas"], true);
    assert_eq!(canvas["hit_path"]["nodes"][0]["ref"], canvas["target"]["ref"]);
    assert_eq!(&canvas["hit_path"]["nodes"].as_array().unwrap()[1..], &path[1..]);
    assert!(canvas["hit_path"]["omission"].is_null());
    mcp.call_tool("act", json!({"launch_id":launch,"action":{"type":"click_at","target":{"by":"ref","id":canvas["target"]["ref"]},"point":canvas["point"],"boundary":after["boundary"]},"then":{"type":"none"}})).await;
    assert_eq!(query(&mut mcp, launch, &canvas["target"]["ref"], props).await["values"], json!({"attr:data-clicks":"1","attr:data-x":"20","attr:data-y":"10","attr:data-trusted":"true"}));
    let final_scene = mcp.call_tool("observe", json!({"launch_id":launch,"op":{"type":"scene"}})).await;
    assert_eq!(final_scene["boundary"]["t_us"], boundary["t_us"]);
    mcp.call_tool("stop", json!({"launch_id":launch})).await;
}
