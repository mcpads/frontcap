//! Validate a retained source edit against its separately saved request and owner.
use frontcap_core::native::android_edit::{Receipt, Request};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: android_edit request.json receipt.json generation pid".into());
    }
    let request: Request = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let receipt: Receipt = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    receipt.validate(&request, &args[2], args[3].parse()?)?;
    println!(
        "verified request {}: {:?}",
        request.request_id, receipt.state
    );
    Ok(())
}
