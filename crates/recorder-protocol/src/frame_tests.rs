use super::*;

#[tokio::test]
async fn frames_are_one_line_each_and_eof_is_none() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (_ar, mut aw) = tokio::io::split(a);
    let (br, _bw) = tokio::io::split(b);
    write_frame(&mut aw, &serde_json::json!({"x": 1})).await.unwrap();
    write_frame(&mut aw, &serde_json::json!({"y": "two\nlines"})).await.unwrap();
    drop(aw);
    drop(_ar);
    let mut br = tokio::io::BufReader::new(br);
    let first: serde_json::Value = read_frame(&mut br).await.unwrap().unwrap();
    let second: serde_json::Value = read_frame(&mut br).await.unwrap().unwrap();
    assert_eq!(first["x"], 1);
    assert_eq!(second["y"], "two\nlines");
    assert!(read_frame::<_, serde_json::Value>(&mut br).await.unwrap().is_none());
}

#[tokio::test]
async fn oversized_frames_are_rejected() {
    let (a, b) = tokio::io::duplex(1 << 16);
    let (_r, mut w) = tokio::io::split(a);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let big = vec![b'a'; MAX_FRAME + 10];
        let _ = w.write_all(&big).await;
    });
    let mut br = tokio::io::BufReader::new(tokio::io::split(b).0);
    assert!(matches!(read_frame::<_, serde_json::Value>(&mut br).await, Err(FrameError::TooLarge)));
}
