//! Fractional-millisecond budgets must finish once, without granting their remainder twice.
mod support;
use frontcap_chromium::{Generation, generation::ReconnectOptions, session::Session};
use frontcap_core::{ids::RunId, predicate::WatchRegistry, rules::RuleSet, time::{VirtualDuration, VirtualTime}};
use cdp::protocol::{emulation, page, target};

#[tokio::test]
async fn fractional_budgets_keep_the_exact_boundary_and_presentation_time() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "fractional", format!("http://{addr}/presentation_recovery.html"), RuleSet::default())).await.unwrap();
    let mut expected = 0;
    for by in [16_025, 642, 333, 667, 333, 16_667, 1, 999, 333] {
        g.advance(VirtualDuration(by)).await.unwrap();
        expected += by;
        assert_eq!(g.now(), VirtualTime(expected), "the expired fractional budget must not be granted again");
        if expected >= 17_000 {
            let shot = g.screenshot().await.unwrap();
            assert!(shot.current && shot.exact);
            assert_eq!(shot.t_us, VirtualTime(expected));
        }
    }
    g.stop().await.unwrap();
}

#[tokio::test]
async fn fractional_grants_and_one_microsecond_overruns_survive_controller_loss() {
    let addr = support::serve();
    for extra in [0u64, 1] {
        let runs = tempfile::tempdir().unwrap();
        let detached = support::DetachedProfile::new();
        let mut options = support::options(runs.path(), "fractional-recovery", format!("http://{addr}/presentation_recovery.html"), RuleSet::default());
        detached.apply(&mut options);
        let profile = options.profile.clone();
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(201)).await.unwrap();
        let launch_id = g.launch_id().clone();
        let endpoint = g.advance_and_abandon(VirtualDuration(16_025)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // A lost shared controller retains its endpoint but suspends publication.
        // Adopt that hold explicitly before injecting movement beyond the old grant.
        let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
        let attached = client.call(None, &target::AttachToTargetParams {
            flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
        }).await.unwrap();
        let native = Session::new(client, attached.session_id.0);
        let clock = native.call(&page::FrontcapGetSharedClockParams::default()).await.unwrap();
        let epoch = i128::from(profile.initial_date_ms) * 1000;
        let endpoint_us = epoch + 217_025;
        let stopped = clock.unix_time_us.parse::<i128>().unwrap();
        assert!((epoch + 201_000..=endpoint_us).contains(&stopped));
        assert_eq!(clock.bound_us, endpoint_us.to_string());
        assert!(!clock.controller_attached && !clock.suspension_id.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(native.call(&page::FrontcapGetSharedClockParams::default()).await.unwrap().unix_time_us, clock.unix_time_us);
        for bound in [endpoint_us, endpoint_us + i128::from(extra)] {
            let held = native.call(&page::FrontcapSuspendSharedClockParams::new(clock.clock_id.clone())).await.unwrap();
            let now = held.unix_time_us.parse::<i128>().unwrap();
            if now == bound { continue; }
            assert!(now < bound);
            native.call(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)).await.unwrap();
            native.call(&page::FrontcapResumeSharedClockParams {
                bound_us: Some(bound.to_string()),
                ..page::FrontcapResumeSharedClockParams::new(clock.clock_id.clone(), held.suspension_id)
            }).await.unwrap();
            let mut expired = native.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>();
            native.call(&emulation::SetVirtualTimePolicyParams {
                budget: Some((bound - now) as f64 / 1000.0),
                ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
            }).await.unwrap();
            let expired = expired.next_timeout(std::time::Duration::from_secs(3)).await.unwrap();
            assert!(expired.is_some(), "injected grant {now} -> {bound} (extra {extra}) did not expire; native={:?}", native.call(&page::FrontcapGetSharedClockParams::default()).await);
            assert_eq!(native.call(&page::FrontcapGetSharedClockParams::default()).await.unwrap().unix_time_us, bound.to_string());
        }
        native.client.close();
        let (g, recovered) = Generation::reconnect(ReconnectOptions {
            repo_root: support::repo_root(), runs_root: runs.path().into(), run: RunId::new("fractional-recovery").unwrap(),
            launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
            granted_until: None, dialog_rules: Default::default(), upstream: None,
        }).await.unwrap();
        let granted = VirtualTime(217_025);
        let reached = VirtualTime(granted.0 + extra);
        assert_eq!(g.now(), reached);
        assert_eq!(recovered.ran_past, (extra != 0).then_some((granted, reached)));
        g.stop().await.unwrap();
    }
}
