//! Read published recording evidence without contacting or restarting its browser.
use crate::tools::ToolError;
use frontcap_core::observation::RecordedFrame;
use std::path::{Component, Path, PathBuf};

#[derive(serde::Serialize)]
pub(crate) struct RecordedFrames {
    pub frames: Vec<RecordedFrame>,
    /// The final index entry is incomplete, so it is not included. May be a concurrent append or
    /// evidence interrupted while being written; never claim it describes a retained frame.
    pub incomplete_tail: bool,
}

fn error(message: impl Into<String>) -> ToolError {
    ToolError::new("invalid_recording", message, "inspect the generation's bundle with debug bundle")
}

pub(crate) fn retained_path(bundle: &Path, file: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(file);
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) || !relative.starts_with("frames") {
        return Err(error("the recording index names a file outside frames/"));
    }
    let root = bundle.canonicalize().map_err(|e| error(e.to_string()))?;
    let path = root.join(relative).canonicalize().map_err(|e| error(format!("{file}: {e}")))?;
    if !path.starts_with(root.join("frames")) {
        return Err(error("the recorded file resolves outside frames/"));
    }
    Ok(path)
}

pub(crate) fn frames(bundle: &Path, since: Option<u64>, until: Option<u64>) -> Result<RecordedFrames, ToolError> {
    if since.zip(until).is_some_and(|(s, u)| s > u) {
        return Err(ToolError::new("invalid_arguments", "since_us exceeds until_us", "use an inclusive time range in increasing order"));
    }
    let index = bundle.join("frames/frames.jsonl");
    match std::fs::symlink_metadata(&index) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RecordedFrames { frames: Vec::new(), incomplete_tail: false }),
        Err(e) => return Err(error(e.to_string())),
        Ok(_) => {}
    }
    let path = retained_path(bundle, "frames/frames.jsonl")?;
    let bytes = std::fs::read(path).map_err(|e| error(e.to_string()))?;
    // The writer appends one newline-terminated entry only after publishing its image. A concurrent
    // read may see a partial last entry, including a partial UTF-8 character; it is not published yet.
    let mut frames = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n').filter(|line| line.ends_with(b"\n")) {
        let frame: RecordedFrame = serde_json::from_slice(line).map_err(|e| error(format!("invalid frame index entry: {e}")))?;
        if since.is_none_or(|s| frame.t_us.0 >= s) && until.is_none_or(|u| frame.t_us.0 <= u) {
            frames.push(frame);
        }
    }
    Ok(RecordedFrames { frames, incomplete_tail: !bytes.is_empty() && !bytes.ends_with(b"\n") })
}

pub(crate) fn image(bundle: &Path, file: &str) -> Result<(RecordedFrame, Vec<u8>), ToolError> {
    let frame = frames(bundle, None, None)?.frames.into_iter().find(|frame| frame.file == file)
        .ok_or_else(|| ToolError::new("not_found", "no recorded frame has that file identifier", "use a file returned by capture recorded_frames or record_stop"))?;
    let path = retained_path(bundle, &frame.file)?;
    let png = std::fs::read(path).map_err(|e| error(e.to_string()))?;
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(error("the recorded image is not a PNG"));
    }
    Ok((frame, png))
}

pub(crate) fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), ToolError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| error(e.to_string()))?;
    let (w, h) = (reader.info().width as usize, reader.info().height as usize);
    if w == 0 || h == 0 || w.checked_mul(h).is_none_or(|n| n > 16_777_216) {
        return Err(error("recorded image exceeds the 16-megapixel decode bound"));
    }
    let mut bytes = vec![0; reader.output_buffer_size().ok_or_else(|| error("invalid image dimensions"))?];
    let info = reader.next_frame(&mut bytes).map_err(|e| error(e.to_string()))?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in bytes[..info.buffer_size()].chunks_exact(info.color_type.samples()) {
        let pixel = match info.color_type {
            png::ColorType::Rgb => [p[0], p[1], p[2], 255],
            png::ColorType::Rgba => [p[0], p[1], p[2], p[3]],
            png::ColorType::Grayscale => [p[0], p[0], p[0], 255],
            png::ColorType::GrayscaleAlpha => [p[0], p[0], p[0], p[1]],
            png::ColorType::Indexed => return Err(error("unexpanded indexed PNG")),
        };
        rgba.extend_from_slice(&pixel);
    }
    Ok((w, h, rgba))
}


#[cfg(test)]
#[path = "recordings_tests.rs"]
mod tests;
