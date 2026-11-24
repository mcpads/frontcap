//! Verify a sequential capture using its separately retained digest and expected owner.
use frontcap_native::android_capture::{Owner, verify};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 7 && !(args.len()==8 && args[7]=="source-changed") {
        return Err("usage: android_capture directory manifest_sha256 package activity generation pid source_sha256 [source-changed]".into());
    }
    let owner = Owner {
        package: &args[2],
        activity: &args[3],
        generation: &args[4],
        pid: args[5].parse()?,
        source_sha256: &args[6],
    };
    if args.len()==8 {
        let result=frontcap_native::android_capture::verify_collection(std::path::Path::new(&args[0]),&owner,&args[1],true)?;
        assert!(matches!(result,frontcap_core::native::android_capture::Collection::Unavailable(_)));
        println!("verified unavailable source-changed observation");return Ok(());
    }
    let capture = verify(std::path::Path::new(&args[0]), &owner, &args[1])?;
    println!(
        "verified {} source Views and {}x{} screen",
        capture.catalog.targets.len(),
        capture.size[0],
        capture.size[1]
    );
    Ok(())
}
