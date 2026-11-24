use super::*;
use frontcap_core::intervention::CookiePartition;

fn session(origin: &str) -> InitialCookie {
    InitialCookie { origin: origin.into(), name: "session".into(), value: "signed-in".into(), expires_at_ms: None, domain: None,
        path: "/".into(), secure: false, http_only: true, same_site: CookieSameSite::Lax, priority: Default::default(), source: None, timing: None, partition: None }
}

#[test]
fn cookie_scopes_refuse_implicit_origin_repairs_and_unrelated_domains() {
    for origin in ["https://app.example.test", "http://127.0.0.1:8123", "http://[::1]:8123"] {
        validate_initial_cookies(&[session(origin)], 946_684_800_000).unwrap();
    }
    for origin in ["https://user@app.example.test", "https://app.example.test/path", "https://APP.example.test",
        "https://app.example.test:443", "http://127.1", "http://0127.0.0.1", "http://[::1]/", "https://app.example.test#fragment"] {
        assert!(validate_initial_cookies(&[session(origin)], 946_684_800_000).is_err(), "{origin}");
    }
    let mut cookie = session("https://app.example.test");
    cookie.domain = Some(".example.test".into());
    validate_initial_cookies(&[cookie.clone()], 946_684_800_000).unwrap();
    for domain in [".other.test", ".ample.test", "example.test", ".example.test:443"] {
        cookie.domain = Some(domain.into());
        assert!(validate_initial_cookies(&[cookie.clone()], 946_684_800_000).is_err(), "{domain}");
    }
}

#[test]
fn cookie_security_and_storage_identity_are_explicit() {
    let mut cookie = session("https://app.example.test");
    cookie.name = "__Host-Http-session".into();
    assert!(validate_initial_cookies(&[cookie.clone()], 946_684_800_000).is_err());
    cookie.secure = true;
    validate_initial_cookies(&[cookie.clone()], 946_684_800_000).unwrap();
    let mut unsafe_copy = cookie.clone(); unsafe_copy.http_only = false;
    assert!(validate_initial_cookies(&[unsafe_copy], 946_684_800_000).is_err());
    let mut unsafe_copy = cookie.clone(); unsafe_copy.domain = Some(".example.test".into());
    assert!(validate_initial_cookies(&[unsafe_copy], 946_684_800_000).is_err());
    let mut another_port = cookie.clone(); another_port.origin = "https://app.example.test:8443".into();
    assert!(validate_initial_cookies(&[cookie.clone(), another_port], 946_684_800_000).is_err(), "cookie identity is not scoped by port");
    cookie.same_site = CookieSameSite::None;
    cookie.partition = Some(CookiePartition { top_level_site: "https://example.test".into(), has_cross_site_ancestor: true });
    let mut another_site = cookie.clone(); another_site.partition.as_mut().unwrap().top_level_site = "https://elsewhere.test".into();
    validate_initial_cookies(&[cookie.clone(), another_site], 946_684_800_000).unwrap();
    cookie.secure = false;
    assert!(validate_initial_cookies(&[cookie], 946_684_800_000).is_err(), "partition and SameSite=None require Secure");
}

#[test]
fn cookie_protocol_preserves_secure_attribute_independently_of_source() {
    for (origin, host, source_scheme, source_port) in [
        ("https://app.example.test", "app.example.test", network::CookieSourceScheme::Secure, 443),
        ("https://app.example.test:8443", "app.example.test", network::CookieSourceScheme::Secure, 8443),
        ("http://127.0.0.1:8123", "127.0.0.1", network::CookieSourceScheme::NonSecure, 8123),
        ("http://[::1]:8123", "[::1]", network::CookieSourceScheme::NonSecure, 8123),
    ] {
        let c = session(origin);
        let native = cookie_param(&c, 946_684_800_000).unwrap();
        assert!(native.url.is_none(), "a URL lets Chromium override Secure");
        assert_eq!(native.domain.as_deref(), Some(host));
        assert_eq!(native.secure, Some(false));
        assert_eq!(native.source_scheme, Some(source_scheme));
        assert_eq!(native.source_port, Some(source_port));
    }
    let mut c = session("https://app.example.test");
    c.domain = Some(".example.test".into()); c.secure = true;
    let native = cookie_param(&c, 946_684_800_000).unwrap();
    assert_eq!(native.domain.as_deref(), Some(".example.test"));
    assert_eq!(native.secure, Some(true));
}


#[test]
fn source_metadata_is_explicit_and_rejects_invalid_ports_and_secure_conflicts() {
    let mut c = session("https://app.example.test");
    for scheme in [CookieSourceScheme::Unset, CookieSourceScheme::NonSecure, CookieSourceScheme::Secure] {
        for port in [-1, 1, 65535] {
            c.source = Some(CookieSource { scheme, port });
            validate_initial_cookies(&[c.clone()], 946_684_800_000).unwrap();
            let native = cookie_param(&c, 946_684_800_000).unwrap();
            assert_eq!(native.source_port, Some(port));
            assert_eq!(native.source_scheme, Some(match scheme {
                CookieSourceScheme::Unset => network::CookieSourceScheme::Unset,
                CookieSourceScheme::NonSecure => network::CookieSourceScheme::NonSecure,
                CookieSourceScheme::Secure => network::CookieSourceScheme::Secure,
            }));
        }
    }
    for port in [-2, 0, 65536, i64::MAX] {
        c.source = Some(CookieSource { scheme: CookieSourceScheme::Unset, port });
        assert!(validate_initial_cookies(&[c.clone()], 946_684_800_000).is_err());
    }
    c.secure = true;
    c.source = Some(CookieSource { scheme: CookieSourceScheme::NonSecure, port: -1 });
    assert!(validate_initial_cookies(&[c], 946_684_800_000).is_err());
}

#[test]
fn captured_cookie_timing_preserves_microseconds_and_rejects_inconsistent_ages() {
    let mut c = session("https://app.example.test");
    c.timing = Some(CookieTiming { creation_age_us: 121_000_003, last_access_age_us: 100_000_001,
        last_update_age_us: 60_000_002, remaining_lifetime_us: Some(501) });
    validate_initial_cookies(&[c.clone()],946_684_800_000).unwrap();
    let native = cookie_param(&c,946_684_800_000).unwrap().frontcap_timestamps.unwrap();
    let epoch = 12_591_158_400_000_000u64;
    assert_eq!(native.creation,(epoch-121_000_003).to_string());
    assert_eq!(native.last_access,(epoch-100_000_001).to_string());
    assert_eq!(native.last_update,(epoch-60_000_002).to_string());
    assert_eq!(native.expiry,(epoch+501).to_string());
    let mut invalid = c.clone(); invalid.expires_at_ms = Some(946_684_801_000);
    assert!(validate_initial_cookies(&[invalid],946_684_800_000).is_err());
    for timing in [
        CookieTiming { last_access_age_us: 121_000_004, ..c.timing.clone().unwrap() },
        CookieTiming { last_update_age_us: 121_000_004, ..c.timing.clone().unwrap() },
        CookieTiming { creation_age_us: u64::MAX, ..c.timing.clone().unwrap() },
        CookieTiming { remaining_lifetime_us: Some(0), ..c.timing.clone().unwrap() },
        CookieTiming { remaining_lifetime_us: Some(u64::MAX), ..c.timing.clone().unwrap() },
    ] {
        let mut invalid = c.clone(); invalid.timing = Some(timing);
        assert!(validate_initial_cookies(&[invalid],946_684_800_000).is_err());
    }
}

#[test]
fn capture_refuses_unserializable_partition_markers_without_flattening_nonce_keys() {
    // Pinned BuildCookie emits the marker only for non-serializable partitions.
    // False is a nonce partition with a non-opaque site, not an unpartitioned cookie.
    let fixture = serde_json::json!({
        "name":"session", "value":"kept", "domain":"third.test", "path":"/",
        "expires":-1, "size":11, "httpOnly":true, "secure":true, "session":true,
        "priority":"Medium", "sourceScheme":"Secure", "sourcePort":443,
        "frontcapTimestamps":{"creation":"12591158400000000","lastAccess":"12591158400000000","lastUpdate":"12591158400000000","expiry":"0"}
    });
    for marker in [false, true] {
        let mut value = fixture.clone(); value["partitionKeyOpaque"] = marker.into();
        let cookie: network::Cookie = serde_json::from_value(value).unwrap();
        assert!(matches!(captured_cookie(cookie, 12_591_158_400_000_000), Err(ChromiumError::InvalidArgument(reason)) if reason.contains("partition")));
    }
    for key in [None, Some(serde_json::json!({"topLevelSite":"https://example.test","hasCrossSiteAncestor":true}))] {
        let mut value = fixture.clone(); if let Some(key) = key { value["partitionKey"] = key; }
        let cookie: network::Cookie = serde_json::from_value(value).unwrap();
        assert!(captured_cookie(cookie, 12_591_158_400_000_000).is_ok(), "unpartitioned and serializable partitions remain supported");
    }
}
