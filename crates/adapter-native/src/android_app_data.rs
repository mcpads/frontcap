//! Declared files applied through a caller's live, owned Android command channel.
//! This module neither acquires a device nor adopts a saved process or installation.
use crate::app_data::Prepared;
use frontcap_core::write_budget::{Class, WriteBudget};
use std::{io, path::Path};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
fn run_as(package: &str, script: &str) -> Vec<String> {
    [
        "shell".into(),
        "run-as".into(),
        package.into(),
        "sh".into(),
        "-c".into(),
        quote(script),
    ]
    .into()
}

/// Every command goes through the device owner's recorded, supervised exchange.
pub(crate) fn apply_files(
    prepared: &Prepared,
    source: &Path,
    package: &str,
    temporary: &str,
    command: impl FnMut(&[&str]) -> io::Result<String>,
) -> io::Result<()> {
    apply_files_with_budget(prepared, source, package, temporary, WriteBudget::from_environment()?.as_ref(), command)
}

pub(crate) fn apply_files_with_budget(
    prepared: &Prepared,
    source: &Path,
    package: &str,
    temporary: &str,
    budget: Option<&WriteBudget>,
    mut command: impl FnMut(&[&str]) -> io::Result<String>,
) -> io::Result<()> {
    let inventory = &prepared.recipe.inventory;
    inventory.verify(source)?;
    let path = |name: &str| -> io::Result<String> {
        if name.chars().any(char::is_control) || name.contains('\\') {
            return Err(io::Error::other(
                "Android app-data path cannot contain control characters or backslashes",
            ));
        }
        Ok(quote(&format!("./{name}")))
    };
    let mut preflight = String::from("set -eu; ");
    for name in &inventory.directories {
        let name = path(name)?;
        preflight.push_str(&format!(
            "test ! -L {name}; if test -e {name}; then test -d {name}; fi; "
        ));
    }
    for name in inventory.files.keys() {
        let name = path(name)?;
        preflight.push_str(&format!("test ! -e {name}; test ! -L {name}; "));
    }
    for name in inventory.files.keys() {
        if source.join(name).to_str().is_none() {
            return Err(io::Error::other("non-UTF8 input path"));
        }
    }
    // Each declared byte is written once to ADB staging and once to the app's data.
    // Reserve both copies before any SDK command. This is conservative logical admission,
    // not actual written bytes or filesystem allocation; failures never refund it.
    let payload = inventory.files.values().try_fold(0_u64, |total, file| total.checked_add(file.bytes)
        .ok_or_else(|| io::Error::other("Android app-data inventory size overflow")))?;
    let writes = payload.checked_mul(2)
        .ok_or_else(|| io::Error::other("Android app-data staging/application size overflow"))?;
    if let Some(budget) = budget { budget.charge(Class::Ordinary, writes)?; }
    let args = run_as(package, &preflight);
    command(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    command(&["shell", "mkdir", temporary])?;
    for name in &inventory.directories {
        let args = run_as(package, &format!("mkdir -p -- {}", path(name)?));
        command(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    }
    for (number, (name, expected)) in inventory.files.iter().enumerate() {
        let uploaded = format!("{temporary}/{number}");
        let local = source.join(name);
        command(&[
            "push",
            local
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 input path"))?,
            &uploaded,
        ])?;
        let mode = if expected.executable { "700" } else { "600" };
        let write = format!(
            "set -eC; cat > {}; chmod {mode} {}",
            path(name)?,
            path(name)?
        );
        // The shell reads its staging file; the app UID writes from stdin. No cross-UID file read.
        let pipe = format!(
            "set -o pipefail; cat {uploaded} | run-as {package} sh -c {}",
            quote(&write)
        );
        command(&["shell", "sh", "-c", &quote(&pipe)])?;
        let name = format!("./{name}");
        let digest = command(&["shell", "run-as", package, "sha256sum", &quote(&name)])?;
        if digest.trim_end_matches(['\r', '\n']) != format!("{}  {name}", expected.sha256) {
            return Err(io::Error::other("installed app-data digest differs"));
        }
        let stat = command(&[
            "shell",
            "run-as",
            package,
            "stat",
            "-c",
            "'%s %a'",
            &quote(&name),
        ])?;
        if stat.trim() != format!("{} {mode}", expected.bytes) {
            return Err(io::Error::other("installed app-data size or mode differs"));
        }
    }
    command(&["shell", "run-as", package, "sync"])?;
    command(&["shell", "rm", "-r", temporary])?;
    Ok(())
}

/// Validate a NAME-only process inventory captured through the owner's command channel.
pub(crate) fn require_process_absent(package: &str, output: &str) -> io::Result<()> {
    let mut lines = output.lines();
    if lines.next().map(str::trim) != Some("NAME")
        || lines.any(|line| {
            let name = line.trim();
            name == package || name.starts_with(&format!("{package}:"))
        }) {
        return Err(io::Error::other("initial app data requires an unstarted package"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "android_app_data_tests.rs"]
mod tests;
