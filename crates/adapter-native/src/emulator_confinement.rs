//! Opt-in direct-write confinement for an emulator beneath its owned scratch volume.
use std::{io, path::Path, process::Command};

pub(crate) fn validate(enabled: bool, volume: bool, inventory: bool, supervised: bool) -> io::Result<()> {
    if enabled && (!cfg!(target_os = "macos") || !volume || !inventory || !supervised) {
        return Err(io::Error::other("emulator write confinement requires macOS, supervised scratch volume and staged AVD inventory"));
    }
    Ok(())
}

pub(crate) fn policy(root: &Path) -> io::Result<String> {
    let path = root.to_str().ok_or_else(|| io::Error::other("non-UTF8 emulator write root"))?;
    Ok(format!("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* (subpath {}))\n(allow file-write* (literal \"/dev/null\"))\n", serde_json::to_string(path)?))
}

pub(crate) fn prepare(command: &Command, root: &Path, avd: &Path, evidence: &Path) -> io::Result<Command> {
    use std::os::unix::fs::MetadataExt;
    let root = root.canonicalize()?;
    let avd = avd.canonicalize()?;
    let metadata = std::fs::metadata(&root)?;
    if !metadata.is_dir() || !avd.starts_with(&root) || avd == root
        || metadata.dev() == std::fs::metadata(evidence)?.dev() {
        return Err(io::Error::other("emulator write root must be owned mounted scratch containing the staged AVD"));
    }
    let home = root.join("sdk-home");
    let temporary = root.join("sdk-temporary");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&temporary)?;
    let policy = policy(&root)?;
    #[derive(serde::Serialize)]
    struct Manifest<'a> {
        root: &'a Path,
        device: u64,
        inode: u64,
        avd: &'a Path,
        home: &'a Path,
        temporary: &'a Path,
        policy: &'a str,
        descriptors: &'static str,
        scope: &'static str,
    }
    frontcap_core::write_budget::save_inherited(frontcap_core::write_budget::Class::Ordinary,
        &evidence.join("emulator-write-root.json"), &Manifest {
            root: &root, device: metadata.dev(), inode: metadata.ino(), avd: &avd,
            home: &home, temporary: &temporary, policy: &policy,
            descriptors: "resource supervisor marks non-stdio descriptors close-on-exec",
            scope: "emulator descendant direct writes; external services and caller roots excluded",
        })?;
    let mut confined = confine_command(command, &policy);
    for key in ["HOME", "ANDROID_USER_HOME", "ANDROID_EMULATOR_HOME"] { confined.env(key, &home); }
    for key in ["TMPDIR", "TMP", "TEMP"] { confined.env(key, &temporary); }
    Ok(confined)
}

pub(crate) fn confine_command(command: &Command, policy: &str) -> Command {
    let mut confined = Command::new("/usr/bin/sandbox-exec");
    confined.args(["-p", policy]).arg(command.get_program()).args(command.get_args());
    for (key, value) in command.get_envs() {
        if let Some(value) = value { confined.env(key, value); } else { confined.env_remove(key); }
    }
    if let Some(directory) = command.get_current_dir() { confined.current_dir(directory); }
    confined
}

/// Client writes share the original private server's scratch; evidence remains parent-owned.
pub(crate) fn adb_client(command: &Command, root: &Path) -> io::Result<Command> {
    let mut confined = confine_command(command, &policy(root)?);
    for key in ["HOME", "ANDROID_USER_HOME"] { confined.env(key, root.join("adb-home")); }
    for key in ["TMPDIR", "TMP", "TEMP"] { confined.env(key, root.join("adb-temporary")); }
    Ok(confined)
}

/// Prepare the same isolated ADB home for key generation and a foreground server.
pub(crate) fn adb_server(command: &Command, root: &Path, port: u16) -> io::Result<(Command, Command)> {
    if !cfg!(target_os = "macos") || command.get_args().ne(
        ["-L".to_owned(), format!("tcp:{port}"), "server".into(), "nodaemon".into()].iter().map(std::ffi::OsStr::new)) {
        return Err(io::Error::other("confined ADB server requires macOS and the declared foreground endpoint"));
    }
    let mut base = Command::new(command.get_program());
    if let Some(directory) = command.get_current_dir() { base.current_dir(directory); }
    for (key, value) in command.get_envs() {
        if let Some(value) = value { base.env(key, value); } else { base.env_remove(key); }
    }
    for key in std::env::vars_os().map(|(key, _)| key).chain(command.get_envs().map(|(key, _)| key.to_owned())) {
        if key.to_str().is_some_and(|key| key.starts_with("ADB_") || key.starts_with("ANDROID_")) {
            base.env_remove(key);
        }
    }
    for key in ["ADB_USB", "ADB_MDNS", "ADB_LOCAL_TRANSPORT_MAX_PORT"] { base.env(key, "0"); }
    for name in ["adb-home", "adb-temporary", "adb-home/.android"] {
        let path = root.join(name);
        std::fs::create_dir_all(&path)?;
        if path.canonicalize()? != path { return Err(io::Error::other("ADB server scratch changed")); }
    }
    let mut keygen = adb_client(&base, root)?;
    keygen.arg("keygen").arg(root.join("adb-home/.android/adbkey"));
    let mut server = adb_client(&base, root)?;
    server.args(command.get_args());
    Ok((keygen, server))
}

#[cfg(test)]
#[path = "emulator_confinement_tests.rs"]
mod tests;
