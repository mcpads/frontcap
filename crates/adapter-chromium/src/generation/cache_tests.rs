use super::*;
use serde_json::json;

fn field() -> StateField {
    StateField::CacheEntry {scope:StorageScope {origin:"https://cache.test".into(),storage_key:"https://cache.test/".into()},bucket:None,cache:"items".into(),request_url:"https://cache.test/item".into(),request_headers:BTreeMap::new()}
}

#[test]
fn guards_preserve_native_response_metadata_while_writes_refuse_to_erase_it() {
    let original=json!({"state":"present","response":{"status":200,"response_type":"basic","urls":["https://cache.test/source"],"headers":{"Content-Type":"text/plain"},"body":"source"}});
    let normalized=normalize_cache_record(&field(),&original,false).unwrap();
    assert_eq!(normalized["response"]["response_type"],"basic");
    assert_eq!(normalized["response"]["urls"],json!(["https://cache.test/source"]));
    assert_eq!(normalized["response"]["body_base64"],"c291cmNl");
    assert_eq!(normalized["response"]["headers"],json!({"content-type":"text/plain"}));
    assert!(normalize_cache_record(&field(),&original,true).is_err());
    let opaque=json!({"state":"present","response":{"status":0,"response_type":"opaque_response","body_base64":""}});
    assert!(normalize_cache_record(&field(),&opaque,false).is_ok());
    assert!(normalize_cache_record(&field(),&opaque,true).is_err());
}

#[test]
fn cache_mutation_rejects_ambiguous_bodies_headers_and_noncanonical_requests() {
    for response in [
        json!({"status":200,"body":"text","body_base64":"dGV4dA=="}),
        json!({"status":200,"body_base64":"%%%"}),
        json!({"status":200,"headers":{"x-mode":"one","X-Mode":"two"}}),
        json!({"status":200,"headers":{"vary":"*"}}),
        json!({"status":204,"body":"not-empty"}),
        json!({"status":206,"body":"partial"}),
    ] {
        assert!(normalize_cache_record(&field(),&json!({"state":"present","response":response}),true).is_err());
    }
    for url in ["https://cache.test/item#fragment","https://user@cache.test/item","https://cache.test/a/../item","https://cache.test/%xx"] {
        let mut request=field();
        let StateField::CacheEntry {request_url,..}=&mut request else {unreachable!()};
        *request_url=url.into();
        assert!(normalize_cache_record(&request,&json!({"state":"absent"}),true).is_err(),"{url}");
    }
    let boundary="x".repeat(4*1024*1024);
    assert!(normalize_cache_record(&field(),&json!({"state":"present","response":{"status":200,"body":boundary}}),true).is_ok());
    assert!(normalize_cache_record(&field(),&json!({"state":"present","response":{"status":200,"body":"x".repeat(4*1024*1024+1)}}),true).is_err());
}

fn stored_metadata() -> serde_json::Value {
    json!({"response_type":"basic","urls":["https://cache.test/source"],"cors_exposed_header_names":[],
        "mime_type":"text/plain","request_method":"GET","response_time":"13370000000000000","padding":"0",
        "connection_info":1,"alpn_negotiated_protocol":"http/1.1","was_fetched_via_spdy":false,"request_include_credentials":true})
}
#[test]
fn native_metadata_round_trips_and_detects_inconsistent_response_fields() {
    let original=json!({"state":"present","response":{"status":200,"response_type":"basic","urls":["https://cache.test/source"],"headers":{"Set-Cookie":"private=value"},"body":"source","metadata":stored_metadata()}});
    let normalized=normalize_cache_record(&field(),&original,true).unwrap();
    assert_eq!(normalized["response"]["metadata"],stored_metadata());
    assert!(cache_write_matches(&normalized,&normalized));
    let mut different=normalized.clone();different["response"]["metadata"]["request_include_credentials"]=json!(false);
    assert!(!cache_write_matches(&normalized,&different));
    for (key,bad) in [("response_time",json!("013")),("padding",json!("-1")),("connection_info",json!(43)),("urls",json!(["https://cache.test/a/../source"]))] {
        let mut invalid=original.clone();invalid["response"]["metadata"][key]=bad;
        assert!(normalize_cache_record(&field(),&invalid,true).is_err(),"{key}");
    }
    let mut invalid=original.clone();invalid["response"]["response_type"]=json!("cors");
    assert!(normalize_cache_record(&field(),&invalid,true).is_err());
}
#[test]
fn synthetic_verification_compares_user_fields_and_empty_caches_are_valid() {
    let wanted=normalize_cache_record(&field(),&json!({"state":"present","response":{"status":200,"body":"source"}}),true).unwrap();
    let mut actual=wanted.clone();actual["response"]["metadata"]=stored_metadata();
    actual["response"]["metadata"]["response_type"]=json!("default");actual["response"]["metadata"]["urls"]=json!([]);
    assert!(cache_write_matches(&wanted,&actual));
    actual["response"]["body_base64"]=json!("ZGlmZmVyZW50");
    assert!(!cache_write_matches(&wanted,&actual));
    assert!(validate_initial_caches(&[InitialCache {origin:"https://cache.test".into(),partition:None,bucket:None,name:"empty".into(),entries:vec![]}]).is_ok());
}
