use super::*;

fn profile() -> Profile {
    serde_json::from_value(serde_json::json!({
        "package":"org.example.client", "activity":"org.example.client.Main",
        "capture_script":"/capture.py", "clock_script":"/clock.py", "edit_script":"/edit.py",
        "tap_script":"/tap.py", "scroll_script":"/scroll.py"
    })).unwrap()
}

#[test]
fn launch_strings_survive_device_shell_parsing() {
    let mut profile = profile();
    let values = ["", "한글 공백", "a'b\"c", "$(printf injected); `printf injected` & *", "http://10.0.2.2:123/g/requests/"];
    for (index, value) in values.iter().enumerate() {
        profile.launch_extras.insert(format!("app.key_{index}"), (*value).into());
    }
    profile.validate().unwrap();
    let args = launch_arguments("org.example.client/org.example.client.Main", Some(&profile));
    // Model adb's join and the receiving shell, without an Android dependency.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("printf '%s\\0' {}", args[6..].join(" "))])
        .output().unwrap();
    assert!(output.status.success());
    let words: Vec<_> = output.stdout.split(|b| *b == 0).collect();
    for (index, value) in values.iter().enumerate() {
        assert_eq!(words[index * 3], b"--es");
        assert_eq!(words[index * 3 + 1], format!("app.key_{index}").as_bytes());
        assert_eq!(words[index * 3 + 2], value.as_bytes());
    }
}

#[test]
fn launch_configuration_is_bounded_and_old_profiles_stay_unchanged() {
    let original = profile();
    let json = serde_json::to_value(&original).unwrap();
    assert!(json.get("launch_extras").is_none());
    assert!(json.get("service_url_extra").is_none());
    assert_eq!(launch_arguments("pkg/Activity", Some(&original)), launch_arguments("pkg/Activity", None));
    for key in ["", "--bad", "a b", "a;echo", "a\n"] {
        let mut p = original.clone();
        p.service_url_extra = Some(key.into());
        assert!(p.validate().is_err());
        p.service_url_extra = None;
        p.launch_extras.insert(key.into(), "ok".into());
        assert!(p.validate().is_err());
    }
    for value in ["\0".into(), "line\nbreak".into(), "x".repeat(2049)] {
        let mut p = original.clone();
        p.launch_extras.insert("key".into(), value);
        assert!(p.validate().is_err());
    }
    let mut p = original;
    for n in 0..17 { p.launch_extras.insert(format!("key{n}"), String::new()); }
    assert!(p.validate().is_err());
}
