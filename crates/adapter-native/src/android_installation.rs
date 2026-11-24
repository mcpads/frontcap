//! Bounded Android SDK readback through a caller's already owned ADB command path.
use frontcap_core::native::{
    android_capture::valid_app,
    android_lifecycle::{Installation, Process},
};
use std::{io, path::PathBuf};

/// Device paths later sent to `adb shell` must remain literal arguments after its joining step.
fn device_path(raw: &str) -> io::Result<PathBuf> {
    let value = raw.trim();
    if !value.starts_with('/')
        || value
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && !b"/._-+=@~".contains(&b))
        || value.split('/').any(|p| p == "." || p == "..")
    {
        return Err(io::Error::other("invalid literal Android device path"));
    }
    Ok(value.into())
}

pub(crate) fn installation(
    package: &str,
    activity: &str,
    apk_sha256: &str,
    mut command: impl FnMut(&[&str]) -> io::Result<String>,
) -> io::Result<Installation> {
    if !valid_app(package, activity) {
        return Err(io::Error::other("invalid Android application"));
    }
    let paths = command(&["shell", "pm", "path", package])?;
    let rows: Vec<_> = paths.lines().collect();
    if rows.len() != 1 {
        return Err(io::Error::other(
            "same-install replacement requires one installed APK",
        ));
    }
    let apk = device_path(
        rows[0]
            .strip_prefix("package:")
            .ok_or_else(|| io::Error::other("invalid Android APK inventory"))?,
    )?;
    let apk_text = apk
        .to_str()
        .ok_or_else(|| io::Error::other("invalid APK path"))?;
    let digest = command(&["shell", "sha256sum", apk_text])?;
    let fields: Vec<_> = digest.split_whitespace().collect();
    if fields.len() != 2 || fields[1] != apk_text {
        return Err(io::Error::other("installed APK digest has another path"));
    }
    let data = device_path(&command(&["shell", "run-as", package, "pwd"])?)?;
    let stat = command(&["shell", "run-as", package, "stat", "-c", "'%d %i'", "."])?;
    let numbers: Vec<u64> = stat
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    if numbers.len() != 2 {
        return Err(io::Error::other("invalid Android data-container identity"));
    }
    let value = Installation {
        package: package.into(),
        apk,
        apk_sha256: fields[0].into(),
        data,
        data_device: numbers[0],
        data_inode: numbers[1],
    };
    value
        .validate(package, apk_sha256)
        .map_err(io::Error::other)?;
    Ok(value)
}

pub(crate) fn process(
    package: &str,
    activity: &str,
    pid: u32,
    mut command: impl FnMut(&[&str]) -> io::Result<String>,
) -> io::Result<Process> {
    if !valid_app(package, activity) || pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::other("invalid Android process owner"));
    }
    let found = command(&["shell", "pidof", package])?;
    if found.trim() != pid.to_string() {
        return Err(io::Error::other("Android package process differs"));
    }
    let boot_id = command(&["shell", "cat", "/proc/sys/kernel/random/boot_id"])?
        .trim()
        .to_owned();
    let path = format!("/proc/{pid}/stat");
    let stat = command(&["shell", "run-as", package, "cat", &path])?;
    let start_ticks = start_ticks(&stat, pid)?;
    let path = format!("/proc/{pid}/cmdline");
    let name = command(&["shell", "run-as", package, "cat", &path])?;
    let name = name.trim_end_matches('\0');
    let value = Process {
        pid,
        boot_id,
        start_ticks,
        command: name.into(),
    };
    value.validate(package).map_err(io::Error::other)?;
    // The process must still be the same after collecting its command and birth evidence.
    let path = format!("/proc/{pid}/stat");
    if start_ticks
        != self::start_ticks(&command(&["shell", "run-as", package, "cat", &path])?, pid)?
    {
        return Err(io::Error::other(
            "Android process changed during inspection",
        ));
    }
    Ok(value)
}
fn start_ticks(raw: &str, pid: u32) -> io::Result<u64> {
    let (prefix, rest) = raw
        .trim()
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid Android process stat"))?;
    if !prefix.starts_with(&format!("{pid} (")) {
        return Err(io::Error::other(
            "Android process stat belongs to another PID",
        ));
    }
    let fields: Vec<_> = rest.split_whitespace().collect();
    // Field 3 follows the comm delimiter; the birth tick is field 22.
    if fields.len() < 20 || matches!(fields[0], "Z" | "X" | "x") {
        return Err(io::Error::other("Android process is not live"));
    }
    fields[19].parse().map_err(io::Error::other)
}

#[cfg(test)]
#[path = "android_installation_tests.rs"]
mod tests;
