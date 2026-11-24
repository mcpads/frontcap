use super::*;
use frontcap_core::{observation::RecordedFrame,time::VirtualTime};

fn recording(root:&Path) {
    std::fs::create_dir_all(root.join("frames")).unwrap();
    let mut index=String::new();
    for (i,(at,color)) in [(1000,[255,0,0]),(35000,[0,0,255]),(35000,[0,255,0]),(91000,[0,0,255])].into_iter().enumerate() {
        let file=format!("frames/{i}.png");
        let mut png=Vec::new();
        {
            let mut encoder=png::Encoder::new(&mut png,8,4);
            encoder.set_color(png::ColorType::Rgb);encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&color.repeat(32)).unwrap();
        }
        std::fs::write(root.join(&file),png).unwrap();
        index.push_str(&(serde_json::to_string(&RecordedFrame {frame:i as u64,t_us:VirtualTime(at),document:1,file}).unwrap()+"\n"));
    }
    std::fs::write(root.join("frames/frames.jsonl"),index).unwrap();
}

#[tokio::test]
async fn encoded_video_keeps_virtual_intervals_and_labels_synthetic_tail() {
    for tool in ["ffmpeg","ffprobe"] {
        if std::process::Command::new(tool).arg("-version").output().is_err() {
            eprintln!("SKIP: {tool} unavailable; video timing remains unqualified on this host"); return;
        }
    }
    let root=tempfile::tempdir().unwrap();recording(root.path());
    let before=std::fs::read(root.path().join("frames/frames.jsonl")).unwrap();
    let metadata=derive(root.path().to_owned(),None,None,100).await.unwrap();
    assert_eq!(metadata["source_start_us"],1000);
    assert_eq!(metadata["source_last_us"],91000);
    assert_eq!(metadata["duration_us"],190000);
    assert_eq!(metadata["sources"][1]["encoded"],false);
    assert_eq!(metadata["sources"][2]["encoded"],true);
    let video=root.path().join(metadata["file"].as_str().unwrap());
    let output=std::process::Command::new("ffprobe").args(["-v","error","-show_entries","packet=pts,duration:stream=time_base,duration","-of","json"]).arg(&video).output().unwrap();
    assert!(output.status.success());
    let probed:Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(probed["streams"][0]["time_base"],"1/1000000");
    assert_eq!(probed["streams"][0]["duration"],"0.190000");
    assert_eq!(probed["packets"].as_array().unwrap().iter().map(|p|p["pts"].as_u64().unwrap()).collect::<Vec<_>>(),vec![0,34000,90000,189999]);
    assert_eq!(probed["packets"].as_array().unwrap().iter().map(|p|p["duration"].as_u64().unwrap()).sum::<u64>(),190000);
    let decoded=std::process::Command::new("ffmpeg").args(["-v","error","-i"]).arg(&video)
        .args(["-fps_mode","passthrough","-f","rawvideo","-pix_fmt","rgb24","pipe:1"]).output().unwrap();
    assert!(decoded.status.success());
    assert_eq!(decoded.stdout.len(),4*8*4*3);
    for (pixels,color) in decoded.stdout.as_chunks::<{8*4*3}>().0.iter().zip([[255u8,0,0],[0,255,0],[0,0,255],[0,0,255]]) {
        assert!(pixels[..3].iter().zip(color).all(|(actual,expected)|actual.abs_diff(expected)<=5),"{:?}",&pixels[..3]);
    }
    let again=derive(root.path().to_owned(),None,None,100).await.unwrap();
    assert_eq!(again,metadata,"same frozen evidence and encoder produce identical provenance");
    assert_eq!(derived::list(root.path()).unwrap().len(),1);
    assert_eq!(std::fs::read(root.path().join("frames/frames.jsonl")).unwrap(),before);
}

#[test]
fn preparation_rejects_corrupt_and_invalid_inputs_without_publishing() {
    let root=tempfile::tempdir().unwrap();recording(root.path());
    assert!(prepare(root.path(),None,None,0).is_err());
    assert!(prepare(root.path(),Some(100000),None,100).is_err());
    std::fs::write(root.path().join("frames/1.png"),b"corrupt even though zero duration").unwrap();
    assert!(prepare(root.path(),None,None,100).is_err());
    assert!(!root.path().join("derived").exists());
}
