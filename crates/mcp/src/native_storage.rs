//! Explicit metadata-only inventory. Never an admission quota, retention decision or atomic snapshot.
use serde::Serialize;
use std::{fs, io, os::unix::fs::MetadataExt, path::Path, time::Instant};

#[derive(Default, Serialize)]
pub struct Files {
    count: u64,
    logical_bytes: u64,
    allocated_bytes: u64,
}

#[derive(Serialize)]
pub struct Usage {
    scope: &'static str,
    atomic: bool,
    filesystem_device: u64,
    filesystem_boundaries_skipped: u64,
    allocated_bytes_measured: bool,
    entries_scanned: u32,
    max_entries: u32,
    limit_reached: bool,
    unreadable_entries: u64,
    symlinks_skipped: u64,
    special_entries_skipped: u64,
    regular_files: Files,
    diagnostic_stream_files: Files,
    emulator_temporary_files: Files,
    owned_volume_image_files: Files,
    xctest_result_files: Files,
    png_files: Files,
    apk_files: Files,
    other_files: Files,
    elapsed_ms: u128,
    declared_simulator_image: ImageObservation,
    declared_android_scratch_image: ImageObservation,
    external_session: SessionObservation,
}

/// Separate observation; scopes can overlap, so consumers must not sum them blindly.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum SessionObservation {
    NotDeclared,
    Absent { path: std::path::PathBuf },
    EntryBudgetExhausted { path: std::path::PathBuf },
    Measured { path: std::path::PathBuf, usage: Box<Usage> },
    Unavailable { error: String },
}

fn session_observation(root: &Path, remaining: u32) -> SessionObservation {
    let observe = || -> io::Result<SessionObservation> {
        let path = frontcap_recorder::native::session_directory(root)?;
        if path == root.join("native-session") { return Ok(SessionObservation::NotDeclared); }
        match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {},
            Ok(_) => return Err(io::Error::other("external session is not a real directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SessionObservation::Absent { path }),
            Err(error) => return Err(error),
        }
        if remaining == 0 { return Ok(SessionObservation::EntryBudgetExhausted { path }); }
        let usage = measure_directory(&path, remaining, "external_session_directory_same_filesystem")?;
        Ok(SessionObservation::Measured { path, usage: Box::new(usage) })
    };
    observe().unwrap_or_else(|error| SessionObservation::Unavailable { error: error.to_string() })
}

/// Separate path observation, never added to runtime totals (the scopes can overlap).
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ImageObservation {
    NotDeclared,
    Absent { path: std::path::PathBuf },
    Measured { path: std::path::PathBuf, logical_bytes: u64, allocated_bytes: u64 },
    Unavailable { error: String },
}

fn image_observation(resolved: io::Result<Option<std::path::PathBuf>>) -> ImageObservation {
    let observe = || -> io::Result<ImageObservation> {
        let Some(path) = resolved? else {
            return Ok(ImageObservation::NotDeclared);
        };
        match path.symlink_metadata() {
            Ok(meta) if meta.is_file() => Ok(ImageObservation::Measured {
                path, logical_bytes: meta.len(), allocated_bytes: meta.blocks().saturating_mul(512),
            }),
            Ok(_) => Err(io::Error::other("declared image is not a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ImageObservation::Absent { path }),
            Err(error) => Err(error),
        }
    };
    observe().unwrap_or_else(|error| ImageObservation::Unavailable { error: error.to_string() })
}

pub fn measure(root: &Path, max_entries: u32) -> io::Result<Usage> {
    let started = Instant::now();
    let mut usage = measure_directory(root, max_entries, "runtime_directory_same_filesystem")?;
    usage.declared_simulator_image = image_observation(frontcap_recorder::native::declared_simulator_image(root));
    usage.declared_android_scratch_image = image_observation(frontcap_recorder::native::declared_android_scratch_image(root));
    usage.external_session = session_observation(root, max_entries - usage.entries_scanned);
    usage.elapsed_ms = started.elapsed().as_millis();
    Ok(usage)
}

fn measure_directory(root: &Path, max_entries: u32, scope: &'static str) -> io::Result<Usage> {
    if max_entries == 0 || max_entries > 100_000 {
        return Err(io::Error::other("invalid storage inventory bound"));
    }
    let meta = root.symlink_metadata()?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(io::Error::other(
            "storage inventory root must be a directory, not a symbolic link",
        ));
    }
    let started = Instant::now();
    let mut usage = Usage {
        scope,
        atomic: false,
        filesystem_device: meta.dev(),
        filesystem_boundaries_skipped: 0,
        allocated_bytes_measured: true,
        entries_scanned: 0,
        max_entries,
        limit_reached: false,
        unreadable_entries: 0,
        symlinks_skipped: 0,
        special_entries_skipped: 0,
        regular_files: Files::default(),
        diagnostic_stream_files: Files::default(),
        emulator_temporary_files: Files::default(),
        owned_volume_image_files: Files::default(),
        xctest_result_files: Files::default(),
        png_files: Files::default(),
        apk_files: Files::default(),
        other_files: Files::default(),
        elapsed_ms: 0,
        declared_simulator_image: ImageObservation::NotDeclared,
        declared_android_scratch_image: ImageObservation::NotDeclared,
        external_session: SessionObservation::NotDeclared,
    };
    visit(root, root, 0, &mut usage);
    usage.elapsed_ms = started.elapsed().as_millis();
    Ok(usage)
}

fn visit(base: &Path, root: &Path, depth: usize, usage: &mut Usage) {
    if depth > 64 {
        usage.limit_reached = true;
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => {
            usage.unreadable_entries += 1;
            return;
        }
    };
    for entry in entries {
        if usage.entries_scanned == usage.max_entries {
            usage.limit_reached = true;
            break;
        }
        usage.entries_scanned += 1;
        let metadata = entry.and_then(|entry| Ok((entry.path(), entry.metadata()?)));
        let (path, metadata) = match metadata {
            Ok(value) => value,
            Err(_) => {
                usage.unreadable_entries += 1;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            usage.symlinks_skipped += 1;
            continue;
        }
        // The image's allocated blocks already belong to this filesystem. Descending into
        // its mounted contents would count storage twice and escape the declared root scope.
        if metadata.dev() != usage.filesystem_device {
            usage.filesystem_boundaries_skipped += 1;
            continue;
        }
        if metadata.is_dir() {
            visit(base, &path, depth + 1, usage);
            continue;
        }
        if !metadata.is_file() {
            usage.special_entries_skipped += 1;
            continue;
        }
        let size = metadata.len();
        let allocated = metadata.blocks().saturating_mul(512);
        usage.regular_files.count += 1;
        usage.regular_files.logical_bytes += size;
        usage.regular_files.allocated_bytes += allocated;
        // This is a path-based category, not evidence of ownership or permission to delete.
        let relative = path.strip_prefix(base).map(|relative| {
            if usage.scope == "external_session_directory_same_filesystem" {
                Path::new("native-session").join(relative)
            } else { relative.to_owned() }
        });
        let temporary = relative.as_ref().is_ok_and(|relative| {
            relative.starts_with("native-session/device/emulator-temporary")
                || relative.starts_with("native-session/target/emulator-temporary")
        });
        let owned_image = relative.as_ref().is_ok_and(|relative| relative.as_path() == Path::new(
            "native-session/target-resources/scratch-volume/worker/volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session/session/worker/resource-group/scratch-volume/worker/volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session/target/simulator/data-volume/worker/volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session/session/worker/simulator-data-volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session/session/worker/simulator-data-volume/worker/volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session-volume/worker/volume/bounded.dmg")
            || relative.as_path() == Path::new("native-session-volume/worker/volume/bounded.dmg.gz"));
        let xctest_result = relative.as_ref().is_ok_and(|relative| {
            relative.starts_with("native-session")
                && relative.parent().is_some_and(|parent| {
                    parent.components().any(|part| part.as_os_str() == "test.xcresult")
                })
        });
        let category = if temporary {
            &mut usage.emulator_temporary_files
        } else if owned_image {
            &mut usage.owned_volume_image_files
        } else if xctest_result {
            &mut usage.xctest_result_files
        } else {
            match path.file_name().and_then(|s| s.to_str()) {
                Some("stdout.bin" | "stderr.bin" | "stdout.bin.gz" | "stderr.bin.gz") => {
                    &mut usage.diagnostic_stream_files
                }
                _ => match path.extension().and_then(|s| s.to_str()) {
                    Some("png") => &mut usage.png_files,
                    Some("apk") => &mut usage.apk_files,
                    _ => &mut usage.other_files,
                },
            }
        };
        category.count += 1;
        category.logical_bytes += size;
        category.allocated_bytes += allocated;
    }
}

#[cfg(test)]
#[path = "native_storage_tests.rs"]
mod tests;
