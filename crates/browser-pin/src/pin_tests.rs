use super::*;

const SAMPLE: &str = r#"
version = "154.0.8037.92"
revision = 1689415

[artifacts.mac-arm64]
url = "https://example.invalid/a.zip"
sha256 = "77da14e75d7f2568e6f7898d3df7cdc6faac74b15e903b2c9d486ebb6ca9b929"
"#;

#[test]
fn parses_pin_file() {
    let pin = parse_pin(SAMPLE).unwrap();
    assert_eq!(pin.version, "154.0.8037.92");
    assert_eq!(pin.revision, 1689415);
    assert_eq!(pin.artifacts["mac-arm64"].url, "https://example.invalid/a.zip");
}

#[test]
fn rejects_malformed_sha() {
    let bad = SAMPLE.replace("77da14e7", "XYZ");
    assert!(matches!(parse_pin(&bad), Err(PinError::InvalidSha { .. })));
}

#[test]
fn repository_pin_covers_supported_hosts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pin = load_pin(&root).unwrap();
    for platform in ["mac-arm64", "mac-x64", "linux64", "linux-arm64"] {
        assert!(pin.artifacts.contains_key(platform), "missing {platform}");
    }
}
