use super::*;
use frontcap_core::time::VirtualTime;

fn entry(file: &str, t_us: u64) -> RecordedFrame {
    RecordedFrame { frame: t_us, t_us: VirtualTime(t_us), document: 1, file: file.into() }
}

fn index(root: &Path, entries: &[RecordedFrame]) {
    std::fs::create_dir_all(root.join("frames")).unwrap();
    let text: String = entries.iter().map(|e| serde_json::to_string(e).unwrap() + "\n").collect();
    std::fs::write(root.join("frames/frames.jsonl"), text).unwrap();
}

#[test]
fn ranges_and_incomplete_publication_preserve_available_evidence() {
    let root = tempfile::tempdir().unwrap();
    assert!(frames(root.path(), None, None).unwrap().frames.is_empty());
    let entries = [entry("frames/a.png",10),entry("frames/b.png",20),entry("frames/c.png",30)];
    index(root.path(), &entries);
    assert_eq!(frames(root.path(), Some(20), Some(20)).unwrap().frames, entries[1..2]);
    assert!(frames(root.path(), Some(30), Some(10)).is_err());
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(root.path().join("frames/frames.jsonl")).unwrap()
        .write_all(b"{\"file\":\"\xe2").unwrap();
    let kept = frames(root.path(), None, None).unwrap();
    assert_eq!(kept.frames, entries);
    assert!(kept.incomplete_tail);
    std::fs::write(root.path().join("frames/frames.jsonl"), b"not an entry\n").unwrap();
    assert!(frames(root.path(), None, None).is_err(), "a corrupt complete entry is not silently skipped");
}

#[test]
fn only_indexed_images_inside_the_recording_are_read() {
    let root = tempfile::tempdir().unwrap();
    index(root.path(), &[entry("frames/a.png",10)]);
    let png = b"\x89PNG\r\n\x1a\nretained bytes";
    std::fs::write(root.path().join("frames/a.png"), png).unwrap();
    assert_eq!(image(root.path(),"frames/a.png").unwrap().1, png);
    assert!(image(root.path(),"../outside.png").is_err());
    std::fs::write(root.path().join("frames/a.png"), b"not a PNG").unwrap();
    assert!(image(root.path(),"frames/a.png").is_err());
    index(root.path(), &[entry("frames/../outside.png",10)]);
    std::fs::write(root.path().join("outside.png"), png).unwrap();
    assert!(image(root.path(),"frames/../outside.png").is_err());
}

#[cfg(unix)]
#[test]
fn indexed_symlinks_cannot_expose_other_files() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    index(root.path(), &[entry("frames/a.png",10)]);
    std::fs::write(outside.path().join("outside.png"), b"\x89PNG\r\n\x1a\noutside").unwrap();
    std::os::unix::fs::symlink(outside.path().join("outside.png"), root.path().join("frames/a.png")).unwrap();
    assert!(image(root.path(),"frames/a.png").is_err());
}
