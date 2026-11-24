//! Contact sheets derived from published recording evidence. Never consult the live generation.
use crate::{recordings, tools::ToolError};
use crate::recordings::decode_png as decode;
use frontcap_core::{canonical::sha256_bytes_hex, observation::RecordedFrame};
use serde::Serialize;
use std::path::Path;

const WIDTH: usize = 320;
const HEIGHT: usize = 200;
const LABEL: usize = 24;
const MAX_SCAN: usize = 512;

#[derive(Serialize)]
pub(crate) struct Sheet {
    pub file: String,
    pub manifest: String,
    pub sha256: String,
    pub pixel_size: [u32; 2],
    pub selection: &'static str,
    pub coverage: &'static str,
    pub recorded_frames: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visual_changes: Option<usize>,
    pub candidate_frames: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_selection: Option<EventSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_changes: Option<usize>,
    pub omitted_candidates: usize,
    pub incomplete_tail: bool,
    /// The region of the recorded frames the sheet shows and compares, `[x, y, width, height]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<[u32; 4]>,
    pub tiles: Vec<Tile>,
}

#[derive(Serialize)]
pub(crate) struct Tile {
    pub label: String,
    pub source: RecordedFrame,
    pub source_sha256: String,
    pub rect: [u32; 4],
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new("invalid_recording", message, "inspect the retained recording; keyframes cannot repair missing evidence")
}


/// `rgba` (`w`×`h`) cut to `rect`, which must lie within it.
fn region(w: usize, h: usize, rgba: Vec<u8>, rect: Option<[u32; 4]>) -> Result<(usize, usize, Vec<u8>), ToolError> {
    let Some([x, y, rw, rh]) = rect.map(|r| r.map(|v| v as usize)) else { return Ok((w, h, rgba)) };
    if rw == 0 || rh == 0 || x + rw > w || y + rh > h {
        return Err(ToolError::new("invalid_arguments", format!("rect must lie within the recorded frames' {w}×{h} pixels"), "choose a nonempty region of the frame, as recorded_frame shows it"));
    }
    let mut cut = Vec::with_capacity(rw * rh * 4);
    for row in y..y + rh {
        cut.extend_from_slice(&rgba[(row * w + x) * 4..(row * w + x + rw) * 4]);
    }
    Ok((rw, rh, cut))
}

fn thumbnail(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    let mut pixels = vec![235; WIDTH * HEIGHT * 3];
    let scale = (WIDTH as f64 / w as f64).min(HEIGHT as f64 / h as f64).min(1.0);
    let tw = ((w as f64 * scale).round() as usize).max(1);
    let th = ((h as f64 * scale).round() as usize).max(1);
    let ox = (WIDTH - tw) / 2;
    let oy = (HEIGHT - th) / 2;
    for y in 0..th {
        for x in 0..tw {
            let src = ((y * h / th) * w + x * w / tw) * 4;
            let dst = ((y + oy) * WIDTH + x + ox) * 3;
            let alpha = u32::from(rgba[src + 3]);
            for c in 0..3 {
                pixels[dst + c] = ((u32::from(rgba[src + c]) * alpha + 255 * (255 - alpha)) / 255) as u8;
            }
        }
    }
    pixels
}

// Tiny fixed numeric labels avoid a host font dependency. Labels are tile number : virtual microseconds.
fn label(pixels: &mut [u8], stride: usize, x: usize, y: usize, text: &str) {
    const DIGITS: [[u8; 5]; 10] = [
        [7,5,5,5,7],[2,6,2,2,7],[7,1,7,4,7],[7,1,7,1,7],[5,5,7,1,1],
        [7,4,7,1,7],[7,4,7,5,7],[7,1,1,1,1],[7,5,7,5,7],[7,5,7,1,7],
    ];
    for (n, ch) in text.bytes().enumerate() {
        let rows = match ch { b'0'..=b'9' => DIGITS[(ch-b'0') as usize], b':' => [0,2,0,2,0], _ => [0;5] };
        for (row, bits) in rows.into_iter().enumerate() {
            for col in 0..3 {
                if bits & (1 << (2-col)) != 0 {
                    for dy in 0..2 { for dx in 0..2 {
                        let offset = ((y+row*2+dy)*stride+x+n*8+col*2+dx)*3;
                        pixels[offset..offset+3].fill(30);
                    }}
                }
            }
        }
    }
}

pub(crate) fn derive(bundle: &Path, since: Option<u64>, until: Option<u64>, max: u32, rect: Option<[u32; 4]>) -> Result<(Sheet, Vec<u8>), ToolError> {
    if !(1..=24).contains(&max) {
        return Err(ToolError::new("invalid_arguments", "max_frames must be 1–24", "choose a bounded contact sheet"));
    }
    let evidence = recordings::frames(bundle, since, until)?;
    if evidence.frames.len() > MAX_SCAN {
        return Err(ToolError::new("result_too_large", "keyframes scans at most 512 recorded frames", "narrow since_us/until_us; no evidence was silently skipped"));
    }
    if evidence.frames.is_empty() {
        return Err(ToolError::new("not_found", "no published frames in the requested range", "inspect capture recorded_frames or record presentations first"));
    }
    let count = evidence.frames.len();
    let mut changes = Vec::new();
    let mut previous = None;
    for frame in evidence.frames {
        let path = recordings::retained_path(bundle, &frame.file)?;
        let bytes = std::fs::read(path).map_err(|e| invalid(e.to_string()))?;
        let (w, h, rgba) = decode(&bytes)?;
        let (w, h, rgba) = region(w, h, rgba, rect)?;
        let identity = (frame.document, w, h, sha256_bytes_hex(&rgba));
        if previous.as_ref() != Some(&identity) {
            changes.push((frame, sha256_bytes_hex(&bytes), thumbnail(w, h, &rgba)));
            previous = Some(identity);
        }
    }
    let visual_changes=Some(changes.len());
    render(bundle,changes,max,RenderEvidence { count, visual_changes, incomplete_tail:evidence.incomplete_tail, events:None, rect })
}

struct RenderEvidence {
    rect: Option<[u32; 4]>,
    count: usize,
    visual_changes: Option<usize>,
    incomplete_tail: bool,
    events: Option<EventSelection>,
}

type Candidate = (RecordedFrame,String,Vec<u8>);

fn render(bundle:&Path, changes:Vec<Candidate>, max:u32, evidence:RenderEvidence) -> Result<(Sheet,Vec<u8>),ToolError> {
    let total = changes.len();
    let selected = total.min(max as usize);
    let cols = selected.min(3);
    let rows = selected.div_ceil(cols);
    let (width, height) = (cols * WIDTH, rows * (HEIGHT + LABEL));
    let mut pixels = vec![250; width * height * 3];
    let mut tiles = Vec::new();
    for tile in 0..selected {
        let index = if selected == 1 { 0 } else { tile * (total-1) / (selected-1) };
        let (frame, hash, thumb) = &changes[index];
        let (x, y) = ((tile % cols) * WIDTH, (tile / cols) * (HEIGHT + LABEL));
        let text = format!("{}: {}", tile+1, frame.t_us.0);
        label(&mut pixels, width, x+8, y+7, &text);
        for row in 0..HEIGHT {
            let dst = ((y+LABEL+row)*width+x)*3;
            pixels[dst..dst+WIDTH*3].copy_from_slice(&thumb[row*WIDTH*3..(row+1)*WIDTH*3]);
        }
        tiles.push(Tile { label: text, source: frame.clone(), source_sha256: hash.clone(), rect: [x as u32, (y+LABEL) as u32, WIDTH as u32, HEIGHT as u32] });
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().map_err(|e| invalid(e.to_string()))?.write_image_data(&pixels).map_err(|e| invalid(e.to_string()))?;
    }
    let hash = sha256_bytes_hex(&png);
    let mut sheet = Sheet {
        file: format!("derived/keyframes-{hash}.png"), manifest: String::new(), sha256: hash,
        pixel_size: [width as u32, height as u32], selection: if evidence.events.is_some() {
            "last strictly before, first/last at the same time, first strictly after each event; no causal or same-time order claim"
        } else { "consecutive pixel changes, evenly sampled including endpoints; one tile selects the first" },
        coverage: "published recorded frames only; not proof that every presentation was recorded; labels are tile: virtual microseconds",
        recorded_frames: evidence.count, visual_changes: evidence.visual_changes, candidate_frames:total,
        omitted_changes: evidence.visual_changes.map(|_|total-selected), omitted_candidates:total-selected, event_selection:evidence.events,
        incomplete_tail: evidence.incomplete_tail, rect: evidence.rect, tiles,
    };
    let provenance = frontcap_core::canonical::canonical_of(&sheet);
    sheet.manifest = format!("derived/keyframes-{}.json", sha256_bytes_hex(provenance.as_bytes()));
    crate::derived::publish(bundle, "keyframes", &sheet.file, &png, &sheet.manifest, &sheet)?;
    Ok((sheet, png))
}

#[cfg(test)]
#[path = "keyframes_tests.rs"]
mod tests;

#[derive(Serialize)]
pub(crate) struct EventSelection {
    pub trace_prefix_sha256: String,
    pub trace_incomplete_tail: bool,
    pub frame_context: &'static str,
    pub matches: Vec<EventMatch>,
}

#[derive(Serialize)]
pub(crate) struct EventMatch {
    pub event: frontcap_core::event::BehaviorEvent,
    pub strictly_before: Option<String>,
    pub same_time_first: Option<String>,
    pub same_time_last: Option<String>,
    pub same_time_count: usize,
    pub same_time_omitted: usize,
    pub strictly_after: Option<String>,
}

pub(crate) fn event_frames(bundle:&Path,seqs:&[u64],rect:Option<[u32;4]>) -> Result<(Sheet,Vec<u8>),ToolError> {
    if seqs.is_empty() || seqs.len()>6 {
        return Err(ToolError::new("invalid_arguments","event_frames takes 1–6 event seqs","choose seqs returned by debug events"));
    }
    let history=crate::history::read(bundle)?;
    let evidence=recordings::frames(bundle,None,None)?;
    if evidence.frames.is_empty() { return Err(ToolError::new("not_found","no published recording frames","record presentations before deriving event frames")); }
    if evidence.frames.windows(2).any(|pair|pair[1].t_us<pair[0].t_us) {
        return Err(invalid("recording index moves backward in virtual time"));
    }
    let mut indices=std::collections::BTreeSet::new();
    let mut matches=Vec::new();
    let requested:std::collections::BTreeSet<_>=seqs.iter().copied().collect();
    for seq in requested {
        let event=history.events.iter().find(|e|e.seq==seq).ok_or_else(||ToolError::new("not_found",format!("no published event at seq {seq}"),"read debug events; an incomplete last record is not published"))?;
        let before=evidence.frames.iter().rposition(|f|f.t_us<event.t_us);
        let first=evidence.frames.iter().position(|f|f.t_us==event.t_us);
        let last=evidence.frames.iter().rposition(|f|f.t_us==event.t_us);
        let after=evidence.frames.iter().position(|f|f.t_us>event.t_us);
        for i in [before,first,last,after].into_iter().flatten() { indices.insert(i); }
        let file=|i:Option<usize>|i.map(|n|evidence.frames[n].file.clone());
        matches.push(EventMatch {event:event.clone(),strictly_before:file(before),same_time_first:file(first),same_time_last:file(last),
            same_time_count:first.zip(last).map(|(a,b)|b-a+1).unwrap_or(0),
            same_time_omitted:first.zip(last).map(|(a,b)|(b-a+1).saturating_sub(2)).unwrap_or(0),strictly_after:file(after)});
    }
    let mut candidates=Vec::new();
    for i in indices {
        let frame=evidence.frames[i].clone();
        let path=recordings::retained_path(bundle,&frame.file)?;
        let bytes=std::fs::read(path).map_err(|e|invalid(e.to_string()))?;
        let (w,h,rgba)=decode(&bytes)?;
        let (w,h,rgba)=region(w,h,rgba,rect)?;
        candidates.push((frame,sha256_bytes_hex(&bytes),thumbnail(w,h,&rgba)));
    }
    let selection=EventSelection {trace_prefix_sha256:history.complete_prefix_sha256,trace_incomplete_tail:history.incomplete_tail,
        frame_context:"launch page only; an event can concern another context; temporal neighbors do not prove its visual effect",matches};
    render(bundle,candidates,24,RenderEvidence {count:evidence.frames.len(),visual_changes:None,incomplete_tail:evidence.incomplete_tail,events:Some(selection),rect})
}
