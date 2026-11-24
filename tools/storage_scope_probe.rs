//! Native storage capabilities must not cross documents or survive revocation/reuse.
mod support;
use cdp::protocol::{page, target};
use frontcap_chromium::session::Session;
use serde_json::{Value, json};
use std::time::Duration;

macro_rules! command {
    ($name:ident, $method:literal) => {
        #[derive(serde::Serialize)]
        #[serde(transparent)]
        struct $name(Value);
        impl cdp::Command for $name {
            const METHOD: &'static str = $method;
            type Returns = Value;
        }
    };
}
command!(Grant, "Storage.frontcapGrantStorageScope");
command!(Bind, "CacheStorage.frontcapBindStorageScope");
command!(Release, "CacheStorage.frontcapReleaseStorageScope");
command!(Resolve, "Storage.frontcapResolveStorageKey");
command!(OriginSite, "Storage.frontcapGetOriginSite");
command!(Put, "CacheStorage.frontcapPutEntries");
command!(Read, "CacheStorage.requestCachedResponse");
async fn navigate(session: &Session, url: &str) {
    let mut loaded = session.subscribe::<page::LoadEventFiredEvent>();
    session
        .call(&page::NavigateParams::new(url.into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), loaded.next())
        .await
        .unwrap()
        .unwrap();
}
async fn attach(client: &cdp::Client, id: target::TargetID) -> Session {
    let result = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(id)
            },
        )
        .await
        .unwrap();
    let session = Session::new(client.clone(), result.session_id.0);
    session.call(&page::EnableParams::default()).await.unwrap();
    session
}
async fn grant(session: &Session) -> Value {
    session
        .call(&Grant(json!({"origin":"https://scope-probe.test"})))
        .await
        .unwrap()
}
fn binding(grant: &Value) -> Bind {
    Bind(json!({"storageKey":grant["storageKey"],"scopeToken":grant["scopeToken"]}))
}
async fn release(session: &Session, grant: &Value) {
    session
        .call(&Release(json!({"storageKey":grant["storageKey"]})))
        .await
        .unwrap();
}
#[tokio::test]
async fn storage_scope_probe() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", server.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ = request.respond(
                tiny_http::Response::from_string("<!doctype html><title>Scope probe</title>")
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap(),
                    ),
            );
        }
    });
    let (binary, _) = browser_pin::engine::patched_binary(
        &support::repo_root(),
        browser_pin::current_platform().unwrap(),
    )
    .unwrap()
    .expect("build current engine first");
    {
        let ordinary = browser_pin::launch(&binary, &[], Duration::from_secs(10)).unwrap();
        let client = cdp::Client::connect(&ordinary.browser_ws_url)
            .await
            .unwrap();
        let target = client
            .call(None, &target::GetTargetsParams::default())
            .await
            .unwrap()
            .target_infos
            .into_iter()
            .find(|t| t.r#type == "page")
            .unwrap();
        let session = attach(&client, target.target_id).await;
        let error = session
            .call(&Grant(json!({"origin":"https://scope-probe.test"})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("disabled"),
            "ordinary engine must refuse storage scope grants: {error}"
        );
        assert!(
            session
                .call(&OriginSite(
                    json!({"origin":"https://app.example.co.uk:8443"})
                ))
                .await
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
    }
    let browser = browser_pin::launch(
        &binary,
        &["--enable-features=FrontcapFramesWhileFrozen".into()],
        Duration::from_secs(10),
    )
    .unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let target_a = client
        .call(None, &target::CreateTargetParams::new("about:blank".into()))
        .await
        .unwrap()
        .target_id;
    let target_b = client
        .call(None, &target::CreateTargetParams::new("about:blank".into()))
        .await
        .unwrap()
        .target_id;
    let a = attach(&client, target_a.clone()).await;
    let b = attach(&client, target_b).await;
    // Site metadata works on a blank document without navigating an application.
    for (origin, expected) in [
        ("https://app.example.co.uk:8443", "https://example.co.uk"),
        ("https://tenant.github.io", "https://tenant.github.io"),
        ("http://127.0.0.1:8080", "http://127.0.0.1"),
        ("http://localhost:8080", "http://localhost"),
    ] {
        assert_eq!(
            a.call(&OriginSite(json!({"origin":origin}))).await.unwrap()["site"],
            expected
        );
    }
    for origin in [
        "https://example.test/path",
        "data:text/plain,opaque",
        "https://example.test:443",
    ] {
        assert!(a.call(&OriginSite(json!({"origin":origin}))).await.is_err());
    }
    navigate(&a, &url).await;
    navigate(&b, &url).await;
    assert!(a.call(&Resolve(json!({"origin":"https://scope-probe.test","topLevelSite":"https://other.test","hasCrossSiteAncestor":false}))).await.is_err());
    assert!(
        a.call(&Resolve(json!({"origin":"https://scope-probe.test/path"})))
            .await
            .is_err()
    );
    let first = grant(&a).await;
    assert!(
        b.call(&binding(&first)).await.is_err(),
        "another document must not consume the grant"
    );
    a.call(&binding(&first)).await.unwrap();
    let put = a
        .call(&Put(
            json!({"storageKey":first["storageKey"],"cacheName":"probe",
        "entries":[{"requestURL":"https://scope-probe.test/value","requestHeaders":[],
        "responseStatus":200,"responseStatusText":"OK","responseHeaders":[],"body":"c2NvcGU="}]}),
        ))
        .await
        .unwrap();
    let read = a.call(&Read(json!({"cacheId":put["cacheId"],"requestURL":"https://scope-probe.test/value","requestHeaders":[]}))).await.unwrap();
    assert_eq!(
        read["response"]["body"], "c2NvcGU=",
        "bound scope must reach actual storage/readback"
    );
    release(&a, &first).await;
    assert!(
        a.call(&binding(&first)).await.is_err(),
        "a consumed grant must not be reusable"
    );
    let replaced = grant(&a).await;
    navigate(&a, &url).await;
    assert!(
        a.call(&binding(&replaced)).await.is_err(),
        "replacement document must not use the former grant"
    );
    let detached = grant(&a).await;
    client
        .call(
            None,
            &target::DetachFromTargetParams {
                session_id: Some(target::SessionID(a.id.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let a = attach(&client, target_a).await;
    assert!(
        a.call(&binding(&detached)).await.is_err(),
        "new controller session must not use a detached session's grant"
    );
    let mislabeled = grant(&a).await;
    assert!(
        a.call(&Bind(
            json!({"storageKey":"https://wrong-scope.test/","scopeToken":mislabeled["scopeToken"]})
        ))
        .await
        .is_err(),
        "a caller-supplied alias must not override the actual bound identity"
    );
    assert!(
        a.call(&binding(&mislabeled)).await.is_err(),
        "failed binding must not reuse a consumed grant"
    );
    let fresh = grant(&a).await;
    a.call(&binding(&fresh)).await.unwrap();
    release(&a, &fresh).await;
    println!(
        "STORAGE_SCOPE_PROBE {}",
        json!({"feature_disabled":"refused","wrong_document":"refused","reuse":"refused","replaced_document":"refused","detached_session":"refused","identity_alias":"refused","cache_write_readback":"scope","fresh_scope":"bound_and_released"})
    );
}
