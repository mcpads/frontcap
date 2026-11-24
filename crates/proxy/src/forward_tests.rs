use super::*;

#[test]
fn hosts_and_ports_of_authorities() {
    assert_eq!((host_of("example.test:8443"), port_of("example.test:8443", "https")), ("example.test".to_string(), 8443));
    assert_eq!((host_of("[::1]:9000"), port_of("[::1]:9000", "http")), ("::1".to_string(), 9000));
    assert_eq!((host_of("example.test"), port_of("example.test", "https")), ("example.test".to_string(), 443));
    assert_eq!(port_of("[::1]", "http"), 80);
}

/// The fields `Connection` names, in any line and case, go with it; an upgrade keeps what the next hop needs.
#[test]
fn fields_connection_names_are_not_forwarded() {
    let mut h = hyper::HeaderMap::new();
    h.append("connection", "X-Private-Hop, close".parse().unwrap());
    h.append("connection", "x-other".parse().unwrap());
    h.insert("x-private-hop", "local".parse().unwrap());
    h.insert("x-other", "local".parse().unwrap());
    h.insert("keep-alive", "timeout=5".parse().unwrap());
    h.insert("x-kept", "end to end".parse().unwrap());
    strip_hop_by_hop(&mut h, false);
    let names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
    assert_eq!(names, vec!["x-kept"]);
    let mut up = hyper::HeaderMap::new();
    up.insert("connection", "Upgrade, x-private-hop".parse().unwrap());
    up.insert("upgrade", "websocket".parse().unwrap());
    up.insert("x-private-hop", "local".parse().unwrap());
    up.insert("sec-websocket-key", "k".parse().unwrap());
    strip_hop_by_hop(&mut up, true);
    assert_eq!(up.get("connection").unwrap(), "upgrade");
    assert_eq!(up.get("upgrade").unwrap(), "websocket");
    assert!(up.get("x-private-hop").is_none() && up.get("sec-websocket-key").is_some());
}

/// A response loses its hop-by-hop fields, a challenge addressed to the proxy, and the platform's own
/// arrival field, which would move a waiting page's clock.
#[test]
fn a_response_keeps_only_what_is_meant_for_the_page() {
    let mut h = hyper::HeaderMap::new();
    h.insert("connection", "x-hop".parse().unwrap());
    h.insert("x-hop", "1".parse().unwrap());
    h.insert("proxy-authenticate", "Basic".parse().unwrap());
    h.insert("x-frontcap-arrival-us", "9999999999999999".parse().unwrap());
    h.insert("www-authenticate", "Basic".parse().unwrap());
    strip_response(&mut h);
    let names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
    assert_eq!(names, vec!["www-authenticate"]);
}
