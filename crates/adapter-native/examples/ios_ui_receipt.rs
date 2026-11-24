//! Verify saved bridge calls/results, raw receipts and captures without contacting a target.
use frontcap_core::native::{
    ios_request::{Completion, Owner, Request},
    ios_ui::UiOutcome,
};
use frontcap_native::{
    ios::{UiCall, UiReply},
    ios_receipt,
};
use serde::de::DeserializeOwned;
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};
fn read<T: DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(io::Error::other("receipt exceeds byte budget").into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or("usage: ios_ui_receipt directory result.json ...")?,
    );
    let owner: Owner = read(&root.join("request-owner.json"))?;
    owner.validate()?;
    let mut count = 0;
    for name in args {
        let path = PathBuf::from(name);
        let stem = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".result.json"))
            .ok_or("expected a .result.json file")?;
        let call: UiCall = read(&path.with_file_name(format!("{stem}.call.json")))?;
        let reply: UiReply = read(&path)?;
        reply.validate(&call)?;
        let UiReply::Completed { completion } = reply else {
            return Err("not a completed result".into());
        };
        if let UiOutcome::Launched { app } = &completion.outcome {
            if app != &owner.app {
                return Err("launch differs from owner".into());
            }
            ios_receipt::verify_launch(&root, &owner.runner, &completion)?;
        } else {
            let request = Request::Ui(completion.request.clone());
            ios_receipt::verify(&root, &owner, &request, &Completion::Ui(completion))?;
        }
        count += 1;
    }
    if count == 0 {
        return Err("no result files supplied".into());
    }
    println!("verified {count} UIKit bridge results and their retained evidence");
    Ok(())
}
