use cdp::protocol::{emulation, page, target};
use frontcap_chromium::{generation::Endpoint, session::Session};
use frontcap_core::time::VirtualTime;

/// Exercise unread native work while no recorder consumes reports. Shared time
/// suspends on recorder loss, so this fault fixture explicitly adopts its grant.
pub async fn progress_without_recorder(endpoint: &Endpoint, at: VirtualTime, epoch_ms: u64) {
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    let n = Session::new(client, attached.session_id.0);
    let clock = n.call(&page::FrontcapGetSharedClockParams::default()).await.unwrap();
    let held = n.call(&page::FrontcapSuspendSharedClockParams::new(clock.clock_id.clone())).await.unwrap();
    let now = held.unix_time_us.parse::<i128>().unwrap();
    let bound = held.bound_us.parse::<i128>().unwrap();
    let expected = i128::from(epoch_ms) * 1000 + i128::from(at.0);
    assert!(now <= expected && expected <= bound);
    n.call(&page::FrontcapResumeSharedClockParams::new(clock.clock_id, held.suspension_id)).await.unwrap();
    n.call(&emulation::SetVirtualTimePolicyParams {
        budget: Some((bound - now) as f64 / 1000.0),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    }).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let reached = n.call(&page::FrontcapGetSharedClockParams::default()).await.unwrap();
            if reached.unix_time_us.parse::<i128>().unwrap() >= expected { break; }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }).await.expect("native fixture reaches the unread event before recovery");
    n.client.close();
}
