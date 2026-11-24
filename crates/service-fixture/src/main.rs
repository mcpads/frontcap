use frontcap_service_fixture::Fixture;
use std::{
    io::{self, Write},
    path::PathBuf,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(
        args.next()
            .ok_or("usage: frontcap-service-fixture NEW_DIRECTORY [ASSET_MANIFEST]")?,
    );
    let assets = if let Some(manifest) = args.next() {
        let manifest = PathBuf::from(manifest);
        let mut assets: Vec<frontcap_service_fixture::assets::AssetInput> =
            serde_json::from_slice(&std::fs::read(&manifest)?)?;
        for asset in &mut assets {
            if asset.source.is_relative() {
                asset.source = manifest.parent().unwrap().join(&asset.source);
            }
        }
        assets
    } else {
        Vec::new()
    };
    if args.next().is_some() {
        return Err("expected a new evidence directory and optional asset manifest".into());
    }
    let fixture = Fixture::create_with_assets(directory, assets).await?;
    println!("{}", serde_json::to_string(&fixture.record)?);
    io::stdout().flush()?;
    fixture.run().await?;
    Ok(())
}
