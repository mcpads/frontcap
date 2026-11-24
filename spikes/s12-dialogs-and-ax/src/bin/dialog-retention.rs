//! Native controlled-dialog retention, idempotency, and virtual-budget oracle.
use anyhow::{ensure, Context};
use cdp::protocol::{
    emulation::{self, VirtualTimePolicy},
    network, page, target,
};
use spike_common::*;
use std::time::Duration;

async fn scenario(kind: &str, disconnect: bool, hold: Duration) -> anyhow::Result<()> {
    let browser = Browser::start(&[
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapFramesWhileFrozen,FrontcapVirtualNavigation".into(),
    ]).await?;
    let mut p = browser.new_page(false).await?;
    let Browser {
        mut client,
        launched,
    } = browser;
    client
        .call(
            Some(&p.session),
            &emulation::SetVirtualTimePolicyParams {
                initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
                ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause)
            },
        )
        .await?;
    client
        .call(
            Some(&p.session),
            &page::FrontcapSetDialogControlParams::new(true),
        )
        .await?;
    let invocation = match kind {
        "alert" => "alert('held')",
        "confirm" => "confirm('held')",
        "prompt" => "prompt('held','default')",
        _ => unreachable!(),
    };
    eval(&client, &p, &format!(
        "window.log=[]; setTimeout(()=>{{log.push(['before',Date.now()]); const answer={invocation}; log.push(['after',Date.now(),answer??null]);}},100); setTimeout(()=>log.push(['later',Date.now()]),200)"
    )).await?;
    let mut opening = client.subscribe::<page::JavascriptDialogOpeningEvent>(Some(&p.session));
    let mut expired =
        client.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
    set_policy(&client, &p, VirtualTimePolicy::Advance, Some(150.0)).await?;
    opening
        .next_timeout(Duration::from_secs(10))
        .await?
        .context("dialog did not open")?;
    let initial = client
        .call(Some(&p.session), &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        initial.controlled && initial.dialogs.len() == 1,
        "expected one controlled dialog"
    );
    let dialog = initial.dialogs[0].clone();
    ensure!(
        dialog.unix_time_us.as_deref() == Some("946684800100000"),
        "incorrect native opening stamp: {:?}",
        dialog.unix_time_us
    );
    if disconnect {
        drop(client);
        tokio::time::sleep(Duration::from_millis(200)).await;
        client = cdp::Client::connect(&launched.browser_ws_url).await?;
        let attached = client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(target::TargetID(p.target_id.clone()))
                },
            )
            .await?;
        p.session = attached.session_id.0;
        // Renderer domain initialization blocks while the native modal is open.
        // Query and answer through the browser handler before any Page.enable.
        expired = client.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
    }
    tokio::time::sleep(hold).await;
    let held = client
        .call(Some(&p.session), &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        serde_json::to_value(&held.dialogs)? == serde_json::to_value(&initial.dialogs)?,
        "pending dialog changed while held"
    );
    ensure!(
        held.receipts.is_empty(),
        "dialog was automatically answered"
    );
    let invalid =
        page::FrontcapAnswerDialogParams::new("wrong-dialog".into(), "answer-one".into(), true);
    ensure!(
        client.call(Some(&p.session), &invalid).await.is_err(),
        "wrong dialog accepted"
    );
    let answer =
        page::FrontcapAnswerDialogParams::new(dialog.dialog_id.clone(), "answer-one".into(), true);
    let receipt = client.call(Some(&p.session), &answer).await?;
    let again = client.call(Some(&p.session), &answer).await?;
    ensure!(
        serde_json::to_value(&receipt)? == serde_json::to_value(&again)?,
        "duplicate answer receipt changed"
    );
    let conflict =
        page::FrontcapAnswerDialogParams::new(dialog.dialog_id.clone(), "answer-one".into(), false);
    ensure!(
        client.call(Some(&p.session), &conflict).await.is_err(),
        "changed answer arguments accepted"
    );
    if !disconnect {
        expired
            .next_timeout(Duration::from_secs(10))
            .await?
            .context("remaining original budget did not expire")?;
    } else {
        // The old session owns its budget event; establish a renderer barrier
        // before inspecting the retained browser ledger and page effects.
        client
            .call(Some(&p.session), &page::EnableParams::default())
            .await?;
    }
    // The expired event belongs to the old inspector session. Verify the native
    // retained grant independently after reconnect, without granting more time.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let clock = client.call(Some(&p.session), &emulation::GetVirtualTimeParams::default()).await?;
            let now = clock.unix_time_us.parse::<u64>()?;
            ensure!(now <= 946684800150000, "original virtual grant overran: {now}");
            if now == 946684800150000 { return Ok::<(), anyhow::Error>(()); }
            tokio::task::yield_now().await;
        }
    }).await.context("retained original budget did not end")??;
    let state = eval(&client, &p, "({now:Date.now(),log})").await?;
    ensure!(
        state["log"].as_array().context("missing log")?.len() == 2,
        "answer repeated or later timer escaped: {state}"
    );
    ensure!(
        state["log"][0][1] == 946684800100000_i64 / 1000,
        "wrong before time: {state}"
    );
    ensure!(
        state["log"][1][1] == 946684800100000_i64 / 1000,
        "host time advanced modal: {state}"
    );
    ensure!(
        state["now"] == 946684800150_i64,
        "original grant was not preserved: {state}"
    );
    if kind == "prompt" {
        ensure!(
            state["log"][1][2] == "default",
            "omitted prompt answer lost default: {state}"
        );
    }
    // A second connection loss must preserve the receipt, too.
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;
    client = cdp::Client::connect(&launched.browser_ws_url).await?;
    let attached = client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(target::TargetID(p.target_id.clone()))
            },
        )
        .await?;
    p.session = attached.session_id.0;
    let recovered = client.call(Some(&p.session), &answer).await?;
    ensure!(
        serde_json::to_value(&receipt)? == serde_json::to_value(&recovered)?,
        "receipt lost across disconnect"
    );
    let ledger = client
        .call(Some(&p.session), &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        ledger.dialogs.is_empty() && ledger.receipts.len() == 1,
        "unexpected answer ledger"
    );
    report(
        &format!("dialog-retention-{kind}-{disconnect}"),
        serde_json::json!({"opening":dialog,"receipt":receipt,"page":state,"host_hold_ms":hold.as_millis(),"disconnect_while_pending":disconnect}),
    );
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    for kind in ["alert", "confirm", "prompt"] {
        scenario(kind, false, Duration::from_millis(200)).await?;
        scenario(
            kind,
            true,
            if kind == "alert" {
                Duration::from_secs(21)
            } else {
                Duration::from_millis(200)
            },
        )
        .await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(120), run())
        .await.context("native dialog oracle exceeded 120 seconds")?
}
