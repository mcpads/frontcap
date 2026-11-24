use frontcap_core::native::android_tap::{Receipt, Request};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args:Vec<_>=std::env::args().collect();
    if args.len()!=5 {return Err("usage: android_tap REQUEST RECEIPT GENERATION PID".into());}
    let request:Request=serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let receipt:Receipt=serde_json::from_slice(&std::fs::read(&args[2])?)?;
    receipt.validate(&request,&args[3],args[4].parse()?)?;
    println!("validated {:?}",receipt.state);Ok(())
}
