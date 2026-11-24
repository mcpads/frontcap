use super::*;
use frontcap_core::time::VirtualTime;

fn recording(root: &Path, colors: &[[u8;3]]) {
    std::fs::create_dir_all(root.join("frames")).unwrap();
    let mut index = String::new();
    for (i, color) in colors.iter().enumerate() {
        let file = format!("frames/{i}.png");
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 8, 4);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&color.repeat(32)).unwrap();
        }
        std::fs::write(root.join(&file),png).unwrap();
        let entry = RecordedFrame { file, frame:i as u64, document:1, t_us:VirtualTime(i as u64*100) };
        index.push_str(&(serde_json::to_string(&entry).unwrap()+"\n"));
    }
    std::fs::write(root.join("frames/frames.jsonl"),index).unwrap();
}

#[test]
fn sheets_select_changes_preserve_provenance_and_do_not_mutate_evidence() {
    let root = tempfile::tempdir().unwrap();
    recording(root.path(),&[[255,0,0],[255,0,0],[0,255,0],[0,0,255],[255,0,0]]);
    let index = std::fs::read(root.path().join("frames/frames.jsonl")).unwrap();
    let (sheet, png) = derive(root.path(),None,None,3,None).unwrap();
    assert_eq!((sheet.recorded_frames,sheet.visual_changes,sheet.omitted_changes),(5,Some(4),Some(1)));
    assert_eq!(sheet.tiles.iter().map(|t|t.source.frame).collect::<Vec<_>>(),vec![0,2,4]);
    assert_eq!(sheet.tiles[1].label,"2: 200");
    assert_eq!(sheet.sha256,sha256_bytes_hex(&png));
    assert_eq!(std::fs::read(root.path().join(&sheet.file)).unwrap(),png);
    let manifest: serde_json::Value=serde_json::from_slice(&std::fs::read(root.path().join(&sheet.manifest)).unwrap()).unwrap();
    assert_eq!(manifest["tiles"][1]["source"]["file"],"frames/2.png");
    for tile in &sheet.tiles {
        assert_eq!(tile.source_sha256,sha256_bytes_hex(&std::fs::read(root.path().join(&tile.source.file)).unwrap()));
    }
    let (w,h,pixels)=decode(&png).unwrap();
    assert_eq!([w as u32,h as u32],sheet.pixel_size);
    for (tile,color) in sheet.tiles.iter().zip([[255,0,0,255],[0,255,0,255],[255,0,0,255]]) {
        let [x,y,tw,th]=tile.rect;
        let center=((y+th/2) as usize*w+(x+tw/2) as usize)*4;
        assert_eq!(pixels[center..center+4],color);
    }
    let (again,bytes)=derive(root.path(),None,None,3,None).unwrap();
    assert_eq!(again.manifest,sheet.manifest);
    assert_eq!(bytes,png);
    assert_eq!(std::fs::read(root.path().join("frames/frames.jsonl")).unwrap(),index);
    let (range,_)=derive(root.path(),Some(200),Some(300),24,None).unwrap();
    assert_eq!(range.tiles.iter().map(|t|t.source.t_us.0).collect::<Vec<_>>(),vec![200,300]);
    assert!(derive(root.path(),None,None,0,None).is_err());
    assert!(derive(root.path(),Some(999),None,2,None).is_err());
}

#[test]
fn incomplete_and_corrupt_evidence_are_not_claimed_complete() {
    let root=tempfile::tempdir().unwrap();
    recording(root.path(),&[[255,0,0],[0,255,0],[0,0,255]]);
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(root.path().join("frames/frames.jsonl")).unwrap().write_all(b"{").unwrap();
    let (sheet,_)=derive(root.path(),None,None,1,None).unwrap();
    assert!(sheet.incomplete_tail);
    assert_eq!(sheet.omitted_changes,Some(2));
    // Even an image that would not have been selected must decode before completeness is counted.
    std::fs::write(root.path().join("frames/1.png"),b"corrupt").unwrap();
    assert!(derive(root.path(),None,None,1,None).is_err());
}

#[test]
fn event_frames_keep_temporal_neighbors_and_same_time_uncertainty() {
    let root=tempfile::tempdir().unwrap();
    recording(root.path(),&[[255,0,0],[255,0,0],[255,0,0],[0,255,0],[0,0,255]]);
    // Multiple presentations at one instant must not be collapsed as visual-change selection does.
    let path=root.path().join("frames/frames.jsonl");
    let frames=std::fs::read_to_string(&path).unwrap().lines().enumerate().map(|(i,line)| {
        let mut frame:RecordedFrame=serde_json::from_str(line).unwrap();
        frame.t_us=VirtualTime([0,100,100,100,200][i]);
        serde_json::to_string(&frame).unwrap()+"\n"
    }).collect::<String>();
    std::fs::write(&path,frames).unwrap();
    std::fs::write(root.path().join("trace.jsonl"),"{\"seq\":0,\"t_us\":100,\"kind\":\"console\",\"payload\":{\"level\":\"log\",\"text\":\"event\"}}\n{").unwrap();
    let (sheet,_)=event_frames(root.path(),&[0,0],None).unwrap();
    assert_eq!(sheet.tiles.iter().map(|t|t.source.frame).collect::<Vec<_>>(),vec![0,1,3,4]);
    assert_eq!(sheet.visual_changes,None);assert_eq!(sheet.omitted_changes,None);
    let selection=sheet.event_selection.unwrap();
    assert!(selection.trace_incomplete_tail);
    assert_eq!(selection.matches.len(),1);
    let m=&selection.matches[0];
    assert_eq!(m.same_time_count,3);assert_eq!(m.same_time_omitted,1);
    assert_eq!(m.strictly_before.as_deref(),Some("frames/0.png"));
    assert_eq!(m.same_time_first.as_deref(),Some("frames/1.png"));
    assert_eq!(m.same_time_last.as_deref(),Some("frames/3.png"));
    assert_eq!(m.strictly_after.as_deref(),Some("frames/4.png"));
    assert!(event_frames(root.path(),&[1],None).is_err());
    assert!(event_frames(root.path(),&[],None).is_err());
}

#[test]
fn a_region_is_compared_and_shown_alone() {
    let root = tempfile::tempdir().unwrap();
    recording(root.path(), &[[0, 0, 0], [255, 255, 255]]);
    let (sheet, _) = derive(root.path(), None, None, 3, Some([0, 0, 2, 2])).unwrap();
    assert_eq!(sheet.rect, Some([0, 0, 2, 2]));
    assert_eq!(sheet.visual_changes, Some(2));
    assert!(derive(root.path(), None, None, 3, Some([0, 0, 0, 2])).is_err(), "an empty region");
    assert!(derive(root.path(), None, None, 3, Some([1, 0, 1000, 1])).is_err(), "a region past the frame");
}
