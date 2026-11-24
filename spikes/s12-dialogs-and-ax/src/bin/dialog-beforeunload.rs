//! Native beforeunload answers retain their receipts through cancelled/accepted navigation.
use anyhow::{ensure, Context};
use cdp::protocol::{
    emulation::{self, VirtualTimePolicy},
    input, network, page,
};
use spike_common::*;
use std::time::Duration;

async fn run() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
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
    let url = format!("http://{addr}/beforeunload.html");
    navigate(c, &p, &url).await?;
    advance(c, &p, 50.0).await?;
    ensure!(
        eval(c, &p, "window.loaded===true").await? == true,
        "fixture did not load"
    );
    eval(c, &p, "arm.focus()").await?;
    for (kind, text) in [
        (input::DispatchKeyEventParamsType::KeyDown, Some("\r".to_owned())),
        (input::DispatchKeyEventParamsType::KeyUp, None),
    ] {
        c.call(
            s,
            &input::DispatchKeyEventParams {
                key: Some("Enter".into()),
                code: Some("Enter".into()),
                windows_virtual_key_code: Some(13),
                native_virtual_key_code: Some(13),
                text: text.clone(),
                unmodified_text: text,
                ..input::DispatchKeyEventParams::new(kind)
            },
        )
        .await?;
    }
    ensure!(
        eval(c, &p, "window.activated").await? == true,
        "native input did not grant trusted user activation"
    );
    c.call(s, &page::FrontcapSetDialogControlParams::new(true))
        .await?;
    let mut opening = c.subscribe::<page::JavascriptDialogOpeningEvent>(s);
    for accept in [false, true] {
        let next_url = format!("{url}?arrived");
        let nav_client = c.clone();
        let session = p.session.clone();
        let navigation = tokio::spawn(async move {
            nav_client
                .call(Some(&session), &page::NavigateParams::new(next_url))
                .await
        });
        let event = opening
            .next_timeout(Duration::from_secs(10))
            .await?
            .context("missing native beforeunload")?;
        ensure!(
            matches!(event.r#type, page::DialogType::Beforeunload),
            "wrong native dialog kind"
        );
        let pending = c
            .call(s, &page::FrontcapGetDialogsParams::default())
            .await?;
        ensure!(pending.dialogs.len() == 1, "missing pending beforeunload");
        let d = &pending.dialogs[0];
        ensure!(
            d.unix_time_us.as_deref() == Some("946684800050000"),
            "beforeunload changed the virtual boundary: {:?}",
            d.unix_time_us
        );
        let answer = page::FrontcapAnswerDialogParams::new(
            d.dialog_id.clone(),
            format!("beforeunload-{accept}"),
            accept,
        );
        let receipt = c.call(s, &answer).await?;
        let navigated = tokio::time::timeout(Duration::from_secs(10), navigation).await???;
        if !accept {
            ensure!(
                navigated.error_text.is_some(),
                "cancelled navigation reported success"
            );
            ensure!(
                eval(c, &p, "location.search").await? == "",
                "cancelled navigation changed document"
            );
        } else {
            ensure!(
                navigated.error_text.is_none(),
                "accepted navigation failed: {:?}",
                navigated.error_text
            );
            advance(c, &p, 50.0).await?;
            ensure!(
                eval(c, &p, "location.search").await? == "?arrived",
                "accepted navigation did not arrive"
            );
        }
        let retry = c.call(s, &answer).await?;
        ensure!(
            serde_json::to_value(&receipt)? == serde_json::to_value(&retry)?,
            "navigation lost correlated receipt"
        );
    }
    let ledger = c
        .call(s, &page::FrontcapGetDialogsParams::default())
        .await?;
    ensure!(
        ledger.dialogs.is_empty() && ledger.receipts.len() == 2,
        "incorrect beforeunload ledger"
    );
    report("dialog-beforeunload", serde_json::json!({"ledger":ledger}));
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(120), run())
        .await.context("native dialog oracle exceeded 120 seconds")?
}
