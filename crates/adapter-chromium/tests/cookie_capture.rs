//! A live recipe captures the browser cookie store, including scopes absent from the page.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

#[tokio::test]
async fn live_cookies_preserve_foreign_paths_partitions_and_attributes() {
    let runs = tempfile::tempdir().unwrap();
    let url = "https://page.example.test/start";
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":url},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html>"}}]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "cookie-source", url.into(), rules.clone());
    options.profile.initial_cookies = serde_json::from_value(json!([
        {"origin":"https://page.example.test","name":"at_other_path","value":"private","path":"/private","http_only":true,"same_site":"strict","priority":"high"},
        {"origin":"http://legacy.test","name":"unset","value":"legacy","path":"/","same_site":"unspecified","source":{"scheme":"unset","port":-1}},
        {"origin":"http://legacy.test:8123","name":"unset_scheme","value":"legacy","path":"/","same_site":"lax","source":{"scheme":"unset","port":8123}},
        {"origin":"https://legacy.test","name":"secure_unset_scheme","value":"legacy","path":"/","secure":true,"same_site":"lax","source":{"scheme":"unset","port":443}},
        {"origin":"https://legacy.test","name":"secure_unset_port","value":"legacy","path":"/","secure":true,"same_site":"lax","source":{"scheme":"secure","port":-1}},
        {"origin":"http://legacy.test","name":"insecure_unset_port","value":"legacy","path":"/","same_site":"lax","source":{"scheme":"non_secure","port":-1}},
        {"origin":"http://unvisited.test:8123","name":"host","value":"foreign","path":"/","same_site":"unspecified","priority":"low"},
        {"origin":"https://example.test","domain":".example.test","name":"domain","value":"shared","path":"/","secure":true,"same_site":"lax","expires_at_ms":946684900000u64},
        {"origin":"https://third.test:8443","name":"partitioned","value":"first","path":"/","secure":true,"same_site":"none","partition":{"top_level_site":"https://example.test","has_cross_site_ancestor":true}},
        {"origin":"https://third.test:8443","name":"partitioned","value":"second","path":"/","secure":true,"same_site":"none","partition":{"top_level_site":"https://elsewhere.test","has_cross_site_ancestor":true}}
    ])).unwrap();
    let expected = options.profile.initial_cookies.clone();
    let mut source = Generation::launch(options).await.unwrap();
    source.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let live = source.live_state().await.unwrap();
    assert_eq!(source.now().0, 1000, "capture must not progress virtual time");
    assert_eq!(live.cookies.len(), expected.len());
    for cookie in &expected {
        let captured = live.cookies.iter().find(|c| c.name == cookie.name && c.partition == cookie.partition).unwrap();
        let timing = captured.timing.as_ref().unwrap();
        assert_eq!(timing.creation_age_us, 1000);
        assert_eq!(timing.last_update_age_us, 1000);
        assert_eq!(timing.remaining_lifetime_us, cookie.expires_at_ms.map(|ms| (ms - 946_684_800_000) * 1000 - 1000));
        let mut attributes = captured.clone(); attributes.timing = None; attributes.expires_at_ms = cookie.expires_at_ms;
        assert_eq!(&attributes, cookie);
    }
    let mut options = support::options(runs.path(), "cookie-restored", url.into(), rules);
    options.profile.initial_cookies = live.cookies.clone();
    let mut restored = Generation::launch(options).await.unwrap();
    restored.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let recaptured = restored.live_state().await.unwrap().cookies;
    for (mut actual, mut expected) in recaptured.into_iter().zip(live.cookies) {
        let age = actual.timing.take().unwrap(); let old = expected.timing.take().unwrap();
        assert_eq!(age.creation_age_us, old.creation_age_us + 1000);
        assert_eq!(age.last_update_age_us, old.last_update_age_us + 1000);
        assert!(age.last_access_age_us <= old.last_access_age_us + 1000, "navigation may access a cookie");
        assert_eq!(age.remaining_lifetime_us, old.remaining_lifetime_us.map(|us| us - 1000));
        assert_eq!(actual, expected);
    }
    assert_eq!(restored.now().0, 1000);
    restored.stop().await.unwrap();
    source.stop().await.unwrap();
}
