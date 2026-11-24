use super::*;
use std::time::Duration;

#[test]
fn launch_fails_when_binary_exits() {
    // `/usr/bin/false` exits immediately and never writes DevToolsActivePort.
    let err = launch(std::path::Path::new("/usr/bin/false"), &[], Duration::from_secs(5)).unwrap_err();
    assert!(matches!(err, LaunchError::ExitedEarly { .. }), "{err:?}");
}

/// An engine standing in for one on a host that refuses its sandbox: it logs the engine's message and aborts.
fn fake_engine(dir: &std::path::Path, log: &str) -> std::path::PathBuf {
    let binary = dir.join("browsers/patched/abc/linux64/headless_shell");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    write_program(&binary, &format!("#!/bin/sh\necho '{log}' >&2\nexit 134\n"));
    binary
}

/// Writes an executable through a child process. A child that another test thread spawns inherits
/// this process's open files until it executes its own program, and Linux refuses to execute a
/// file that any process holds open for writing (`ETXTBSY`). So this process never opens it for
/// writing.
fn write_program(path: &std::path::Path, text: &str) {
    use std::io::Write as _;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
}

#[test]
fn an_engine_refused_its_sandbox_names_the_command_that_allows_it() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_engine(dir.path(), "[1022/035111.030885:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:129] No usable sandbox! If you are running on Ubuntu 23.10+");
    let err = launch(&binary, &[], Duration::from_secs(5)).unwrap_err();
    assert!(matches!(&err, LaunchError::SandboxRestricted), "{err:?}");
    assert!(err.to_string().contains("sudo browser-pin engine allow-sandbox"), "{err}");
    let profile = tempfile::tempdir().unwrap();
    let err = launch_detached(&binary, &[], profile.path(), Duration::from_secs(5)).unwrap_err();
    assert!(matches!(err, LaunchError::SandboxRestricted), "{err:?}");
}

#[test]
fn an_engine_that_exits_early_reports_its_fatal_line() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_engine(dir.path(), "[1:FATAL:somewhere.cc:1] cannot start");
    let err = launch(&binary, &[], Duration::from_secs(5)).unwrap_err();
    assert!(err.to_string().ends_with(": [1:FATAL:somewhere.cc:1] cannot start"), "{err}");
}

#[test]
fn parses_devtools_active_port() {
    let (port, path) = parse_active_port("49418\n/devtools/browser/abc\n").unwrap();
    assert_eq!(port, 49418);
    assert_eq!(path, "/devtools/browser/abc");
}

#[test]
fn launches_pinned_browser() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pin = crate::load_pin(&root).unwrap();
    let platform = crate::current_platform().unwrap();
    let Some(binary) = crate::installed_binary(&root, &pin, platform) else {
        eprintln!("SKIP launches_pinned_browser: run `cargo run -p browser-pin -- install` first");
        return;
    };
    let launched = launch(&binary, &[], Duration::from_secs(20)).unwrap();
    assert!(launched.browser_ws_url.starts_with("ws://127.0.0.1:"));
}
