//! Consecutive native dialogs retain distinct identities and share one task instant.
use anyhow::{ensure, Context};
use cdp::protocol::{
    emulation::{self, VirtualTimePolicy},
    network, page,
};
use spike_common::*;
use std::time::Duration;

async fn run() -> anyhow::Result<()> {
    let browser = Browser::start(&[
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapFramesWhileFrozen,FrontcapVirtualNavigation".into(),
    ]).await?;
    let p = browser.new_page(false).await?;
    let c = &browser.client;
    let s = Some(p.session.as_str());
    c.call(
        s,
        &emulation::SetVirtualTimePolicyParams {
            initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
            ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause)
        },
    )
    .await?;
    c.call(s, &page::FrontcapSetDialogControlParams::new(true))
        .await?;
    eval(c, &p, "window.log=[]; setTimeout(()=>{log.push(['before',Date.now()]); alert('one'); log.push(['confirm',confirm('two'),Date.now()]); log.push(['prompt',prompt('three','default'),Date.now()]); log.push(['dismissed',prompt('four','default'),Date.now()]); log.push(['empty',prompt('five','default'),Date.now()]);},100);setTimeout(()=>log.push(['later']),200)").await?;
    let mut opened = c.subscribe::<page::JavascriptDialogOpeningEvent>(s);
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
    set_policy(c, &p, VirtualTimePolicy::Advance, Some(150.0)).await?;
    let mut previous: Option<page::FrontcapAnswerDialogParams> = None;
    let mut identities = std::collections::BTreeSet::new();
    for (index, (message, accept, text)) in [
        ("one", true, None),
        ("two", false, None),
        ("three", true, Some("한글 🍎")),
        ("four", false, None),
        ("five", true, Some("")),
    ]
    .into_iter()
    .enumerate()
    {
        let event = opened
            .next_timeout(Duration::from_secs(10))
            .await?
            .context("missing consecutive dialog")?;
        ensure!(event.message == message, "unexpected dialog order");
        let pending = c
            .call(s, &page::FrontcapGetDialogsParams::default())
            .await?;
        ensure!(
            pending.dialogs.len() == 1 && pending.receipts.len() == index,
            "incorrect pending/receipt inventory"
        );
        let d = &pending.dialogs[0];
        ensure!(
            identities.insert(d.dialog_id.clone()),
            "dialog identity reused"
        );
        ensure!(
            d.unix_time_us.as_deref() == Some("946684800100000"),
            "task instant changed between dialogs"
        );
        if let Some(prior) = &previous {
            c.call(s, prior).await?;
            let still_pending = c
                .call(s, &page::FrontcapGetDialogsParams::default())
                .await?;
            ensure!(
                still_pending.dialogs[0].dialog_id == d.dialog_id,
                "retry answered the next dialog"
            );
            let reused = page::FrontcapAnswerDialogParams::new(
                d.dialog_id.clone(),
                prior.request_id.clone(),
                accept,
            );
            ensure!(
                c.call(s, &reused).await.is_err(),
                "old request id was accepted for a new dialog"
            );
            let stale = page::FrontcapAnswerDialogParams::new(
                prior.dialog_id.clone(),
                format!("stale-{index}"),
                accept,
            );
            ensure!(c.call(s, &stale).await.is_err(), "stale dialog id accepted");
        }
        if !accept {
            let invalid = page::FrontcapAnswerDialogParams {
                prompt_text: Some("not-an-accepted-prompt".into()),
                ..page::FrontcapAnswerDialogParams::new(
                    d.dialog_id.clone(),
                    format!("invalid-{index}"),
                    false,
                )
            };
            ensure!(
                c.call(s, &invalid).await.is_err(),
                "prompt text was accepted for a dismissal"
            );
        }
        let answer = page::FrontcapAnswerDialogParams {
            prompt_text: text.map(str::to_owned),
            ..page::FrontcapAnswerDialogParams::new(
                d.dialog_id.clone(),
                format!("answer-{index}"),
                accept,
            )
        };
        c.call(s, &answer).await?;
        previous = Some(answer);
    }
    expired
        .next_timeout(Duration::from_secs(10))
        .await?
        .context("original budget did not end")?;
    let state = eval(c, &p, "({now:Date.now(),log})").await?;
    ensure!(
        state
            == serde_json::json!({"now":946684800150_i64,"log":[["before",946684800100_i64],["confirm",false,946684800100_i64],["prompt","한글 🍎",946684800100_i64],["dismissed",null,946684800100_i64],["empty","",946684800100_i64]]}),
        "wrong page effects: {state}"
    );
    let ledger = c
        .call(s, &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        ledger.dialogs.is_empty() && ledger.receipts.len() == 5,
        "missing terminal receipts"
    );
    c.call(
        s,
        &page::FrontcapForgetDialogReceiptsParams::new(identities.into_iter().collect()),
    )
    .await?;
    let forgotten = c
        .call(s, &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        forgotten.receipts.is_empty(),
        "receipt acknowledgement did not clear ledger"
    );
    report(
        "dialog-sequence",
        serde_json::json!({"state":state,"ledger":ledger}),
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(120), run())
        .await.context("native dialog oracle exceeded 120 seconds")?
}
