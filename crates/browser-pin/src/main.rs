use std::path::PathBuf;
use std::time::Duration;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

fn curl_json(url: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("curl").args(["--fail", "--silent", url]).output()?;
    if !output.status.success() {
        return Err(format!("curl {url} failed: {}", output.status).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo_root();
    let pin = browser_pin::load_pin(&root)?;
    let platform = browser_pin::current_platform()?;
    match args.first().map(String::as_str) {
        Some("install") => {
            let bin = browser_pin::install(&root, &pin, platform)?;
            println!("{}", bin.display());
        }
        Some("path") => println!("{}", browser_pin::installed_binary(&root, &pin, platform)
            .unwrap_or_else(|| browser_pin::binary_path(&root, &pin.version, platform)).display()),
        Some("dump-protocol") => {
            let out = args.get(1).ok_or("usage: dump-protocol <out.json>")?;
            // The protocol of the engine frontcap runs: the patched build's (its commands included) when built.
            let bin = match browser_pin::engine::patched_binary(&root, platform)? {
                Some((bin, _)) => bin,
                // A patch set's commands are in the protocol only when its engine is built.
                None if !browser_pin::engine::patches(&root)?.is_empty() => {
                    return Err(format!("the patch set {} is not built: `browser-pin engine build --src <chromium/src>` first", browser_pin::engine::patchset_id(&root)?).into());
                }
                None => browser_pin::install(&root, &pin, platform)?,
            };
            let launched = browser_pin::launch(&bin, &[], Duration::from_secs(20))?;
            let protocol = curl_json(&format!("http://127.0.0.1:{}/json/protocol", launched.port))?;
            std::fs::write(out, serde_json::to_string_pretty(&protocol)? + "\n")?;
            let version = curl_json(&format!("http://127.0.0.1:{}/json/version", launched.port))?;
            let patchset = browser_pin::engine::patched_binary(&root, platform)?.map(|(_, m)| m.patchset);
            let meta = serde_json::json!({ "browser": version["Browser"], "patchset": patchset });
            let meta_path = std::path::Path::new(out).with_file_name("protocol.meta.json");
            std::fs::write(meta_path, serde_json::to_string_pretty(&meta)? + "\n")?;
        }
        Some("engine") => match (args.get(1).map(String::as_str), args.get(2).map(String::as_str), args.get(3)) {
            (Some("build"), Some("--src"), Some(src)) => {
                // `--platform` builds another platform's engine (a cross build) from a checkout synced for it.
                let target = match (args.get(4).map(String::as_str), args.get(5)) {
                    (None, _) => platform,
                    (Some("--platform"), Some(target)) => target.as_str(),
                    _ => return Err("usage: browser-pin engine build --src <chromium/src> [--platform <platform>]".into()),
                };
                let bin = browser_pin::engine::build(&root, std::path::Path::new(src), &pin.version, target)?;
                println!("{}", bin.display());
            }
            (Some("path"), _, _) => match browser_pin::engine::patched_binary(&root, platform)? {
                Some((bin, _)) => println!("{}", bin.display()),
                None => return Err(format!("no patched build of patch set {} for {platform}; run `browser-pin engine build --src <chromium/src>`", browser_pin::engine::patchset_id(&root)?).into()),
            },
            (Some("id"), _, _) => println!("{}", browser_pin::engine::patchset_id(&root)?),
            // Once per host that restricts user namespaces (and after each engine build), as root: the
            // current patched engine and the pinned browser are copied where only root can change them,
            // and a profile lets copies there start their sandbox. `--print` shows the profile only.
            (Some("allow-sandbox"), print, _) => {
                if print == Some("--print") {
                    print!("{}", browser_pin::sandbox::profile());
                    return Ok(());
                }
                if !browser_pin::sandbox::userns_restricted() {
                    println!("this host does not restrict user namespaces: the engine's sandbox needs nothing");
                    return Ok(());
                }
                let mut installed = Vec::new();
                let id = browser_pin::engine::patchset_id(&root)?;
                let from = browser_pin::engine::patched_dir(&root, &id, platform);
                if let Ok(text) = std::fs::read_to_string(from.join(browser_pin::engine::MANIFEST)) {
                    let manifest: browser_pin::engine::Manifest = serde_json::from_str(&text)?;
                    let mut files: Vec<String> = manifest.files.keys().cloned().collect();
                    files.push(browser_pin::engine::MANIFEST.into());
                    let to = browser_pin::sandbox::installed_dir(&id, platform);
                    browser_pin::sandbox::install_engine(&from, &files, &to)?;
                    // The engine resolves to the copy only when it matches the build it was copied from.
                    let (binary, _) = browser_pin::engine::patched_binary(&root, platform)?.ok_or("the patched build no longer verifies")?;
                    if binary != to.join(&manifest.binary) {
                        return Err(format!("the copy in {} does not match the build it was copied from", to.display()).into());
                    }
                    installed.push(binary);
                }
                let from = root.join("browsers").join(&pin.version).join(platform);
                if browser_pin::installed_binary(&root, &pin, platform).is_some() {
                    let to = browser_pin::sandbox::installed_base_dir(&pin.version, platform);
                    browser_pin::sandbox::install_engine(&from, &browser_pin::sandbox::files_under(&from)?, &to)?;
                    let binary = browser_pin::installed_binary(&root, &pin, platform).ok_or("the pinned browser's copy is incomplete")?;
                    if !binary.starts_with(&to) {
                        return Err(format!("the copy in {} is not complete", to.display()).into());
                    }
                    installed.push(binary);
                }
                if installed.is_empty() {
                    return Err(format!("nothing to install for {platform}: build the patched engine or `browser-pin install` the pinned browser first").into());
                }
                for binary in &installed {
                    browser_pin::sandbox::check_root_owned(binary)?;
                }
                browser_pin::sandbox::install_profile()?;
                for binary in installed {
                    println!("{}", binary.display());
                }
            }
            // After an in-tree build of the patched checkout: the next `build` keeps that build's work for
            // files whose content it compiled.
            (Some("record-tree"), Some("--src"), Some(src)) => browser_pin::engine::record_tree(std::path::Path::new(src))?,
            _ => return Err("usage: browser-pin engine build --src <chromium/src> [--platform <platform>] | record-tree --src <chromium/src> | path | id | allow-sandbox [--print]".into()),
        },
        _ => return Err("usage: browser-pin install | path | dump-protocol <out.json> | engine ...".into()),
    }
    Ok(())
}
