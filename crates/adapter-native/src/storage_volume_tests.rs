use super::*;

// Actual pipes/files/child lifetime; host disk attachment belongs to the installed APFS probe.
fn config(root: &Path, wrong_disposal: bool) -> Config {
    let worker = root.join("volume.py");
    std::fs::write(&worker, format!(r#"import sys,json,os,pathlib,hashlib
p=pathlib.Path(sys.argv[1]);assert sys.stdin.buffer.read(1)==b'1'
p.mkdir();mount=p/'volume/mount';mount.mkdir(parents=True);image=p/'volume/bounded.dmg'
with image.open('xb') as f:f.truncate(int(sys.argv[3])*1024*1024)
def save(name,value):
 q=p/name;t=q.with_suffix('.tmp');t.write_text(json.dumps(value));t.replace(q)
save('ready.json',dict(mount=str(mount),image=str(image),size_mib=int(sys.argv[3]),reserve_mib=int(sys.argv[5]),owner_pid=os.getpid()))
request=json.loads(sys.stdin.buffer.readline());save('retirement-request.json',request)
assert request==dict(operation='retire',writers_retired=True,remove=False)
with image.open('rb') as f:
 digest=hashlib.sha256()
 while True:
  block=f.read(65536)
  if not block:break
  digest.update(block)
save('retirement.json',dict(passed=True,detached=True,removed={},retained_image=dict(bytes=image.stat().st_size,sha256=digest.hexdigest())))
"#, if wrong_disposal { "True" } else { "False" })).unwrap();
    Config { python: "/usr/bin/python3".into(), worker, size_mib: 256, reserve_mib: 8 }
}

#[test]
fn retained_image_is_bound_to_original_worker_and_bytes_without_disposal_authority() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let config = config(&root, false);
    let directory = root.join("owner");
    let worker = Worker::start(&config, &directory, Instant::now() + Duration::from_secs(10)).unwrap();
    let evidence = worker.retire_retaining(Instant::now() + Duration::from_secs(30)).unwrap();
    assert!(evidence.worker.has_exited().unwrap());
    let image = directory.join("worker/volume/bounded.dmg");
    assert_eq!(image.metadata().unwrap().len(), 256 * 1024 * 1024);
    inspect_retained(&directory, &config, &evidence).unwrap();
    assert!(inspect(&directory, &config, &evidence).is_err(), "retention must not qualify scratch deletion");
    // Same-length modification must invalidate read-only inspection, without deleting the evidence.
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::OpenOptions::new().write(true).open(&image).unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(b"changed").unwrap();
    assert!(inspect_retained(&directory, &config, &evidence).is_err());
    assert_eq!(image.metadata().unwrap().len(), 256 * 1024 * 1024);
    // A substituted link must never be followed by the verifier.
    let original = image.with_extension("original");
    std::fs::rename(&image, &original).unwrap();
    std::os::unix::fs::symlink(&original, &image).unwrap();
    assert!(inspect_retained(&directory, &config, &evidence).is_err());
    assert!(original.exists());
}

#[test]
fn rejected_watch_returns_original_error_and_preserves_live_retirement_authority() {
    let temporary = tempfile::tempdir().unwrap();let root = temporary.path().canonicalize().unwrap();
    let config = config(&root, false);
    let script = std::fs::read_to_string(&config.worker).unwrap().replace(
        "request=json.loads(sys.stdin.buffer.readline());save('retirement-request.json',request)",
        "watch=sys.stdin.buffer.readline();assert json.loads(watch)['operation']=='watch_uikit'\nsave('watch-rejected.json',dict(request_sha256=hashlib.sha256(watch).hexdigest(),error='original worker exited'))\nrequest=json.loads(sys.stdin.buffer.readline());save('retirement-request.json',request)");
    std::fs::write(&config.worker, script).unwrap();
    let directory = root.join("owner");
    let mut worker = Worker::start(&config, &directory, Instant::now() + Duration::from_secs(10)).unwrap();
    let session = worker.mount.join("session");std::fs::create_dir(&session).unwrap();
    std::fs::write(session.join("ready.json"), b"{}").unwrap();
    let identity = frontcap_core::process::ProcessIdentity::of(std::process::id()).unwrap().unwrap();
    let error = worker.watch_uikit(&session, &identity, None, Instant::now() + Duration::from_secs(10)).unwrap_err();
    assert!(error.to_string().contains("watch rejected: original worker exited"));
    assert!(worker.child.as_mut().unwrap().try_wait().unwrap().is_none());
    let evidence = worker.retire_retaining(Instant::now() + Duration::from_secs(30)).unwrap();
    assert!(evidence.worker.has_exited().unwrap());
    inspect_retained(&directory, &config, &evidence).unwrap();
}

#[test]
fn retained_retirement_rejects_worker_disposal_mismatch() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let config = config(&root, true);
    let directory = root.join("owner");
    let worker = Worker::start(&config, &directory, Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(worker.retire_retaining(Instant::now() + Duration::from_secs(30)).is_err());
    assert!(directory.join("worker/volume/bounded.dmg").exists());
}

#[test]
#[ignore = "requires fresh FRONTCAP_RETAINED_VOLUME_HOST_ROOT and explicit Python/worker paths on macOS"]
fn installed_apfs_retirement_preserves_a_verified_image() {
    let root = PathBuf::from(std::env::var_os("FRONTCAP_RETAINED_VOLUME_HOST_ROOT").expect("fresh host output"));
    assert!(root.is_absolute());
    std::fs::create_dir(&root).unwrap();
    let config = Config {
        python: std::env::var_os("FRONTCAP_RETAINED_VOLUME_PYTHON").expect("Python path").into(),
        worker: std::env::var_os("FRONTCAP_RETAINED_VOLUME_WORKER").expect("worker path").into(),
        size_mib: 256,
        reserve_mib: 8,
    };
    frontcap_core::state::save_atomic(&root.join("config.json"), &config).unwrap();
    let directory = root.join("owner");
    let mut worker = Worker::start(&config, &directory, Instant::now() + Duration::from_secs(90)).unwrap();
    if std::env::var("FRONTCAP_RETAINED_VOLUME_REJECT_WATCH").as_deref() == Ok("1") {
        // Deliberately incomplete session: no target/diagnostic process is ever spawned.
        let session = worker.mount.join("rejected-session");std::fs::create_dir(&session).unwrap();
        std::fs::write(session.join("ready.json"), b"{}").unwrap();
        let identity = frontcap_core::process::ProcessIdentity::of(std::process::id()).unwrap().unwrap();
        let error = worker.watch_uikit(&session, &identity, None, Instant::now() + Duration::from_secs(10)).unwrap_err();
        assert!(error.to_string().contains("UIKit volume watch rejected:"), "{error}");
        assert!(worker.child.as_mut().unwrap().try_wait().unwrap().is_none());
        frontcap_core::state::save_atomic(&root.join("watch-rejection.json"), &error.to_string()).unwrap();
    }
    use std::os::unix::fs::MetadataExt;
    assert_ne!(worker.mount.metadata().unwrap().dev(), root.metadata().unwrap().dev());
    // This fixture is the only data writer. Close it before attesting writer retirement.
    let fixture = worker.mount.join("retained-proof.txt");
    {
        let mut file = std::fs::File::create_new(&fixture).unwrap();
        file.write_all(b"original retained volume evidence\n").unwrap();
        file.sync_all().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    let evidence = if std::env::var("FRONTCAP_RETAINED_VOLUME_COMPRESSION").as_deref() == Ok("gzip") {
        worker.retire_compressed(deadline)
    } else { worker.retire_retaining(deadline) }.unwrap();
    assert!(evidence.worker.has_exited().unwrap());
    assert!(!fixture.exists(), "detached mount must not expose the fixture");
    frontcap_core::state::save_atomic(&root.join("retirement-evidence.json"), &evidence).unwrap();
    inspect_retained(&directory, &config, &evidence).unwrap();
    assert!(inspect(&directory, &config, &evidence).is_err());
    frontcap_core::state::save_atomic(&root.join("verified-image.json"),
        evidence.retained_image.as_ref().unwrap()).unwrap();
}

#[test]
fn compressed_retirement_keeps_original_digest_and_rejects_damaged_archive() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let config = config(&root, false);
    let mut script = std::fs::read_to_string(&config.worker).unwrap().replace(
        "assert request==dict(operation='retire',writers_retired=True,remove=False)",
        "assert request==dict(operation='retire',writers_retired=True,remove=False,compression='gzip')");
    script.push_str(r#"
import gzip
archive=image.with_name(image.name+'.gz')
with image.open('rb') as src,archive.open('xb') as dst:
 with gzip.GzipFile(filename='',mode='wb',fileobj=dst,compresslevel=1,mtime=0) as encoded:
  while True:
   block=src.read(65536)
   if not block:break
   encoded.write(block)
record=json.loads((p/'retirement.json').read_text())
record['retained_image']['gzip']=dict(bytes=archive.stat().st_size,sha256=hashlib.sha256(archive.read_bytes()).hexdigest())
image.unlink();save('retirement.json',record)
"#);
    std::fs::write(&config.worker, script).unwrap();
    let directory = root.join("owner");
    let worker = Worker::start(&config, &directory, Instant::now() + Duration::from_secs(10)).unwrap();
    let evidence = worker.retire_compressed(Instant::now() + Duration::from_secs(30)).unwrap();
    assert!(evidence.worker.has_exited().unwrap());
    inspect_retained(&directory, &config, &evidence).unwrap();
    let retained = evidence.retained_image.as_ref().unwrap();
    assert!(retained.gzip.as_ref().unwrap().bytes < retained.bytes);
    let image = directory.join("worker/volume/bounded.dmg");
    assert!(!image.exists());
    let archive = image.with_extension("dmg.gz");
    let bytes = std::fs::read(&archive).unwrap();
    let mut damaged = bytes.clone(); damaged[3] = 8; // Forbidden filename field; refuse before header allocation.
    std::fs::write(&archive, damaged).unwrap();
    assert!(inspect_retained(&directory, &config, &evidence).is_err());
    std::fs::write(&archive, &bytes).unwrap();
    // Even a caller supplying an updated encoded digest cannot hide trailing bytes.
    let mut appended = bytes; appended.push(0);
    std::fs::write(&archive, &appended).unwrap();
    let mut claimed = retained.clone();
    claimed.gzip = Some(CompressedImage { bytes: appended.len() as u64,
        sha256: frontcap_core::canonical::sha256_bytes_hex(&appended) });
    assert!(verify_image(&image, retained.bytes, false, Some(&claimed), None).is_err());
    assert!(archive.exists());
}
