//! Verify source View/screen capture artifacts without contacting or acting on a device.
use frontcap_core::{
    canonical::sha256_bytes_hex,
    native::{android_capture::{Capture,Collection,Unavailable,UnavailableReason}, android_views::Observation},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(serde::Serialize)]
pub struct Owner<'a> {
    pub package: &'a str,
    pub activity: &'a str,
    pub generation: &'a str,
    pub pid: u32,
    pub source_sha256: &'a str,
}
fn read(root: &Path, name: &str) -> io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(name))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("capture artifact is not a regular file"));
    }
    let mut raw = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut raw)?;
    if raw.len() > 16 * 1024 * 1024 {
        return Err(io::Error::other("capture artifact exceeds size budget"));
    }
    Ok(raw)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRequest {
    request: String,
    generation: String,
    pid: u32,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClockState {
    Ready,
    Ok,
}
// Deliberately project only owner/time/state: app-owned clock diagnostics are not View evidence.
#[derive(Deserialize)]
struct ClockStamp {
    generation: String,
    pid: u32,
    now_ns: u64,
    status: ClockState,
}
fn foreground(raw: &[u8], owner: &Owner<'_>) -> io::Result<bool> {
    let text = std::str::from_utf8(raw).map_err(io::Error::other)?;
    let lines: Vec<_> = text
        .lines()
        .filter(|s| s.trim_start().starts_with("mCurrentFocus="))
        .collect();
    if lines.len() != 1 {
        return Ok(false);
    }
    let names: Vec<_> = lines[0]
        .split_whitespace()
        .filter_map(|s| s.trim_matches(['{', '}']).split_once('/'))
        .collect();
    Ok(names.len() == 1
        && names[0].0 == owner.package
        && (names[0].1 == owner.activity
            || names[0].1.starts_with('.')
                && format!("{}{}", owner.package, names[0].1) == owner.activity))
}
pub fn verify(root: &Path, owner: &Owner<'_>, manifest_sha256: &str) -> io::Result<Capture> {
    verify_evidence(root,owner,manifest_sha256,false)
}
pub fn verify_collection(root: &Path, owner: &Owner<'_>, manifest_sha256: &str, source_changed: bool) -> io::Result<Collection> {
    let capture=verify_evidence(root,owner,manifest_sha256,source_changed)?;
    Ok(if source_changed {Collection::Unavailable(Unavailable {
        capture_id:capture.capture_id,generation:capture.generation,pid:capture.pid,now_ns:capture.now_ns,
        started_monotonic_ns:capture.started_monotonic_ns,finished_monotonic_ns:capture.finished_monotonic_ns,
        manifest_sha256:manifest_sha256.into(),reason:UnavailableReason::SourceChanged,
    })} else {Collection::Observed(capture)})
}
fn verify_evidence(root: &Path, owner: &Owner<'_>, manifest_sha256: &str, source_changed: bool) -> io::Result<Capture> {
    if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(io::Error::other("linked capture directory"));
    }
    let raw = read(root, "manifest.json")?;
    if sha256_bytes_hex(&raw) != manifest_sha256 {
        return Err(io::Error::other("capture manifest digest changed"));
    }
    let capture: Capture = serde_json::from_slice(&raw)?;
    capture.validate().map_err(io::Error::other)?;
    if capture.package != owner.package
        || capture.activity != owner.activity
        || capture.generation != owner.generation
        || capture.pid != owner.pid
        || capture.source_sha256 != owner.source_sha256
    {
        return Err(io::Error::other(
            "capture belongs to a different app owner or source",
        ));
    }
    let mut files = BTreeMap::new();
    for entry in &capture.artifacts {
        let raw = read(root, &entry.path)?;
        if raw.len() as u64 != entry.bytes || sha256_bytes_hex(&raw) != entry.sha256 {
            return Err(io::Error::other("capture artifact changed"));
        }
        files.insert(entry.path.as_str(), raw);
    }
    for (side, request) in [
        ("before", &capture.before_request),
        ("after", &capture.after_request),
    ] {
        let q: SourceRequest =
            serde_json::from_slice(&files[format!("views-{side}-request.json").as_str()])?;
        if q.request != *request || q.generation != owner.generation || q.pid != owner.pid {
            return Err(io::Error::other("View request differs from capture owner"));
        }
        let observation: Observation =
            serde_json::from_slice(&files[format!("views-{side}.json").as_str()])?;
        observation
            .validate(request, owner.generation, owner.pid)
            .map_err(io::Error::other)?;
        if !matches!(observation,Observation::Observed { now_ns,catalog,.. } if now_ns==capture.now_ns && (catalog!=capture.catalog)==(source_changed && side=="after"))
        {
            return Err(io::Error::other(
                "source catalog changed or unavailable during capture",
            ));
        }
        let stamp: ClockStamp =
            serde_json::from_slice(&files[format!("clock-{side}.json").as_str()])?;
        let _terminal = stamp.status;
        if stamp.generation != owner.generation
            || stamp.pid != owner.pid
            || stamp.now_ns != capture.now_ns
        {
            return Err(io::Error::other(
                "clock stamp differs from source observation",
            ));
        }
        if std::str::from_utf8(&files[format!("pid-{side}.txt").as_str()])
            .map_err(io::Error::other)?
            .split_whitespace()
            .collect::<Vec<_>>()
            != [owner.pid.to_string()]
        {
            return Err(io::Error::other("app process changed during capture"));
        }
        if !foreground(&files[format!("window-{side}.txt").as_str()], owner)? {
            return Err(io::Error::other("capture Activity is not foreground"));
        }
        if files[format!("views-{side}-submission.txt").as_str()].trim_ascii()
            != b"Result: Bundle[{submitted=true}]"
        {
            return Err(io::Error::other("source read submission is unconfirmed"));
        }
    }
    if files["clock-before.json"] != files["clock-after.json"] {
        return Err(io::Error::other("clock receipt changed during capture"));
    }
    let mut decoder = png::Decoder::new(io::Cursor::new(&files["screen.png"]));
    decoder.set_limits(png::Limits {
        bytes: 64 * 1024 * 1024,
    });
    let mut reader = decoder.read_info().map_err(io::Error::other)?;
    if [reader.info().width, reader.info().height] != capture.size
        || reader.info().animation_control.is_some()
    {
        return Err(io::Error::other(
            "capture PNG dimensions or animation differ",
        ));
    }
    let size = reader
        .output_buffer_size()
        .filter(|n| *n <= 64 * 1024 * 1024)
        .ok_or_else(|| io::Error::other("PNG decode exceeds budget"))?;
    reader
        .next_frame(&mut vec![0; size])
        .map_err(io::Error::other)?;
    reader.finish().map_err(io::Error::other)?;
    Ok(capture)
}
#[cfg(test)]
#[path = "android_capture_tests.rs"]
pub(crate) mod tests;
