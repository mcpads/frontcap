use super::*;
use serde_json::json;

#[test]
fn randomness_readback_preserves_counts_and_rejects_foreign_documents() {
    let document="abcd0000-0000-0000-0000-000000000001";
    let endpoint=Endpoint {generation:document.into(),pid:42,port:8123};
    let request=ClockRequest {generation:document.into(),request_id:1,operation:ClockOperation::Read,delta_ns:0};
    let value=json!({"generation":document,"pid":42,"port":8123,"request":request,
        "last_id":1,"now_ns":0,"replayed":false,"status":"ok","applied":false,
        "document":document,"state":{"elapsedMs":0,"dateMs":0,"performanceMs":0,
            "advancing":false,"failure":null,"pendingTimers":0,"cancelledOnFailure":0},
        "progress":null,"error":null,
        "starting_randomness":{"document":document,"seed":42,"math_calls":2,
            "crypto_byte_calls":1,"crypto_uuid_calls":1,"observation_errors":0,"uncontrolled_calls":1}});
    let reply:Reply=serde_json::from_value(value.clone()).unwrap();
    reply.validate(&endpoint,&request,Some(document)).unwrap();
    assert_eq!(serde_json::to_value(&reply).unwrap()["starting_randomness"],value["starting_randomness"]);
    for (field,bad) in [("document",json!("abcd0000-0000-0000-0000-000000000002")),
        ("document",json!("invalid")),("observation_errors",json!(5)),
        ("math_calls",json!(u64::MAX)),("uncontrolled_calls",json!(9_007_199_254_740_992_u64))] {
        let mut invalid=value.clone();invalid["starting_randomness"][field]=bad;
        assert!(serde_json::from_value::<Reply>(invalid).unwrap().validate(&endpoint,&request,Some(document)).is_err(),"{field}");
    }
    let mut legacy=value;legacy.as_object_mut().unwrap().remove("starting_randomness");
    let legacy:Reply=serde_json::from_value(legacy).unwrap();
    legacy.validate(&endpoint,&request,Some(document)).unwrap();
    assert!(legacy.observation.starting_randomness.is_none());
}

#[test]
fn library_publication_is_optional_document_bound_and_not_app_readiness() {
    let document = "abcd0000-0000-0000-0000-000000000001";
    let endpoint = Endpoint { generation: document.into(), pid: 42, port: 8123 };
    let request = ClockRequest { generation: document.into(), request_id: 1,
        operation: ClockOperation::Read, delta_ns: 0 };
    let mut value = json!({"generation":document,"pid":42,"port":8123,"request":request,
        "last_id":1,"now_ns":0,"replayed":false,"status":"ok","applied":false,
        "document":document,"state":{"elapsedMs":0,"dateMs":0,"performanceMs":0,
            "advancing":false,"failure":null,"pendingTimers":0,"cancelledOnFailure":0},
        "progress":null,"error":null});
    let legacy: Reply = serde_json::from_value(value.clone()).unwrap();
    legacy.validate(&endpoint, &request, Some(document)).unwrap();
    assert!(legacy.observation.canvas_kit.is_none());
    for published in [false, true] {
        value["canvas_kit"] = json!({"document":document,"published":published});
        let reply: Reply = serde_json::from_value(value.clone()).unwrap();
        reply.validate(&endpoint, &request, Some(document)).unwrap();
        assert_eq!(serde_json::to_value(reply).unwrap()["canvas_kit"], value["canvas_kit"]);
    }
    for foreign in ["invalid", "abcd0000-0000-0000-0000-000000000002"] {
        value["canvas_kit"]["document"] = json!(foreign);
        let reply: Reply = serde_json::from_value(value.clone()).unwrap();
        assert!(reply.validate(&endpoint, &request, Some(document)).is_err());
    }
    value["canvas_kit"] = json!({"document":document,"published":"true"});
    assert!(serde_json::from_value::<Reply>(value).is_err());
}
