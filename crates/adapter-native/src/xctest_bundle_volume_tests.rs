use super::*;
use std::{io::Write, process::{Command, Stdio}, time::Duration};

// Real worker pipes/exit and retained-image protocol, not an actual filesystem quota fixture.
fn fixture(root: &Path) -> Config {
    let script = root.join("volume.py");
    fs::write(&script, r#"import sys,json,os,pathlib,hashlib,gzip
p=pathlib.Path(sys.argv[1]);assert sys.stdin.buffer.read(1)==b'1'
p.mkdir();mount=p/'volume/mount';mount.mkdir(parents=True);image=p/'volume/bounded.dmg'
size=int(sys.argv[3])*1024*1024
with image.open('xb') as f:f.truncate(size)
def save(name,value):
 q=p/name;t=q.with_suffix('.tmp');t.write_text(json.dumps(value));t.replace(q)
save('ready.json',dict(mount=str(mount),image=str(image),size_mib=int(sys.argv[3]),reserve_mib=int(sys.argv[5]),owner_pid=os.getpid()))
line=sys.stdin.buffer.readline()
if not line:sys.exit(0)
request=json.loads(line);assert request==dict(operation='retire',writers_retired=True,remove=False,compression='gzip')
save('observed-retirement.json',request)
digest=hashlib.sha256();compressed=image.with_suffix('.dmg.gz')
with image.open('rb') as source,compressed.open('xb') as raw,gzip.GzipFile(filename='',mode='wb',fileobj=raw,mtime=0) as out:
 while True:
  block=source.read(65536)
  if not block:break
  digest.update(block);out.write(block)
image.unlink()
encoded_hash=hashlib.sha256()
with compressed.open('rb') as source:
 while True:
  block=source.read(65536)
  if not block:break
  encoded_hash.update(block)
encoded=encoded_hash.hexdigest()
save('retirement.json',dict(passed=True,detached=True,removed=False,retained_image=dict(bytes=size,sha256=digest.hexdigest(),gzip=dict(bytes=compressed.stat().st_size,sha256=encoded))))
"#).unwrap();
    Config { python: "/usr/bin/python3".into(), worker: script, size_mib: 256, reserve_mib: 8 }
}

fn acquired_launch(target: &Path, directory: &Path, config: &Config, simulator: &str, bundle: &str)
    -> (crate::simulator::tests::Fixture, crate::xctest::RunnerLaunch) {
    let input = crate::simulator::tests::Fixture::new("normal");
    let mut launch: crate::xctest::RunnerLaunch = crate::xctest::tests::config(&input).into();
    launch.result_bundle_volume = Some(config.clone());
    launch.app.runner_bundle = bundle.into();
    frontcap_core::state::save_atomic(&directory.join("launch.json"), &launch).unwrap();
    frontcap_core::state::save_atomic(&directory.join("simulator.json"), &simulator).unwrap();
    fs::create_dir_all(target.join("simulator")).unwrap();
    frontcap_core::state::save_atomic(&target.join("simulator/simulator.json"), &simulator).unwrap();
    (input, launch)
}

#[test]
fn same_filesystem_bundle_refusal_precedes_sdk_and_still_retires_acquired_simulator() {
    use crate::{simulator::{Simulator, tests::Fixture}, xctest::{RunnerLaunch, Xctest}};
    use std::sync::atomic::AtomicBool;
    let fixture = Fixture::new("normal");
    let mut runner: RunnerLaunch = crate::xctest::tests::config(&fixture).into();
    runner.result_bundle_volume = Some(self::fixture(fixture.root.path()));
    let end = Instant::now() + Duration::from_secs(30);
    let simulator = Simulator::acquire_at(fixture.config.clone(), &fixture.root.path().join("simulator"),
        end, &AtomicBool::new(false), &fixture.root.path().join("host")).unwrap();
    let directory = fixture.root.path().join("xctest");
    let error = match Xctest::on_simulator(runner, simulator, &directory, end, &AtomicBool::new(false)) {
        Err(error) => error,
        Ok(_) => panic!("unbounded SDK result directory accepted"),
    };
    assert!(error.cause.contains("independent owned filesystem"));
    assert!(error.retirement.unwrap().passed);
    assert!(!directory.join("command.json").exists());
    assert!(!directory.join("runner-process.json").exists());
    assert!(directory.join("result-bundle-retirement.json").exists());
    assert!(fixture.root.path().join("deleted").exists());
}

#[test]
fn direct_bundle_writer_requires_live_handle_retirement_before_retaining_volume() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let config = fixture(&root);
    let directory = root.join("owner");fs::create_dir(&directory).unwrap();
    let (_input, original) = acquired_launch(&root, &directory, &config, "owned-simulator", "owned.runner");
    let mut bundle = BundleVolume::prepare(config.clone(), &directory, &root, "owned-simulator", "owned.runner").unwrap();
    let result_bundle = retain_direct_writer(&config, &directory, &mut bundle);
    inspect_target(&root, &original).unwrap();
    assert_eq!(fs::read(result_bundle.join("original")).unwrap(), b"original SDK bundle");
    let prior = fs::read(directory.join("result-bundle-retirement.json")).unwrap();
    fs::remove_file(directory.join("result-bundle-retirement.json")).unwrap();
    assert!(inspect_target(&root, &original).is_err());
    fs::write(directory.join("result-bundle-retirement.json"), prior).unwrap();
    inspect_target(&root, &original).unwrap();
    // Plan and receipt agreeing with each other must not override original acquired/session input.
    let plan_bytes = fs::read(directory.join("result-bundle-plan.json")).unwrap();
    let registry_bytes = fs::read(root.join("xctest-bundle-owners.json")).unwrap();
    let completed_bytes = fs::read(directory.join("result-bundle-retirement.json")).unwrap();
    for change in ["config", "simulator", "runner"] {
        frontcap_core::state::save_atomic(&directory.join("launch.json"), &original).unwrap();
        frontcap_core::state::save_atomic(&directory.join("simulator.json"), &"owned-simulator").unwrap();
        let mut plan: Plan = serde_json::from_slice(&plan_bytes).unwrap();
        let mut completed: Completion = serde_json::from_slice(&completed_bytes).unwrap();
        match change {
            "config" => plan.config.python = "/foreign/python3".into(),
            "simulator" => plan.simulator = "foreign-simulator".into(),
            _ => plan.runner_bundle = "foreign.runner".into(),
        }
        if change == "config" {
            let mut changed_launch = original.clone();
            changed_launch.result_bundle_volume = Some(plan.config.clone());
            frontcap_core::state::save_atomic(&directory.join("launch.json"), &changed_launch).unwrap();
        }
        if change == "simulator" {
            frontcap_core::state::save_atomic(&directory.join("simulator.json"), &plan.simulator).unwrap();
        }
        frontcap_core::state::save_atomic(&root.join("xctest-bundle-owners.json"), &vec![plan.clone()]).unwrap();
        completed.plan = plan.clone();
        frontcap_core::state::save_atomic(&directory.join("result-bundle-plan.json"), &plan).unwrap();
        frontcap_core::state::save_atomic(&directory.join("result-bundle-retirement.json"), &completed).unwrap();
        assert!(inspect_target(&root, &original).is_err(), "{change}");
    }
    frontcap_core::state::save_atomic(&directory.join("launch.json"), &original).unwrap();
    frontcap_core::state::save_atomic(&directory.join("simulator.json"), &"owned-simulator").unwrap();
    fs::write(root.join("xctest-bundle-owners.json"), registry_bytes).unwrap();
    fs::write(directory.join("result-bundle-plan.json"), plan_bytes).unwrap();
    fs::write(directory.join("result-bundle-retirement.json"), completed_bytes).unwrap();
    let mut changed = original.clone();
    changed.result_bundle_volume.as_mut().unwrap().python = "/other/python3".into();
    frontcap_core::state::save_atomic(&directory.join("launch.json"), &changed).unwrap();
    assert!(inspect_target(&root, &original).is_err());
    frontcap_core::state::save_atomic(&directory.join("launch.json"), &original).unwrap();
    let link = root.join("linked-target");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(inspect_target(&link, &original).is_err());
    inspect_target(&root, &original).unwrap();
    let hidden = root.join("hidden-owner");
    fs::rename(&directory, &hidden).unwrap();
    assert!(inspect_target(&root, &original).is_err(), "registered acquired owner cannot silently disappear");
    fs::rename(&hidden, &directory).unwrap();
    inspect_target(&root, &original).unwrap();
}

#[test]
#[ignore = "requires approved isolated APFS host root and real volume worker; never runs in ordinary Linux checks"]
fn installed_bundle_filesystem_refuses_direct_writer_without_losing_control_evidence() {
    let root = PathBuf::from(std::env::var_os("FRONTCAP_XCTEST_BUNDLE_ROOT").expect("fresh owned root"));
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let directory = root.join("owner");fs::create_dir(&directory).unwrap();
    let config = Config {
        python: std::env::var_os("FRONTCAP_RETAINED_VOLUME_PYTHON").expect("Python path").into(),
        worker: std::env::var_os("FRONTCAP_RETAINED_VOLUME_WORKER").expect("worker path").into(),
        size_mib: 256, reserve_mib: 8,
    };
    let (_input, original) = acquired_launch(&root, &directory, &config, "direct-writer-fixture", "no-SDK-app");
    let mut bundle = BundleVolume::prepare(config, &directory, &root, "direct-writer-fixture", "no-SDK-app").unwrap();
    let started = Instant::now();
    let mut child: Option<std::process::Child> = None;
    let mut writer = None;
    let mut child_reaped = true;
    let mut signal_authority = true;
    let mut child_exit = None;
    // All fallible work after acquiring the live manager returns through this cleanup boundary.
    let work = (|| -> io::Result<()> {
        let output = bundle.start(started + Duration::from_secs(90))?;
        fs::write(directory.join("original-control"), b"preserve original receipt")?;
        let stdout = fs::File::create_new(directory.join("writer.stdout"))?;
        let stderr = fs::File::create_new(directory.join("writer.stderr"))?;
        bundle.writer_starting()?;
        child = Some(Command::new("/usr/bin/python3").args(["-c", r#"import pathlib,sys,errno
p=pathlib.Path(sys.argv[1]);p.mkdir()
try:
 with (p/'direct').open('xb') as out:
  for _ in range(512):out.write(b'x'*1048576);out.flush()
except OSError as error:
 assert error.errno==errno.ENOSPC;print('ENOSPC',flush=True);sys.stdin.buffer.read(1);sys.exit(0)
raise AssertionError('Direct writer exceeded declared filesystem capacity')
"#]).arg(output).stdin(Stdio::piped()).stdout(stdout).stderr(stderr).spawn()?);
        child_reaped = false;
        writer = ProcessIdentity::of(child.as_ref().unwrap().id())?;
        let identity = writer.as_ref().ok_or_else(|| io::Error::other("direct writer identity missing"))?;
        frontcap_core::state::save_atomic(&directory.join("runner-process.json"), identity)?;
        loop {
            // The fixed child only prints six bytes; bounded read avoids a blocked pipe reader.
            let mut bytes = Vec::new();
            use std::io::Read;
            fs::File::open(directory.join("writer.stdout"))?.take(65).read_to_end(&mut bytes)?;
            if bytes == b"ENOSPC\n" { return Ok(()); }
            if bytes.len() > 64 { return Err(io::Error::other("unexpected writer output")); }
            match child.as_mut().unwrap().try_wait() {
                Ok(Some(status)) => {
                    child_exit = Some(status); child_reaped = true;
                    return Err(io::Error::other("direct writer exited before ENOSPC receipt"));
                }
                Ok(None) => (),
                Err(error) => { signal_authority = false; return Err(error); }
            }
            if Instant::now() >= started + Duration::from_secs(180) {
                return Err(io::Error::other("direct writer work deadline"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    let mut cleanup_errors = Vec::new();
    if let Some(child) = child.as_mut() {
        // A wait error loses signal authority. Never signal a saved/reused PID after ECHILD.
        drop(child.stdin.take());
        let graceful = Instant::now() + Duration::from_secs(5);
        while !child_reaped && signal_authority && Instant::now() < graceful {
            match child.try_wait() {
                Ok(Some(status)) => { child_exit = Some(status); child_reaped = true; },
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => { signal_authority = false; cleanup_errors.push(error.to_string()); break; }
            }
        }
        if !child_reaped && signal_authority {
            if let Err(error) = child.kill() { cleanup_errors.push(error.to_string()); }
            let end = Instant::now() + Duration::from_secs(10);
            while !child_reaped && Instant::now() < end {
                match child.try_wait() {
                    Ok(Some(status)) => { child_exit = Some(status); child_reaped = true; },
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(error) => { signal_authority = false; cleanup_errors.push(error.to_string()); break; }
                }
            }
        }
    }
    if !child_reaped { cleanup_errors.push("owned direct writer retirement unresolved".into()); }
    // This is the original live BundleVolume/Worker, never an adopted path or saved PID.
    if let Err(error) = bundle.finish(writer, child_reaped, Instant::now() + Duration::from_secs(120)) {
        cleanup_errors.push(error.to_string());
    }
    let mut original_failure = work.err().map(|error| error.to_string());
    if original_failure.is_none() && child_exit.is_some_and(|status| !status.success()) {
        original_failure = Some("direct writer exited unsuccessfully after ENOSPC receipt".into());
    }
    use std::os::unix::process::ExitStatusExt;
    let exit_receipt = child_exit.map(|status| serde_json::json!({
        "success":status.success(), "code":status.code(), "signal":status.signal()}));
    let proof = serde_json::json!({"original_failure": original_failure, "child_reaped":child_reaped,
        "cleanup_errors":cleanup_errors, "signal_authority":signal_authority, "child_exit":exit_receipt, "elapsed_ms":started.elapsed().as_millis()});
    frontcap_core::state::save_atomic(&directory.join("driver-result.json"), &proof).unwrap();
    assert!(original_failure.is_none(), "original work failed: {proof}");
    assert!(cleanup_errors.is_empty(), "cleanup failed: {proof}");
    inspect_target(&root, &original).unwrap();
    assert_eq!(fs::read(directory.join("original-control")).unwrap(), b"preserve original receipt");
}

pub(crate) fn retain_owner(target: &Path) -> PathBuf {
    let config = fixture(target);
    let directory = target.join("retained-xctest");fs::create_dir(&directory).unwrap();
    let simulator: String = load(&target.join("simulator/simulator.json")).unwrap().unwrap();
    let (_input, _original) = acquired_launch(target, &directory, &config, &simulator, "owned.runner");
    let mut bundle = BundleVolume::prepare(config.clone(), &directory, target, &simulator, "owned.runner").unwrap();
    retain_direct_writer(&config, &directory, &mut bundle);
    directory
}


fn retain_direct_writer(config: &Config, directory: &Path, bundle: &mut BundleVolume) -> PathBuf {
    // Deliberately use the existing worker protocol stand-in; production start separately rejects
    // this same-filesystem mount. This test proves direct-writer ordering, not physical enforcement.
    bundle.creation_attempted = true;
    let worker = Worker::start(config, &directory.join("result-bundle-volume"), Instant::now()+Duration::from_secs(10)).unwrap();
    let metadata = fs::symlink_metadata(&worker.mount).unwrap();
    let result_bundle = worker.mount.join("test.xcresult");
    bundle.root = Some(Root { mount: worker.mount.clone(), result_bundle: result_bundle.clone(), device:metadata.dev(), inode:metadata.ino() });
    save_inherited(Class::Ordinary, &directory.join("result-bundle-root.json"), bundle.root.as_ref().unwrap()).unwrap();
    bundle.worker = Some(worker);
    bundle.writer_starting().unwrap();
    let mut child = Command::new("/usr/bin/python3").args(["-c", "import pathlib,sys; p=pathlib.Path(sys.argv[1]); p.mkdir(); (p/'original').write_bytes(b'original SDK bundle'); print('written',flush=True); sys.stdin.buffer.read(1)"])
        .arg(&result_bundle).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    use std::io::{BufRead, BufReader};
    assert_eq!(BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap(), "written");
    let writer = ProcessIdentity::of(child.id()).unwrap().unwrap();
    frontcap_core::state::save_atomic(&directory.join("runner-process.json"), &writer).unwrap();
    assert!(bundle.finish(Some(writer.clone()), false, Instant::now()+Duration::from_secs(5)).is_err());
    assert!(!directory.join("result-bundle-volume/worker/observed-retirement.json").exists());
    child.stdin.take().unwrap().write_all(b"1").unwrap();child.wait().unwrap();
    bundle.finish(Some(writer), true, Instant::now()+Duration::from_secs(30)).unwrap();
    result_bundle
}
