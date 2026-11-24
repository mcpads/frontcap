use super::*;

#[test]
fn concurrent_publishers_retain_every_artifact_and_retries_are_identical() {
    let root=tempfile::tempdir().unwrap();
    let workers:Vec<_>=(0..8).map(|n| {
        let root=root.path().to_owned();
        std::thread::spawn(move || {
            let file=format!("derived/{n}.png");
            let manifest=format!("derived/{n}.json");
            for _ in 0..2 {
                publish(&root,"keyframes",&file,&[n],&manifest,&serde_json::json!({"source":n})).unwrap();
            }
        })
    }).collect();
    for worker in workers { worker.join().unwrap(); }
    let artifacts=list(root.path()).unwrap();
    assert_eq!(artifacts.len(),8);
    std::fs::write(root.path().join("derived/0.png"),b"corrupt").unwrap();
    assert!(list(root.path()).is_err());
    assert!(publish(root.path(),"keyframes","derived/0.png",&[0],"derived/0.json",&serde_json::json!({"source":0})).is_err(),"retry must not overwrite corrupt retained evidence");
}

#[test]
fn incomplete_publication_is_not_advertised_and_corrupt_catalog_is_not_replaced() {
    let root=tempfile::tempdir().unwrap();
    assert!(list(root.path()).unwrap().is_empty());
    assert!(!root.path().join("derived").exists(),"a read must not create artifacts");
    std::fs::create_dir(root.path().join("derived")).unwrap();
    std::fs::write(root.path().join("derived/orphan.png"),b"unpublished").unwrap();
    assert!(list(root.path()).unwrap().is_empty());
    std::fs::write(root.path().join("derived/index.json"),b"{").unwrap();
    assert!(list(root.path()).is_err());
    assert!(publish(root.path(),"keyframes","derived/a.png",b"a","derived/a.json",&()).is_err());
    assert!(!root.path().join("derived/a.png").exists());
}

#[cfg(unix)]
#[test]
fn artifacts_and_catalog_cannot_redirect_reads_or_writes() {
    let root=tempfile::tempdir().unwrap();
    let outside=tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(),root.path().join("derived")).unwrap();
    assert!(publish(root.path(),"keyframes","derived/a.png",b"a","derived/a.json",&()).is_err());
    assert!(list(root.path()).is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(),0);
}
