use super::*;

fn app(root: &std::path::Path, name: &str, scheme: &str, binary: bool) -> String {
    let path = root.join(format!("{name}.app"));
    std::fs::create_dir_all(&path).unwrap();
    // The transport fixture includes unknown real-world metadata, which must be ignored.
    let info = serde_json::json!({"CFBundleIdentifier": name, "CFBundleVersion": "1",
        "CFBundleURLTypes": [{"CFBundleURLName": name, "CFBundleURLSchemes": [scheme]}]});
    if binary { plist::to_file_binary(path.join("Info.plist"), &info).unwrap(); }
    else { plist::to_file_xml(path.join("Info.plist"), &info).unwrap(); }
    format!(r#""{name}" = {{ CFBundleIdentifier = "{name}"; Path = "{}"; ApplicationType = User; }};"#, path.display())
}

#[test]
fn installed_scheme_admission_rejects_foreign_ambiguous_and_incomplete_metadata() {
    let root = tempfile::tempdir().unwrap();
    let owned = app(root.path(), "dev.example.owned", "OWNED-CLIENT-TEST", true);
    let foreign = app(root.path(), "dev.example.foreign", "foreign", false);
    let inventory = format!("{{ {owned} {foreign} }}");
    let admission = inspect(inventory.as_bytes(), "dev.example.owned", "owned-client-test://item/0?bad#path", || Ok(())).unwrap();
    assert_eq!(admission.apps.len(), 2);
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "foreign://note/1", || Ok(())).is_err());
    assert!(inspect(inventory.as_bytes(), "dev.example.missing", "owned-client-test://item/1", || Ok(())).is_err());
    app(root.path(), "dev.example.foreign", "owned-client-test", false);
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "owned-client-test://item/1", || Ok(())).is_err());
    std::fs::remove_file(root.path().join("dev.example.foreign.app/Info.plist")).unwrap();
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "owned-client-test://item/1", || Ok(())).is_err());
    let inventory = format!("{{ {owned} }}");
    let info = root.path().join("dev.example.owned.app/Info.plist");
    std::fs::write(&info, b"invalid plist").unwrap();
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "owned-client-test://item/1", || Ok(())).is_err());
    app(root.path(), "dev.example.owned", "owned-client-test", true);
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "owned-client-test://item/1",
        || Err(io::Error::other("cancelled"))).is_err());
    app(root.path(), "dev.example.owned", "HTTPS", true);
    assert!(inspect(inventory.as_bytes(), "dev.example.owned", "https://example.com", || Ok(())).is_err());
}
