use super::*;
use serde_json::json;

#[test]
fn frame_activity_preserves_pending_work_and_rejects_foreign_or_invalid_samples() {
    use crate::native::android_webview::Snapshot;
    let mut snapshot: Snapshot = serde_json::from_value(json!({"href":"http://localhost/",
        "ready_state":"complete","visibility":"visible","focused":true,
        "performance_ms":25.0,"date_ms":1000.0})).unwrap();
    snapshot.validate(None).unwrap();
    assert!(serde_json::to_value(&snapshot).unwrap().get("frame_activity").is_none());
    let activity = Activity { document:"abcd0000-0000-0000-0000-000000000001".into(),
        pending:2, dispatching:false, last_timestamp:Some(0.0), delivered:1 };
    snapshot.frame_activity = Some(Box::new(activity.clone()));
    snapshot.validate(Some(&activity.document)).unwrap();
    let restored: Snapshot = serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
    assert_eq!(restored.frame_activity, Some(Box::new(activity.clone())));
    assert!(snapshot.validate(Some("abcd0000-0000-0000-0000-000000000002")).is_err());
    snapshot.navigation = Some(serde_json::from_value(json!({
        "document":"abcd0000-0000-0000-0000-000000000002",
        "current":{"href":"http://localhost/","hrefTruncated":false,"focused":true},
        "historyObserved":true,"failure":null,"received":0,"dropped":0,"events":[]})).unwrap());
    assert!(snapshot.validate(None).is_err());
    for invalid in [
        Activity { pending:9_007_199_254_740_992, ..activity.clone() },
        Activity { delivered:9_007_199_254_740_992, ..activity.clone() },
        Activity { last_timestamp:Some(f64::NAN), ..activity.clone() },
        Activity { last_timestamp:Some(-1.0), ..activity.clone() },
        Activity { dispatching:true, last_timestamp:None, ..activity.clone() },
        Activity { document:"foreign".into(), ..activity },
    ] { assert!(!invalid.valid()); }
}
