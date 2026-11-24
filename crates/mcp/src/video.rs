//! Offline variable-frame-rate MP4. The final repeated image is an explicit duration marker, not
//! a fabricated browser presentation. Source images and the original recording index stay intact.
use crate::{derived, recordings, tools::ToolError};
use frontcap_core::canonical::{canonical_of, sha256_bytes_hex};
use serde_json::{Value,json};
use std::{path::{Path,PathBuf},process::Stdio,time::Duration};

fn error(message: impl Into<String>) -> ToolError {
    ToolError::new("video_failed",message,"inspect the retained frames and ffmpeg availability; no video is published on failure")
}

struct Prepared {
    work: tempfile::TempDir,
    sources: Vec<Value>,
    start: u64,
    last: u64,
    duration: u64,
    dimensions: [u32;2],
    incomplete: bool,
}

fn seconds(us: u64) -> String { format!("{}.{:06}",us/1_000_000,us%1_000_000) }

fn prepare(bundle: &Path, since: Option<u64>, until: Option<u64>, tail_ms: u64) -> Result<Prepared,ToolError> {
    if !(1..=10000).contains(&tail_ms) { return Err(ToolError::new("invalid_arguments","tail_ms must be 1–10000","choose an explicit final video hold")); }
    let evidence=recordings::frames(bundle,since,until)?;
    if evidence.frames.is_empty() { return Err(ToolError::new("not_found","no recorded frames in this range","record frames or choose another range")); }
    if evidence.frames.len()>512 { return Err(ToolError::new("result_too_large","video scans at most 512 frames","narrow since_us/until_us")); }
    let work=tempfile::tempdir().map_err(|e|error(e.to_string()))?;
    let start=evidence.frames[0].t_us.0;
    let last=evidence.frames.last().unwrap().t_us.0;
    let duration=last.checked_sub(start).and_then(|d|d.checked_add(tail_ms*1000)).filter(|d|*d<=3_600_000_000)
        .ok_or_else(||error("recording times are invalid or video exceeds one hour"))?;
    let mut sources=Vec::new();
    let mut dimensions=None;
    let mut total_bytes=0usize;
    for (i,frame) in evidence.frames.iter().enumerate() {
        if i>0 && frame.t_us<evidence.frames[i-1].t_us { return Err(error("recording index moves backward in virtual time")); }
        let path=recordings::retained_path(bundle,&frame.file)?;
        let len=std::fs::metadata(&path).map_err(|e|error(e.to_string()))?.len();
        if len>256*1024*1024 { return Err(error("source image exceeds 256 MiB")); }
        let bytes=std::fs::read(path).map_err(|e|error(e.to_string()))?;
        total_bytes=total_bytes.saturating_add(bytes.len());
        if total_bytes>256*1024*1024 { return Err(error("source images exceed 256 MiB; narrow the time range")); }
        let (w,h,_)=recordings::decode_png(&bytes)?;
        let size=[w as u32,h as u32];
        if dimensions.is_some_and(|d|d!=size) { return Err(error("recorded image dimensions change; derive each viewport interval separately")); }
        dimensions=Some(size);
        std::fs::write(work.path().join(format!("{i}.png")),&bytes).map_err(|e|error(e.to_string()))?;
        // Zero-duration earlier presentations remain in provenance, but only the last can have a
        // positive playback interval at that virtual instant.
        let encoded=evidence.frames.get(i+1).is_none_or(|next|next.t_us!=frame.t_us);
        sources.push(json!({"source":frame,"sha256":sha256_bytes_hex(&bytes),"encoded":encoded,"offset_us":frame.t_us.0-start}));
    }
    let chosen:Vec<_>=sources.iter().enumerate().filter(|(_,s)|s["encoded"]==true).collect();
    let mut concat=String::from("ffconcat version 1.0\n");
    for (n,(i,source)) in chosen.iter().enumerate() {
        let offset=source["offset_us"].as_u64().unwrap();
        let next=chosen.get(n+1).map(|(_,s)|s["offset_us"].as_u64().unwrap()).unwrap_or(duration-1);
        concat.push_str(&format!("file {i}.png\noption framerate 1000000\nduration {}\n",seconds(next-offset)));
    }
    let last_index=chosen.last().unwrap().0;
    concat.push_str(&format!("file {last_index}.png\noption framerate 1000000\n"));
    std::fs::write(work.path().join("frames.txt"),concat).map_err(|e|error(e.to_string()))?;
    Ok(Prepared {work,sources,start,last,duration,dimensions:dimensions.unwrap(),incomplete:evidence.incomplete_tail})
}

pub(crate) async fn derive(bundle: PathBuf, since: Option<u64>, until: Option<u64>, tail_ms: u64) -> Result<Value,ToolError> {
    let source=bundle.clone();
    let prepared=tokio::task::spawn_blocking(move ||prepare(&source,since,until,tail_ms)).await.map_err(|e|error(e.to_string()))??;
    let version=tokio::time::timeout(Duration::from_secs(5),tokio::process::Command::new("ffmpeg").arg("-version").kill_on_drop(true).output()).await
        .map_err(|_|error("ffmpeg version check timed out"))?.map_err(|e|ToolError::new("unsupported_capability",format!("ffmpeg unavailable: {e}"),"install ffmpeg on the MCP host"))?;
    if !version.status.success() { return Err(error("ffmpeg version check failed")); }
    let encoder=String::from_utf8_lossy(&version.stdout).lines().next().unwrap_or_default().to_owned();
    let arguments=["-y","-nostdin","-v","error","-f","concat","-safe","0","-i","frames.txt",
        "-fps_mode","vfr","-enc_time_base","1:1000000","-c:v","libx264","-preset","ultrafast","-crf","18",
        "-vf","pad=ceil(iw/2)*2:ceil(ih/2)*2","-pix_fmt","yuv420p","-video_track_timescale","1000000","-movflags","+faststart","video.mp4"];
    let result=tokio::time::timeout(Duration::from_secs(120),tokio::process::Command::new("ffmpeg").args(arguments)
        .current_dir(prepared.work.path()).stdin(Stdio::null()).stdout(Stdio::null()).kill_on_drop(true).output()).await
        .map_err(|_|error("video encoding exceeded 120 seconds"))?.map_err(|e|error(e.to_string()))?;
    if !result.status.success() { return Err(error(format!("ffmpeg failed: {}",String::from_utf8_lossy(&result.stderr).chars().take(4096).collect::<String>()))); }
    let output_path=prepared.work.path().join("video.mp4");
    if tokio::fs::metadata(&output_path).await.map_err(|e|error(e.to_string()))?.len()>256*1024*1024 {
        return Err(error("encoded video exceeds 256 MiB; narrow the time range"));
    }
    let bytes=tokio::fs::read(output_path).await.map_err(|e|error(e.to_string()))?;
    if bytes.is_empty() { return Err(error("encoder produced an empty video")); }
    let hash=sha256_bytes_hex(&bytes);
    let file=format!("derived/video-{hash}.mp4");
    let mut metadata=json!({"file":file,"sha256":hash,"source_start_us":prepared.start,"source_last_us":prepared.last,
        "duration_us":prepared.duration,"synthetic_tail_us":tail_ms*1000,"terminal_marker_us":prepared.duration-1,
        "time_base":"1/1000000","source_pixel_size":prepared.dimensions,"incomplete_tail":prepared.incomplete,
        "sources":prepared.sources,"encoder":encoder,"arguments":arguments,
        "coverage":"recorded frames only; zero-duration predecessors remain in provenance but are not encoded; final hold and one-microsecond marker are synthetic; video is lossy and may pad odd dimensions"});
    let manifest=format!("derived/video-{}.json",sha256_bytes_hex(canonical_of(&metadata).as_bytes()));
    metadata["manifest"]=json!(manifest);
    let output=metadata.clone();
    tokio::task::spawn_blocking(move ||derived::publish(&bundle,"video",&file,&bytes,&manifest,&metadata)).await.map_err(|e|error(e.to_string()))??;
    Ok(output)
}

#[cfg(test)]
#[path="video_tests.rs"]
mod tests;
