//! Independently validate a source View receipt against the requested app owner.
use frontcap_core::native::android_views::Observation;
use std::io::Read;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 { return Err("usage: android_views receipt.json request generation pid".into()); }
    let mut bytes = Vec::new();
    std::fs::File::open(&args[0])?.take(1024*1024+1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024*1024 { return Err("View receipt exceeds byte budget".into()); }
    let observation: Observation = serde_json::from_slice(&bytes)?;
    observation.validate(&args[1],&args[2],args[3].parse()?)?;
    println!("validated source View observation");
    Ok(())
}
