mod support;

use cdp::protocol::{emulation, network, page, target};
use frontcap_chromium::dialog_control::DialogControl;
use frontcap_chromium::session::Session;
use std::time::Duration;

async fn attach(client: cdp::Client, target_id: target::TargetID) -> Session {
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target_id)
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}

async fn pending(control: &DialogControl) -> page::FrontcapDialog {
    let mut updates = control.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let inventory = updates.borrow_and_update().clone().expect("native inventory unavailable");
            if let Some(dialog) = inventory.dialogs.first() {
                return dialog.clone();
            }
            updates.changed().await.unwrap();
        }
    }).await.expect("no native dialog")
}

async fn grant(session: &Session, ms: f64) {
    session.call(&emulation::SetVirtualTimePolicyParams {
        budget: Some(ms), ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    }).await.unwrap();
}

async fn reaches(session: &Session, unix_us: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let clock = session.call(&emulation::GetVirtualTimeParams::default()).await.unwrap();
            let now = clock.unix_time_us.parse::<u64>().unwrap();
            assert!(now <= unix_us, "original virtual grant overran: {now} > {unix_us}");
            if now == unix_us { break }
            tokio::task::yield_now().await;
        }
    }).await.expect("original virtual grant did not complete");
}

#[tokio::test]
async fn native_wait_parks_command_timeout_and_reconnect_preserves_dialog_and_grant() {
    let (binary, _) = browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap())
        .unwrap().expect("build the current patched engine to verify native dialog retention");
    let browser = browser_pin::launch(&binary, &[
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapFramesWhileFrozen,FrontcapVirtualNavigation".into()
    ], Duration::from_secs(20)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let target_id = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await.unwrap().target_id;
    let session = attach(client, target_id.clone()).await;
    let control = DialogControl::attach(&session).await.unwrap();
    session.call(&page::EnableParams::default()).await.unwrap();
    session.call(&emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    }).await.unwrap();
    session.eval("window.answers=[]; setTimeout(()=>{answers.push(prompt('held','default')); answers.push(confirm('chained'));},100); setTimeout(()=>answers.push('later'),200)", None).await.unwrap();
    grant(&session, 150.0).await;
    let dialog = pending(&control).await;
    assert_eq!(dialog.unix_time_us.as_deref(), Some("946684800100000"));
    let blocked = {
        let session = session.clone();
        tokio::spawn(async move { session.eval("Date.now()", None).await })
    };
    let service = session.service_clock();
    let before = service.elapsed();
    tokio::time::sleep(Session::CALL_TIMEOUT + Duration::from_secs(1)).await;
    assert_eq!(service.elapsed(), before, "agent wait consumed the active-service bound");
    assert!(!blocked.is_finished(), "renderer command timed out or escaped the native modal");
    assert_eq!(control.inspect().await.unwrap().dialogs, vec![dialog.clone()]);
    assert!(control.answer(&page::FrontcapAnswerDialogParams::new("wrong".into(), "answer".into(), true)).await.is_err());
    assert_eq!(service.elapsed(), before, "invalid answer released the native hold");
    let answer = page::FrontcapAnswerDialogParams::new(dialog.dialog_id.clone(), "answer".into(), true);
    let receipt = control.answer(&answer).await.unwrap();
    assert_eq!(receipt.prompt_text, "default");
    assert_eq!(control.answer(&answer).await.unwrap(), receipt);
    assert!(control.answer(&page::FrontcapAnswerDialogParams::new(dialog.dialog_id.clone(), "answer".into(), false)).await.is_err());
    let chained = pending(&control).await;
    assert_eq!(chained.message, "chained");
    assert_eq!(chained.unix_time_us.as_deref(), Some("946684800100000"));
    assert_ne!(chained.dialog_id, dialog.dialog_id);
    assert_eq!(control.answer(&answer).await.unwrap(), receipt, "retry affected the following dialog");
    assert_eq!(control.inspect().await.unwrap().dialogs, vec![chained.clone()]);
    control.answer(&page::FrontcapAnswerDialogParams::new(chained.dialog_id.clone(), "chained-answer".into(), false)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), blocked).await.unwrap().unwrap().unwrap();
    reaches(&session, 946_684_800_150_000).await;
    assert_eq!(session.eval("answers", None).await.unwrap(), serde_json::json!(["default", false]));
    control.forget(vec![dialog.dialog_id, chained.dialog_id]).await.unwrap();
    assert!(control.inspect().await.unwrap().receipts.is_empty());

    // A queued command resumes its timeout after the native wait, rather than receiving a new budget.
    // ServiceClock's controlled-time tests cover the exact remaining-budget arithmetic.
    let resumed = service.elapsed();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(service.elapsed() > resumed);

    session.eval("setTimeout(()=>answers.push(confirm('reconnect')),100)", None).await.unwrap();
    grant(&session, 150.0).await;
    let retained = pending(&control).await;
    assert_eq!(retained.unix_time_us.as_deref(), Some("946684800250000"));
    let mut disconnected = control.subscribe();
    drop(session);
    tokio::time::timeout(Duration::from_secs(5), disconnected.wait_for(|state| state.is_err())).await
        .expect("dialog handle kept the CDP connection alive").unwrap();
    assert!(control.inspect().await.is_err());
    drop(control);
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let session = attach(client, target_id).await;
    // Page.enable and other renderer commands are deliberately deferred until after the answer.
    let control = DialogControl::attach(&session).await.unwrap();
    assert_eq!(control.inspect().await.unwrap().dialogs, vec![retained.clone()]);
    let receipt = control.answer(&page::FrontcapAnswerDialogParams::new(retained.dialog_id, "after-reconnect".into(), false)).await.unwrap();
    assert!(!receipt.accept);
    session.call(&page::EnableParams::default()).await.unwrap();
    reaches(&session, 946_684_800_300_000).await;
    assert_eq!(session.eval("answers", None).await.unwrap(), serde_json::json!(["default", false, "later", false]));
}
